use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;
use tokio::sync::{mpsc, RwLock};
use tokio::time::{timeout, Duration};
use tracing::{debug, warn};

use crate::apps::{score_apps, AppCache};
use crate::index::TrigramIndex;
use crate::protocol::{IndexStats, ResultKind, SearchBatch, SearchRequest, SearchResult};

/// Spotlight fallback timeout. If `mdfind` hangs (corrupt index, heavy load),
/// we abort rather than block the search request indefinitely.
const SPOTLIGHT_TIMEOUT: Duration = Duration::from_millis(800);

/// Search pipeline that orchestrates all three phases.
pub struct SearchPipeline {
    index: Arc<RwLock<TrigramIndex>>,
    app_cache: Arc<RwLock<AppCache>>,
    scopes: Vec<PathBuf>,
}

impl SearchPipeline {
    pub fn new(
        index: Arc<RwLock<TrigramIndex>>,
        app_cache: Arc<RwLock<AppCache>>,
        scopes: Vec<PathBuf>,
    ) -> Self {
        SearchPipeline {
            index,
            app_cache,
            scopes,
        }
    }

    /// Access the underlying index (used by the file watcher and shutdown handler).
    pub fn index(&self) -> Arc<RwLock<TrigramIndex>> {
        self.index.clone()
    }

    /// Execute a search and return a channel receiver that yields result batches.
    pub async fn search(&self, req: SearchRequest) -> mpsc::Receiver<SearchBatch> {
        let (tx, rx) = mpsc::channel::<SearchBatch>(64);

        let query = req.query;
        let max_results = req.max_results;
        let id = req.id;
        let command = req.command;
        let kind_filter = req.kind_filter;
        let index = self.index.clone();
        let app_cache = self.app_cache.clone();
        let scopes: Vec<PathBuf> = if req.scopes.is_empty() {
            self.scopes.clone()
        } else {
            req.scopes.iter().map(PathBuf::from).collect()
        };

        tokio::spawn(async move {
            // ── index_content command ────────────────────────────────────────
            if command.as_deref() == Some("index_content") {
                let mut idx = index.write().await;
                for entry in req.content_batch {
                    idx.index_content(&entry.path, &entry.body);
                }
                let _ = tx.send(SearchBatch {
                    id,
                    results: vec![],
                    stats: None,
                    done: true,
                }).await;
                return;
            }
            // ─────────────────────────────────────────────────────────────────

            // ── Stats command ───────────────────────────────────────────────
            if query.trim().is_empty() && command.as_deref() == Some("stats") {
                let idx = index.read().await;
                let stats = IndexStats {
                    doc_count: idx.docs.len(),
                    trigram_count: idx.index.len(),
                    tombstone_count: idx.tombstone_count,
                    avg_doc_len: idx.avg_doc_len,
                };
                let _ = tx
                    .send(SearchBatch {
                        id: id.clone(),
                        results: vec![],
                        stats: Some(stats),
                        done: true,
                    })
                    .await;
                return;
            }
            // ─────────────────────────────────────────────────────────────────

            // ── List apps command ───────────────────────────────────────────
            if command.as_deref() == Some("list_apps") {
                let cache = app_cache.read().await;
                let mut apps: Vec<SearchResult> = cache
                    .entries
                    .iter()
                    .map(|app| crate::apps::app_to_result(app, 0.0))
                    .collect();
                apps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                apps.truncate(max_results);
                let _ = tx
                    .send(SearchBatch {
                        id: id.clone(),
                        results: apps,
                        stats: None,
                        done: true,
                    })
                    .await;
                return;
            }
            // ─────────────────────────────────────────────────────────────────

            // ── Search apps command (fast app-cache-only query) ─────────────
            if command.as_deref() == Some("search_apps") {
                let max_results = max_results.clamp(1, 500);
                let query = query.trim().to_string();
                if query.is_empty() || query.len() > 200 {
                    let _ = tx.send(SearchBatch { id, results: vec![], stats: None, done: true }).await;
                    return;
                }
                let cache = app_cache.read().await;
                let scored = crate::apps::score_apps(&cache, &query, max_results);
                let app_results: Vec<SearchResult> = scored
                    .iter()
                    .filter_map(|s| {
                        cache.entries.get(s.doc_id as usize).map(|app| {
                            crate::apps::app_to_result(app, s.score)
                        })
                    })
                    .collect();
                let _ = tx
                    .send(SearchBatch {
                        id: id.clone(),
                        results: app_results,
                        stats: None,
                        done: true,
                    })
                    .await;
                return;
            }
            // ─────────────────────────────────────────────────────────────────

            // ── Input validation ────────────────────────────────────────────
            let max_results = max_results.clamp(1, 500);
            let query = query.trim().to_string();
            if query.is_empty() || query.len() > 200 {
                let _ = tx.send(SearchBatch { id, results: vec![], stats: None, done: true }).await;
                return;
            }
            let scopes: Vec<PathBuf> = scopes.into_iter().take(10).collect();
            // ─────────────────────────────────────────────────────────────────

            let is_app_only = kind_filter.as_deref() == Some("application");
            let mut seen: HashSet<String> = HashSet::with_capacity(max_results);
            let mut total_yielded = 0usize;

            // ── Phase 3: App cache (sync, < 1ms) ────────────────────────────
            let apps = {
                let cache = app_cache.read().await;
                score_apps(&cache, &query, max_results)
            };

            if !apps.is_empty() {
                let cache = app_cache.read().await;
                let app_results: Vec<SearchResult> = apps
                    .iter()
                    .filter_map(|scored| {
                        cache.entries.get(scored.doc_id as usize).map(|app| {
                            crate::apps::app_to_result(app, scored.score)
                        })
                    })
                    .filter(|r| seen.insert(r.path.clone()))
                    .collect();

                if !app_results.is_empty() {
                    total_yielded += app_results.len();
                    let _ = tx
                        .send(SearchBatch {
                            id: id.clone(),
                            results: app_results,
                            stats: None,
                            done: false,
                        })
                        .await;
                }
            }

            // ── Phase 1: Trigram index ─────────────────────────────────────
            let index_results = {
                let idx = index.read().await;
                let fetch_max = if is_app_only { max_results * 3 } else { max_results };
                let scored = idx.query_with_tier(&query, fetch_max, req.fuzzy, req.tier_mask);
                scored
                    .into_iter()
                    .map(|s| idx.to_result(&s))
                    .filter(|r| {
                        if is_app_only {
                            return r.kind == ResultKind::Application;
                        }
                        true
                    })
                    .filter(|r| seen.insert(r.path.clone()))
                    .take(max_results)
                    .collect::<Vec<_>>()
            };

            let phase1_count = index_results.len();
            if !index_results.is_empty() {
                total_yielded += index_results.len();
                // Record access for top-3 results (frequency boost for future searches)
                let access_paths: Vec<String> = index_results.iter().take(3).map(|r| r.path.clone()).collect();
                let idx_for_access = index.clone();
                tokio::spawn(async move {
                    let mut idx = idx_for_access.write().await;
                    for path in &access_paths {
                        idx.record_access(path);
                    }
                });
                let _ = tx
                    .send(SearchBatch {
                        id: id.clone(),
                        results: index_results,
                        stats: None,
                        done: false,
                    })
                    .await;
            }

            // ── Phase 2: Spotlight fallback ────────────────────────────────
            if !is_app_only && phase1_count < 5 && total_yielded < max_results {
                debug!(
                    "Phase 1 returned only {} results; running Spotlight fallback",
                    phase1_count
                );

                let spotlight_result = timeout(
                    SPOTLIGHT_TIMEOUT,
                    async {
                        let mut spotlight = crate::spotlight::search(
                            &query,
                            &scopes,
                            max_results - total_yielded,
                        )
                        .await;
                        let mut results = Vec::new();
                        while let Some(result) = spotlight.next().await {
                            results.push(result);
                            if results.len() >= max_results - total_yielded {
                                break;
                            }
                        }
                        results
                    },
                )
                .await;

                match spotlight_result {
                    Ok(results) => {
                        let mut batch_buf = Vec::with_capacity(20);
                        for result in results {
                            if seen.insert(result.path.clone()) {
                                total_yielded += 1;
                                batch_buf.push(result);

                                if batch_buf.len() >= 20 {
                                    let _ = tx
                                        .send(SearchBatch {
                                            id: id.clone(),
                                            results: batch_buf.drain(..).collect(),
                                            stats: None,
                                            done: false,
                                        })
                                        .await;
                                }

                                if total_yielded >= max_results {
                                    break;
                                }
                            }
                        }

                        // Flush remaining
                        if !batch_buf.is_empty() {
                            let _ = tx
                                .send(SearchBatch {
                                    id: id.clone(),
                                    results: batch_buf,
                                    stats: None,
                                    done: false,
                                })
                                .await;
                        }
                    }
                    Err(_elapsed) => {
                        warn!(
                            "Spotlight fallback timed out after {}ms for query '{}'",
                            SPOTLIGHT_TIMEOUT.as_millis(),
                            query
                        );
                    }
                }
            }

            // ── Done ───────────────────────────────────────────────────────
            let _ = tx
                .send(SearchBatch {
                    id,
                    results: vec![],
                    stats: None,
                    done: true,
                })
                .await;
        });

        rx
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use tokio::sync::RwLock;
    use tokio::time::{timeout, Duration};

    use super::*;
    use crate::index::TrigramIndex;

    #[tokio::test]
    async fn test_stats_response_shape() {
        let dir = std::env::temp_dir().join("snyd_test_stats");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(dir.join(name), "test").unwrap();
        }

        let index = Arc::new(RwLock::new(TrigramIndex::build(&[dir.clone()])));
        let app_cache = Arc::new(RwLock::new(AppCache::load(&[])));
        let pipeline = SearchPipeline::new(index, app_cache, vec![dir]);

        let req = SearchRequest {
            id: "test-id".to_string(),
            query: "".to_string(),
            max_results: 10,
            scopes: vec![],
            command: Some("stats".to_string()),
            kind_filter: None,
            content_batch: vec![],
            fuzzy: true,
            tier_mask: 0b111,
        };

        let mut rx = pipeline.search(req).await;
        let batch = rx.recv().await.expect("should receive a batch");

        assert!(batch.done);
        assert!(batch.stats.is_some());
        let stats = batch.stats.unwrap();
        assert_eq!(stats.doc_count, 3);
        assert!(batch.results.is_empty());
    }

    #[tokio::test]
    async fn test_empty_query_returns_empty() {
        let dir = std::env::temp_dir().join("snyd_test_empty_query");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "test").unwrap();

        let index = Arc::new(RwLock::new(TrigramIndex::build(&[dir.clone()])));
        let app_cache = Arc::new(RwLock::new(AppCache::load(&[])));
        let pipeline = SearchPipeline::new(index, app_cache, vec![dir.clone()]);

        let req = SearchRequest {
            id: "t1".into(),
            query: "   ".into(), // whitespace only
            max_results: 10,
            scopes: vec![],
            command: None,
            kind_filter: None,
            content_batch: vec![],
            fuzzy: true,
            tier_mask: 0b111,
        };
        let mut rx = pipeline.search(req).await;
        let batch = rx.recv().await.expect("should receive done batch");
        assert!(batch.done);
        assert!(batch.results.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_kind_filter_application_excludes_files() {
        let dir = std::env::temp_dir().join("snyd_test_kind_filter");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("notes.txt"), "test").unwrap();

        let index = Arc::new(RwLock::new(TrigramIndex::build(&[dir.clone()])));
        let app_cache = Arc::new(RwLock::new(AppCache::load(&[])));
        let pipeline = SearchPipeline::new(index, app_cache, vec![dir.clone()]);

        let req = SearchRequest {
            id: "t2".into(),
            query: "notes".into(),
            max_results: 10,
            scopes: vec![],
            command: None,
            kind_filter: Some("application".into()),
            content_batch: vec![],
            fuzzy: true,
            tier_mask: 0b111,
        };
        let mut rx = pipeline.search(req).await;
        let mut all_results = vec![];
        while let Some(batch) = rx.recv().await {
            all_results.extend(batch.results);
            if batch.done {
                break;
            }
        }
        // notes.txt is a file, not an app — should be excluded by kind_filter
        assert!(all_results.iter().all(|r| r.kind == crate::protocol::ResultKind::Application));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_concurrent_reads_do_not_block() {
        let dir = std::env::temp_dir().join("snyd_test_concurrent_reads");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..50 {
            std::fs::write(dir.join(format!("file_{}.txt", i)), "content").unwrap();
        }

        let index = Arc::new(RwLock::new(TrigramIndex::build(&[dir.clone()])));
        let app_cache = Arc::new(RwLock::new(AppCache::load(&[])));
        let pipeline = Arc::new(SearchPipeline::new(index, app_cache, vec![dir.clone()]));

        let mut handles = Vec::new();
        for i in 0..20usize {
            let p = pipeline.clone();
            handles.push(tokio::spawn(async move {
                let req = SearchRequest {
                    id: format!("concurrent-{}", i),
                    query: format!("file_{}", i % 10),
                    max_results: 5,
                    scopes: vec![],
                    command: None,
                    kind_filter: None,
                    content_batch: vec![],
                    fuzzy: true,
                    tier_mask: 0b111,
                };
                let mut rx = p.search(req).await;
                let mut got_done = false;
                let result = timeout(Duration::from_secs(2), async {
                    while let Some(batch) = rx.recv().await {
                        if batch.done {
                            got_done = true;
                            break;
                        }
                    }
                })
                .await;
                assert!(result.is_ok(), "concurrent search {} timed out", i);
                assert!(got_done, "search {} never received done batch", i);
            }));
        }

        for h in handles {
            h.await.expect("task panicked");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_concurrent_reads_and_writes_no_deadlock() {
        let dir = std::env::temp_dir().join("snyd_test_rw_concurrent");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..20 {
            std::fs::write(dir.join(format!("doc_{}.txt", i)), "content").unwrap();
        }

        let index = Arc::new(RwLock::new(TrigramIndex::build(&[dir.clone()])));
        let app_cache = Arc::new(RwLock::new(AppCache::load(&[])));
        let pipeline = Arc::new(SearchPipeline::new(
            index.clone(),
            app_cache,
            vec![dir.clone()],
        ));

        let index_w = index.clone();
        let dir_w = dir.clone();
        let writer = tokio::spawn(async move {
            for i in 20..40usize {
                let path = dir_w.join(format!("doc_{}.txt", i));
                std::fs::write(&path, "new content").unwrap();
                {
                    let mut idx = index_w.write().await;
                    idx.add(&path);
                }
                tokio::task::yield_now().await;
                {
                    let mut idx = index_w.write().await;
                    idx.remove(&path);
                }
                tokio::task::yield_now().await;
            }
        });

        let mut readers = Vec::new();
        for i in 0..10usize {
            let p = pipeline.clone();
            readers.push(tokio::spawn(async move {
                for _ in 0..5 {
                    let req = SearchRequest {
                        id: format!("rw-reader-{}", i),
                        query: "doc".into(),
                        max_results: 10,
                        scopes: vec![],
                        command: None,
                        kind_filter: None,
                        content_batch: vec![],
                        fuzzy: true,
                        tier_mask: 0b111,
                    };
                    let mut rx = p.search(req).await;
                    let result = timeout(Duration::from_secs(3), async {
                        while let Some(batch) = rx.recv().await {
                            if batch.done {
                                break;
                            }
                        }
                    })
                    .await;
                    assert!(
                        result.is_ok(),
                        "reader {} timed out — possible deadlock",
                        i
                    );
                    tokio::task::yield_now().await;
                }
            }));
        }

        timeout(Duration::from_secs(10), writer)
            .await
            .expect("writer timed out")
            .expect("writer panicked");
        for r in readers {
            timeout(Duration::from_secs(10), r)
                .await
                .expect("reader timed out")
                .expect("reader panicked");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_rapid_remove_add_same_path_no_count_drift() {
        let dir = std::env::temp_dir().join("snyd_test_rapid_cycle");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        for i in 0..10 {
            std::fs::write(dir.join(format!("stable_{}.txt", i)), "content").unwrap();
        }
        let target = dir.join("target.txt");
        std::fs::write(&target, "content").unwrap();

        let index = Arc::new(RwLock::new(TrigramIndex::build(&[dir.clone()])));
        let initial_count = index.read().await.active_doc_count;
        assert_eq!(initial_count, 11); // 10 stable + 1 target

        for _ in 0..50 {
            {
                let mut idx = index.write().await;
                idx.remove(&target);
            }
            {
                let mut idx = index.write().await;
                idx.add(&target);
            }
        }

        let final_count = index.read().await.active_doc_count;
        assert_eq!(
            final_count, initial_count,
            "active_doc_count drifted: expected {}, got {}",
            initial_count, final_count
        );

        let avg = index.read().await.avg_doc_len;
        assert!(avg > 0.0, "avg_doc_len went non-positive: {}", avg);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_search_during_compact_no_panic() {
        let dir = std::env::temp_dir().join("snyd_test_compact_concurrent");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        for i in 0..1100 {
            std::fs::write(dir.join(format!("f_{}.txt", i)), "x").unwrap();
        }

        let index = Arc::new(RwLock::new(TrigramIndex::build(&[dir.clone()])));
        let app_cache = Arc::new(RwLock::new(AppCache::load(&[])));
        let pipeline = Arc::new(SearchPipeline::new(
            index.clone(),
            app_cache,
            vec![dir.clone()],
        ));

        let index_w = index.clone();
        let dir_w = dir.clone();
        let compactor = tokio::spawn(async move {
            let mut idx = index_w.write().await;
            for i in 0..1001usize {
                let path = dir_w.join(format!("f_{}.txt", i));
                idx.remove(&path);
            }
        });

        let searcher = tokio::spawn(async move {
            for _ in 0..10 {
                let req = SearchRequest {
                    id: "compact-search".into(),
                    query: "f_1".into(),
                    max_results: 5,
                    scopes: vec![],
                    command: None,
                    kind_filter: None,
                    content_batch: vec![],
                    fuzzy: true,
                    tier_mask: 0b111,
                };
                let mut rx = pipeline.search(req).await;
                let result = timeout(Duration::from_secs(5), async {
                    while let Some(batch) = rx.recv().await {
                        if batch.done {
                            break;
                        }
                    }
                })
                .await;
                assert!(result.is_ok(), "search timed out during compact");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });

        timeout(Duration::from_secs(30), compactor)
            .await
            .expect("compactor timed out")
            .expect("compactor panicked");
        timeout(Duration::from_secs(30), searcher)
            .await
            .expect("searcher timed out")
            .expect("searcher panicked");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
