//! # snyd — Fast Trigram File Search Daemon
//!
//! A high-performance file search engine built on a trigram inverted index
//! with real-time filesystem watching, fuzzy scoring, and macOS Spotlight
//! fallback.
//!
//! ## Quick Start
//!
//! ```bash
//! # Start the daemon
//! snyd
//!
//! # Search via Unix socket (JSON-RPC line protocol)
//! echo '{"id":"1","query":"budget","max_results":10}' | nc -U ~/Library/Caches/snyd/snyd.sock
//! ```
//!
//! ## Protocol
//!
//! snyd listens on a Unix domain socket and speaks a simple JSON-line protocol.
//! Each request is one JSON object terminated by `\n`. Responses are streamed
//! as one or more JSON lines; the final line always has `"done": true`.
//!
//! See [`protocol`](crate::protocol) for the full request/response types.

pub mod apps;
pub mod index;
pub mod kinds;
pub mod persist;
pub mod pipeline;
pub mod protocol;
pub mod spotlight;
pub mod watcher;

use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::apps::AppCache;
use crate::index::TrigramIndex;
use crate::pipeline::SearchPipeline;

/// Daemon configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Directories to index and watch.
    pub scopes: Vec<PathBuf>,
    /// Unix socket path to listen on.
    pub socket_path: PathBuf,
    /// Application bundle directories (for fast app cache).
    pub app_dirs: Vec<PathBuf>,
    /// Cache directory for the persisted index.
    pub cache_dir: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        let cache = dirs::cache_dir()
            .unwrap_or_else(|| home.join(".cache"))
            .join("snyd");
        let home_scopes = vec![
            home.join("Desktop"),
            home.join("Documents"),
            home.join("Downloads"),
        ];
        Self {
            scopes: home_scopes,
            socket_path: cache.join("snyd.sock"),
            app_dirs: vec![],
            cache_dir: cache,
        }
    }
}

/// Shared daemon state passed to connection handlers.
pub struct DaemonState {
    pub pipeline: Arc<SearchPipeline>,
    pub scopes: Vec<PathBuf>,
}

/// Build the index (from cache or fresh) and app cache, then return the
/// [`DaemonState`] needed by the connection handler.
pub async fn build_state(config: &Config) -> DaemonState {
    use tracing::{info, warn};

    let index: Arc<RwLock<TrigramIndex>> = match persist::load(&config.scopes, &config.cache_dir) {
        Some(cached) => {
            info!(
                "Loaded index from cache: {} docs, {} trigrams",
                cached.docs.len(),
                cached.index.len()
            );
            Arc::new(RwLock::new(cached))
        }
        None => {
            let start = std::time::Instant::now();
            info!("Building trigram index for {} scopes…", config.scopes.len());
            let idx = TrigramIndex::build(&config.scopes);
            let build_time = start.elapsed();
            info!(
                "Index built in {:?}: {} docs, {} unique trigrams",
                build_time,
                idx.docs.len(),
                idx.index.len()
            );
            if let Err(e) = persist::save(&idx, &config.scopes, &config.cache_dir) {
                warn!("Failed to save index cache: {}", e);
            }
            Arc::new(RwLock::new(idx))
        }
    };

    let app_cache = Arc::new(RwLock::new(AppCache::load(&config.app_dirs)));

    let pipeline = Arc::new(SearchPipeline::new(
        index.clone(),
        app_cache.clone(),
        config.scopes.clone(),
    ));

    DaemonState {
        pipeline,
        scopes: config.scopes.clone(),
    }
}
