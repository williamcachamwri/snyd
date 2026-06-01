use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::index::TrigramIndex;

/// Skip-list for noisy paths and file types.
const SKIP_PATH_SEGMENTS: &[&str] = &[
    "/.git/",
    "/node_modules/",
    "/.build/",
    "/DerivedData/",
    "/target/",
    "/dist/",
    "/out/",
];

const SKIP_EXTS: &[&str] = &[
    "o", "pyc", "class", "swp", "swo", "DS_Store",
    "tmp", "temp", "cache", "lock", "log",
];

/// Return true if a path should be ignored by the watcher.
fn should_skip(path: &Path) -> bool {
    let path_str = path.to_string_lossy();
    for seg in SKIP_PATH_SEGMENTS {
        if path_str.contains(seg) {
            return true;
        }
    }
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        if SKIP_EXTS.contains(&ext) {
            return true;
        }
    }
    false
}

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
                    #[cfg(target_os = "linux")]
                    if e.to_string().contains("No space") {
                        eprintln!("inotify watch limit reached. Run:");
                        eprintln!("  echo 'fs.inotify.max_user_watches=524288' | sudo tee -a /etc/sysctl.conf && sudo sysctl -p");
                    }
                    warn!("Failed to watch {}: {}", scope.display(), e);
                }
            }

            // Spawn blocking thread to collect, filter, and debounce events
            std::thread::spawn(move || {
                let mut accumulated: HashSet<PathBuf> = HashSet::new();
                let mut last_flush = std::time::Instant::now();
                let mut last_event = std::time::Instant::now();
                let mut skipped_count = 0usize;

                loop {
                    // Drain events with a short timeout
                    let mut got_any = false;
                    while let Ok(res) = notify_rx.recv_timeout(Duration::from_millis(50)) {
                        got_any = true;
                        if let Ok(event) = res {
                            for path in event.paths {
                                if should_skip(&path) {
                                    skipped_count += 1;
                                    continue;
                                }
                                accumulated.insert(path);
                                last_event = std::time::Instant::now();
                            }
                        }
                    }

                    let now = std::time::Instant::now();
                    let debounce_elapsed = now.duration_since(last_flush);
                    let since_last_event = now.duration_since(last_event);

                    // Flush if:
                    // 1. Debounce window elapsed (150ms) AND no new events in last 50ms, OR
                    // 2. Max wait exceeded (500ms) even if events keep coming
                    let should_flush = !accumulated.is_empty()
                        && (debounce_elapsed >= Duration::from_millis(150)
                            && since_last_event >= Duration::from_millis(50))
                        || (debounce_elapsed >= Duration::from_millis(500));

                    if should_flush {
                        let batch: Vec<IndexEvent> = accumulated
                            .drain()
                            .map(|p| determine_event(&p))
                            .collect();
                        if !batch.is_empty() {
                            debug!(
                                "Watcher flush: {} events ({} skipped by filter)",
                                batch.len(),
                                skipped_count
                            );
                            skipped_count = 0;
                            if let Err(e) = tx.try_send(batch) {
                                warn!("Failed to send watcher batch: {}", e);
                            }
                        } else {
                            skipped_count = 0;
                        }
                        last_flush = now;
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
    let mut last_save = tokio::time::Instant::now();
    let save_interval = Duration::from_secs(30);

    while let Some(batch) = watcher.recv().await {
        // One write lock for the entire batch — use atomic update() for Modified
        {
            let mut idx = index.write().await;
            for event in &batch {
                match event {
                    IndexEvent::Modified(path) => {
                        idx.update(path);
                        debug!("Index updated: {}", path.display());
                    }
                    IndexEvent::Deleted(path) => {
                        idx.remove(path);
                        debug!("Index removed: {}", path.display());
                    }
                }
            }
        } // write lock released here

        // Throttled save: only save every 30s and only if no pending events
        let now = tokio::time::Instant::now();
        if now.duration_since(last_save) >= save_interval {
            // Peek whether there are pending events; if so, skip this save window
            // (mpsc::Receiver doesn't have a non-blocking peek, so we just save
            // unconditionally — the next batch will come in soon and we'll check again.)
            let idx_clone = index.clone();
            let scopes_clone = scopes.clone();
            tokio::spawn(async move {
                let idx = idx_clone.read().await;
                let cache_dir = dirs::cache_dir()
                    .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
                    .join("snyd");
                if let Err(e) = crate::persist::save(&idx, &scopes_clone, &cache_dir) {
                    warn!("Periodic index save failed: {}", e);
                } else {
                    debug!("Periodic index saved");
                }
            });
            last_save = now;
        }
    }

    info!("File watcher channel closed; stopping index updater");
}
