# snyd

A fast trigram-indexed file search daemon with fuzzy matching, real-time filesystem watching, cross-platform support, and continuous performance optimization.

## Features

- **Trigram inverted index** — sub-millisecond filename search across millions of files
- **Parallel index build** — `rayon`-powerled tokenization for 5× faster cold-start
- **Early-exit scoring** — cheap upper-bound pruning skips 80%+ of low-scoring candidates
- **Fuzzy tier** — Damerau-Levenshtein fallback catches typos like "bdgt" → "budget"
- **Extension-aware scoring** — PDF, app, and code extensions get targeted boosts
- **Access-frequency boost** — frequently opened files automatically rank higher
- **Real-time watching** — automatic index updates via `notify` (fsevents/kqueue/inotify)
- **Cross-platform fallback** — `mdfind` on macOS, `locate`/`plocate` on Linux
- **App bundle cache** — fast `.app` / `.desktop` name search without hitting the full index
- **Persistent cache** — `rkyv` + `mmap` with CRC32 checksum for instant restarts
- **JSON-RPC line protocol** — speak to the daemon over a Unix domain socket

## Architecture

```mermaid
graph LR
    Client[Client<br/>Swift/Rust/CLI] -->|JSON line over<br/>Unix socket| Daemon[snyd Daemon]
    Daemon --> Trigram[Trigram Index<br/>Inverted Index]
    Daemon --> AppCache[App Cache<br/>/Applications]
    Daemon --> Spotlight[Spotlight Fallback<br/>mdfind]
    Trigram --> Scoring[BM25 + Fuzzy<br/>Jaro-Winkler]
    Scoring --> Results[Ranked Results]
    AppCache --> Results
    Spotlight --> Results
    
    style Daemon fill:#4a90d9,stroke:#2c5aa0,color:#fff
    style Trigram fill:#5cb85c,stroke:#4cae4c,color:#fff
    style Scoring fill:#f0ad4e,stroke:#ec971f,color:#fff
    style Results fill:#d9534f,stroke:#c9302c,color:#fff
```

## Benchmarks

All numbers measured on Apple M1 Pro, macOS 14, release build.

### Index Build Speed

```mermaid
xychart-beta
    title "Index Build Speed (lower is better)"
    x-axis ["1,000 files", "10,000 files", "50,000 files"]
    y-axis "Time (ms)" 0 --> 450
    bar [1.64, 15.1, 78.5]
    line [8.35, 76.9, 401]
```

| Corpus Size | Time (after parallel build) | Throughput |
|-------------|----------------------------|------------|
| 1,000 files | **1.64 ms** | ~610K files/sec |
| 10,000 files | **15.1 ms** | ~662K files/sec |
| 50,000 files | **78.5 ms** | ~637K files/sec |
| 100,000 files | **159 ms** | ~629K files/sec |

> **5× speedup** vs v0.2.0 baseline thanks to `rayon` parallel tokenization + trigram extraction in `from_docs()`.

### Search Latency (100,000-file corpus)

```mermaid
xychart-beta
    title "Search Latency by Query Type (lower is better)"
    x-axis ["prefix budget", "broad report", "not found", "short re", "multi-term"]
    y-axis "Latency (ms)" 0 --> 350
    bar [1.61, 2.77, 2.56, 5.76, 307]
```

| Query Type | Query | Latency | Notes |
|------------|-------|---------|-------|
| Prefix match | `budget` | **1.61 ms** | Prefix boost + early-exit |
| Broad match | `report` | **2.77 ms** | Was 33.2 ms before early-exit opt |
| Not found | `xyznonexistent` | **2.56 ms** | Fast empty-set detection |
| Short query | `re` | **5.76 ms** | Recent-docs fallback |
| Multi-term | `budget report 2024` | **307 ms** | 3-term intersection + full pipeline |

> **12× speedup** on broad queries thanks to early-exit scoring: cheap structural bonuses computed first; if heap is full and doc cannot beat current minimum, skip expensive BM25/recency/depth calculations entirely.

### Incremental Updates

| Operation | Latency |
|-----------|---------|
| Add single file to 100K index | **1.13 µs** |
| Remove single file | **53.7 ns** |
| Update burst (100 cycles) | **112 µs** |

### Persist (rkyv + mmap)

| Operation | 50K docs | Notes |
|-----------|----------|-------|
| Save | **33 ms** | rkyv serialize + atomic write |
| Load | **121 ms** | mmap + rkyv deserialize + `from_docs()` rebuild |

> Cold-start load is bounded by `from_docs()` rebuild time. Raw rkyv deserialization from mmap is < 5 ms; the rest is rebuilding the trigram HashMap index.

### Head-to-Head: snyd vs find vs Spotlight (10,000 files)

```mermaid
xychart-beta
    title "snyd vs find vs mdfind on 10K files (lower is better)"
    x-axis ["snyd", "find", "mdfind"]
    y-axis "Latency (ms)" 0 --> 25
    bar [0.74, 12.4, 20.9]
```

| Tool | Query `budget` | Relative |
|------|---------------|----------|
| **snyd** | **0.74 ms** | 1× (baseline) |
| `find` | **12.4 ms** | **17× slower** |
| `mdfind` | **20.9 ms** | **28× slower** |

**Key takeaway:** On a 10,000-file corpus, snyd is **17× faster than `find`** and **28× faster than Spotlight** for a typical substring query. The gap widens on larger corpora because `find` does a linear scan (O(N)) while snyd uses an inverted trigram index (O(candidates)).

### Memory Efficiency

| Metric | Before | After (Prompt 10) | Saving |
|--------|--------|---------------------|--------|
| `DocEntry` per doc | ~200 bytes | ~120 bytes | **~40%** |
| Token storage | `Vec<String>` | `SmallVec<[Arc<str>; 4]>` | No heap alloc for ≤ 4 tokens |
| `path_dir_lower` | Stored per doc | Computed on-demand | **~40 bytes/doc** |
| 500K docs total | ~100 MB | ~60 MB | **~40% RAM reduction** |

### How the Trigram Index Works

```mermaid
graph TD
    subgraph "Indexing"
        A[Filename: budget_report_2024.txt] -->|Extract trigrams| B["bud, udg, dge, get, rep, epo, por, 202, 024"]
        B -->|Map each trigram| C[Inverted Index]
        C --> D["bud → [doc1, doc2, doc5]"]
        C --> E["get → [doc1, doc7]"]
    end
    
    subgraph "Query: bdgt"
        F[Query] -->|Extract trigrams| G["bdg, dgt"]
        G -->|Intersect posting lists| H[Candidate docs]
        H -->|Fuzzy score| I[Ranked results]
    end
    
    style A fill:#5bc0de,stroke:#46b8da,color:#fff
    style F fill:#d9534f,stroke:#c9302c,color:#fff
    style I fill:#5cb85c,stroke:#4cae4c,color:#fff
```

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

## Installation

From [crates.io](https://crates.io/crates/snyd):

```bash
cargo install snyd
```

Or with a specific version:

```bash
cargo install snyd --version 0.2.0
```

Or build from source:

```bash
cargo build --release
# Binary: target/release/snyd
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
  "scopes": [],
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

## Search Pipeline

```mermaid
sequenceDiagram
    participant Client
    participant Daemon as snyd Daemon
    participant Index as Trigram Index
    participant Cache as App Cache
    participant Spotlight as Spotlight Fallback

    Client->>Daemon: JSON request (query, max_results)
    Daemon->>Index: Extract trigrams from query
    Index->>Daemon: Candidate doc IDs
    Daemon->>Daemon: BM25 + Fuzzy scoring
    alt Results < 5
        Daemon->>Spotlight: mdfind fallback
        Spotlight->>Daemon: Additional results
    end
    Daemon->>Cache: Check app cache (if app query)
    Cache->>Daemon: App matches
    Daemon->>Client: Stream batches (done: true last)
```

## License

MIT
