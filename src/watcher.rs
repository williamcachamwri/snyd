use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

use crate::index::TrigramIndex;

/// Events that can mutate the index.
///
/// Event mapping from the underlying filesystem watcher (notify/FSEvents):
/// - `Modified`  → path exists: re-index (handles both new files and modifications)
/// - `Deleted`   → path gone: remove from index
#[derive(Debug, Clone)]
pub enum IndexEvent {
    Deleted(PathBuf),
    Modified(PathBuf),
}

/// Watch configured scopes and stream incremental index events.
pub struct IndexWatcher {
    _watcher: RecommendedWatcher,
    event_rx: mpsc::Receiver<Vec<IndexEvent>>,
}

impl IndexWatcher {
    /// Start watching all scopes. Events are debounced and sent as batches.
    ///
    /// If the underlying watcher cannot be created, the daemon logs an error
    /// and continues without live file-system updates.
    pub fn new(scopes: Vec<PathBuf>) -> Self {
        let (tx, rx) = mpsc::channel(128);

        let watcher = {
            let (notify_tx, notify_rx) =
                std::sync::mpsc::channel::<Result<Event, notify::Error>>();

            let mut watcher = match RecommendedWatcher::new(
                move |res: Result<Event, notify::Error>| {
                    let _ = notify_tx.send(res);
                },
                notify::Config::default(),
            ) {
                Ok(w) => w,
                Err(e) => {
                    error!("Failed to create file watcher: {}. Running without live updates.", e);
                    // A no-op watcher with an empty closure should always succeed on any
                    // platform that supports notify. If it somehow fails, the daemon cannot
                    // watch files — panic here is acceptable since it indicates a
                    // system-level misconfiguration.
                    return IndexWatcher {
                        _watcher: RecommendedWatcher::new(
                            move |_res: Result<Event, notify::Error>| {},
                            notify::Config::default(),
                        )
                        .expect("no-op watcher creation failed — system-level error"),
                        event_rx: rx,
                    };
                }
            };

            for scope in &scopes {
                if let Err(e) = watcher.watch(scope.as_path(), RecursiveMode::Recursive) {
                    warn!("Failed to watch {}: {}", scope.display(), e);
                }
            }

            // Spawn blocking thread to collect and debounce events
            std::thread::spawn(move || {
                let mut accumulated: HashSet<PathBuf> = HashSet::new();
                let mut last_flush = std::time::Instant::now();

                loop {
                    // Drain events with a short timeout
                    let mut got_any = false;
                    while let Ok(res) = notify_rx.recv_timeout(Duration::from_millis(50)) {
                        got_any = true;
                        if let Ok(event) = res {
                            for path in event.paths {
                                accumulated.insert(path);
                            }
                        }
                    }

                    // Flush if debounce window elapsed
                    if !accumulated.is_empty()
                        && last_flush.elapsed() >= Duration::from_millis(200)
                    {
                        let batch: Vec<IndexEvent> = accumulated
                            .drain()
                            .map(|p| determine_event(&p))
                            .collect();
                        if let Err(e) = tx.try_send(batch) {
                            warn!("Failed to send watcher batch: {}", e);
                        }
                        last_flush = std::time::Instant::now();
                    }

                    if !got_any && accumulated.is_empty() {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            });

            watcher
        };

        IndexWatcher {
            _watcher: watcher,
            event_rx: rx,
        }
    }

    /// Receive the next batch of index events (debounced).
    pub async fn recv(&mut self) -> Option<Vec<IndexEvent>> {
        self.event_rx.recv().await
    }
}

fn determine_event(path: &Path) -> IndexEvent {
    if path.exists() {
        IndexEvent::Modified(path.to_path_buf())
    } else {
        IndexEvent::Deleted(path.to_path_buf())
    }
}

/// Background task that applies watcher events to the index.
///
/// All events in a single batch are applied under **one** write lock so that
/// search requests are not blocked by a rapid stream of individual events.
pub async fn run_index_updater(
    mut watcher: IndexWatcher,
    index: std::sync::Arc<tokio::sync::RwLock<TrigramIndex>>,
    scopes: Vec<PathBuf>,
) {
    let mut update_count = 0u32;
    while let Some(batch) = watcher.recv().await {
        // One write lock for the entire batch
        {
            let mut idx = index.write().await;
            for event in &batch {
                match event {
                    IndexEvent::Modified(path) => {
                        if path.exists() {
                            idx.remove(path);
                            idx.add(path);
                            debug!("Index updated: {}", path.display());
                        } else {
                            idx.remove(path);
                        }
                    }
                    IndexEvent::Deleted(path) => {
                        idx.remove(path);
                        debug!("Index removed: {}", path.display());
                    }
                }
            }
        } // write lock released here

        update_count += batch.len() as u32;
        // Periodic save (every ~500 file events, not 500 batches)
        if update_count % 500 < batch.len() as u32 {
            let idx_clone = index.clone();
            let scopes_clone = scopes.clone();
            tokio::spawn(async move {
                let idx = idx_clone.read().await;
                let cache_dir = dirs::cache_dir()
                    .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
                    .join("snyd");
                if let Err(e) = crate::persist::save(&idx, &scopes_clone, &cache_dir) {
                    warn!("Periodic index save failed: {}", e);
                }
            });
        }
    }
}
