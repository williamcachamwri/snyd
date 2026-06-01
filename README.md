# snyd

A fast trigram-indexed file search daemon with fuzzy matching, real-time filesystem watching, and macOS Spotlight fallback.

## Features

- **Trigram inverted index** — sub-millisecond filename search across millions of files
- **Fuzzy scoring** — typo-tolerant matching (Jaro-Winkler + token overlap)
- **Real-time watching** — automatic index updates via `notify` (fsevents/kqueue/inotify)
- **macOS Spotlight fallback** — `mdfind` integration when the trigram index is sparse
- **App bundle cache** — fast `.app` name search without hitting the full index
- **Persistent cache** — bincode-encoded index with 24-hour TTL for instant restarts
- **JSON-RPC line protocol** — speak to the daemon over a Unix domain socket

## Quick Start

```bash
# Start the daemon (indexes ~/Desktop, ~/Documents, ~/Downloads by default)
snyd

# Search via Unix socket
echo '{"id":"1","query":"budget","max_results":10}' | nc -U ~/.cache/snyd/snyd.sock

# Index custom directories
snyd -d /Applications -d /Users/wica/Projects

# Use a custom socket
snyd -s /tmp/my-snyd.sock -d /data
```

## Protocol

snyd listens on a Unix domain socket and speaks a simple JSON-line protocol.
Each request is one JSON object terminated by `\n`. Responses are streamed
as one or more JSON lines; the final line always has `"done": true`.

### Request

```json
{
  "id": "request-1",
  "query": "xcode",
  "max_results": 10,
  "scopes": ["/Users/wica"],
  "command": null,
  "kind_filter": null,
  "content_batch": []
}
```

### Response (streaming)

```json
{"id":"request-1","results":[{"path":"/Applications/Xcode.app","name":"Xcode","kind":"application","size":0,"modified":0,"score":120.0}],"done":false}
{"id":"request-1","results":[],"done":true}
```

### Commands

| Command | Description |
|---------|-------------|
| `null` (default) | Full file search |
| `list_apps` | List all applications (empty query) |
| `search_apps` | Search application names |
| `index_content` | Push body text into the trigram index |
| `stats` | Return index statistics |

## Installation

```bash
cargo install --path .
```

Or build from source:

```bash
cargo build --release
# Binary: target/release/snyd
```

## Configuration

All options can be set via CLI flags or environment variables:

| Flag | Env Var | Default |
|------|---------|---------|
| `-s, --socket` | `SNYD_SOCKET` | `~/.cache/snyd/snyd.sock` |
| `-d, --scopes` | `SNYD_SCOPES` | `~/Desktop:~/Documents:~/Downloads` |
| `--app-dirs` | `SNYD_APP_DIRS` | (none) |
| `-c, --cache` | `SNYD_CACHE` | `~/.cache/snyd` |
| `--log-level` | `SNYD_LOG` | `info` |

## Library API

```rust
use snyd::{build_state, Config};

#[tokio::main]
async fn main() {
    let config = Config {
        scopes: vec!["/Users/wica".into()],
        socket_path: "/tmp/snyd.sock".into(),
        app_dirs: vec!["/Applications".into()],
        cache_dir: "/tmp/snyd-cache".into(),
    };

    let state = build_state(&config).await;
    // state.pipeline.search(req).await ...
}
```

## License

MIT
