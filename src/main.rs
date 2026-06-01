use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal;
use tracing::{error, info, warn};

use snyd::protocol::SearchRequest;
use snyd::watcher::{run_index_updater, IndexWatcher};
use snyd::{build_state, Config};

#[derive(Parser, Debug)]
#[command(name = "snyd")]
#[command(about = "Fast trigram-indexed file search daemon")]
#[command(version)]
struct Cli {
    /// Unix socket path to listen on
    #[arg(short, long, env = "SNYD_SOCKET")]
    socket: Option<PathBuf>,

    /// Directories to index (can be given multiple times)
    #[arg(short = 'd', long, env = "SNYD_SCOPES", value_delimiter = ':')]
    scopes: Vec<PathBuf>,

    /// macOS application directories to cache (e.g. /Applications)
    #[arg(long, env = "SNYD_APP_DIRS", value_delimiter = ':')]
    app_dirs: Vec<PathBuf>,

    /// Cache directory for the persisted index
    #[arg(short, long, env = "SNYD_CACHE")]
    cache: Option<PathBuf>,

    /// Log level filter (e.g. info, debug, warn)
    #[arg(long, default_value = "info", env = "SNYD_LOG")]
    log_level: String,
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(&cli.log_level)
        .init();

    info!("snyd starting up…");

    let mut config = Config::default();

    if let Some(socket) = cli.socket {
        config.socket_path = socket;
    }
    if !cli.scopes.is_empty() {
        config.scopes = cli.scopes;
    }
    if !cli.app_dirs.is_empty() {
        config.app_dirs = cli.app_dirs;
    }
    if let Some(cache) = cli.cache {
        config.cache_dir = cache;
    }

    let socket = &config.socket_path;

    // Single-instance guard
    if socket.exists() {
        let probe = std::os::unix::net::UnixStream::connect(socket);
        if probe.is_ok() {
            info!("Another snyd instance is already running — exiting.");
            return Ok(());
        }
        let _ = tokio::fs::remove_file(socket).await;
    }

    // Ensure cache dir exists
    if let Some(parent) = config.cache_dir.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }

    // Build index + pipeline
    let state = build_state(&config).await;

    // Start file watcher
    let index = state.pipeline.index();
    let watcher = IndexWatcher::new(state.scopes.clone());
    tokio::spawn(run_index_updater(watcher, index.clone(), state.scopes.clone()));
    info!("File watcher started");

    // Bind Unix socket
    let listener = match UnixListener::bind(socket) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            let probe = std::os::unix::net::UnixStream::connect(socket);
            if probe.is_ok() {
                info!("Lost startup race — exiting.");
                return Ok(());
            }
            let _ = tokio::fs::remove_file(socket).await;
            UnixListener::bind(socket)?
        }
        Err(e) => return Err(e),
    };
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    info!("Listening on {} (mode 0o600)", socket.display());

    // Graceful shutdown
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let shutdown_clone = shutdown.clone();
    tokio::spawn(async move {
        let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("register SIGTERM");
        let mut sigint = signal::unix::signal(signal::unix::SignalKind::interrupt())
            .expect("register SIGINT");

        tokio::select! {
            _ = sigterm.recv() => info!("SIGTERM received"),
            _ = sigint.recv() => info!("SIGINT received"),
        }

        shutdown_clone.notify_waiters();
    });

    // Accept loop
    let mut active_connections = tokio::task::JoinSet::new();

    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, _addr)) => {
                        let pipeline = state.pipeline.clone();
                        active_connections.spawn(handle_connection(stream, pipeline));
                    }
                    Err(e) => {
                        warn!("Accept error: {}", e);
                    }
                }
            }

            _ = shutdown.notified() => {
                info!("Shutting down: draining {} active connections…", active_connections.len());
                drop(listener);
                while active_connections.join_next().await.is_some() {}
                {
                    let idx = index.read().await;
                    if let Err(e) = snyd::persist::save(&idx, &state.scopes, &config.cache_dir) {
                        warn!("Failed to save index cache on shutdown: {}", e);
                    }
                }
                info!("snyd exited cleanly");
                return Ok(());
            }
        }
    }
}

/// Handle a persistent Unix socket connection.
///
/// Protocol: each request is one JSON line ending in `\n`. The daemon responds
/// with one or more JSON lines (each a `SearchBatch`). The final batch in a
/// response always has `done: true`. The connection stays open so the client can
/// reuse the socket across queries.
async fn handle_connection(
    stream: UnixStream,
    pipeline: Arc<snyd::pipeline::SearchPipeline>,
) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    loop {
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) => return, // EOF
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let req: SearchRequest = match serde_json::from_str(trimmed) {
                    Ok(r) => r,
                    Err(e) => {
                        warn!("Invalid JSON request: {}", e);
                        let err = b"{\"error\":\"invalid request\"}\n";
                        if write_half.write_all(err).await.is_err() {
                            return;
                        }
                        continue;
                    }
                };

                let mut rx = pipeline.search(req).await;
                while let Some(batch) = rx.recv().await {
                    let json = match serde_json::to_string(&batch) {
                        Ok(j) => j,
                        Err(e) => {
                            error!("Failed to serialize batch: {}", e);
                            break;
                        }
                    };
                    if write_half.write_all(json.as_bytes()).await.is_err() {
                        return;
                    }
                    if write_half.write_all(b"\n").await.is_err() {
                        return;
                    }
                }
            }
            Err(e) => {
                warn!("Read error: {}", e);
                return;
            }
        }
    }
}
