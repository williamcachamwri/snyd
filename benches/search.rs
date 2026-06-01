use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::runtime::Runtime;
use tokio::sync::RwLock;

use snyd::index::{DocEntry, TrigramIndex};
use snyd::apps::AppCache;
use snyd::pipeline::SearchPipeline;
use snyd::protocol::SearchRequest;

/// Generate a realistic mixed corpus of N filenames.
fn generate_corpus(n: usize) -> Vec<(String, String, u64)> {
    let mut out = Vec::with_capacity(n);
    let names = [
        ("budget", "report", "xlsx"),
        ("meeting", "notes", "docx"),
        ("MyReact", "Component", "tsx"),
        ("User", "Controller", "java"),
        ("image", "001", "png"),
        ("photo", "20240315", "jpg"),
        ("README", "", "md"),
        ("package", "", "json"),
        ("Cargo", "", "toml"),
        ("main", "", "rs"),
        ("index", "", "html"),
        ("styles", "", "css"),
        ("app", "", "swift"),
        ("server", "config", "yaml"),
        ("test", "spec", "js"),
    ];
    for i in 0..n {
        let (a, b, ext) = names[i % names.len()];
        let name = if b.is_empty() {
            format!("{}_{:05}.{}", a, i, ext)
        } else {
            format!("{}_{}_{:05}.{}", a, b, i, ext)
        };
        let path = format!("/tmp/snyd_bench/{}/{}", i % 100, name);
        out.push((path, name, i as u64));
    }
    out
}

fn build_index_from_corpus(corpus: &[(String, String, u64)]) -> TrigramIndex {
    let docs: Vec<DocEntry> = corpus
        .iter()
        .map(|(path, name, mtime)| {
            let extension = std::path::Path::new(path)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            DocEntry {
                path: path.clone(),
                name_lower: name.to_lowercase(),
                acronym: String::new(),
                tokens: smallvec::SmallVec::new(),
                body_lower: String::new(),
                body_tokens: smallvec::SmallVec::new(),
                kind: snyd::protocol::ResultKind::File,
                mtime: *mtime,
                size: 1024,
                deleted: false,
                extension,
                access_count: 0,
                last_accessed: 0,
            }
        })
        .collect();
    TrigramIndex::from_docs(docs)
}

// ═════════════════════════════════════════════════════════════════════════════
// 1. INDEX BUILD
// ═════════════════════════════════════════════════════════════════════════════

fn bench_index_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("index_build");
    for size in [1_000, 10_000, 50_000] {
        let corpus = generate_corpus(size);
        group.bench_with_input(BenchmarkId::new("files", size), &corpus, |b, corpus| {
            b.iter(|| {
                let _ = build_index_from_corpus(black_box(corpus));
            });
        });
    }
    group.finish();
}

fn bench_from_docs(c: &mut Criterion) {
    let mut group = c.benchmark_group("from_docs");
    for size in [10_000, 50_000, 100_000] {
        let corpus = generate_corpus(size);
        group.bench_with_input(BenchmarkId::new("docs", size), &corpus, |b, corpus| {
            b.iter(|| {
                let _ = build_index_from_corpus(black_box(corpus));
            });
        });
    }
    group.finish();
}

// ═════════════════════════════════════════════════════════════════════════════
// 2. QUERY LATENCY
// ═════════════════════════════════════════════════════════════════════════════

fn bench_query_latency(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let corpus = generate_corpus(100_000);
    let index = Arc::new(RwLock::new(build_index_from_corpus(&corpus)));
    let app_cache = Arc::new(RwLock::new(AppCache::load(&[])));
    let pipeline = Arc::new(SearchPipeline::new(index, app_cache, vec![]));

    let mut group = c.benchmark_group("query_latency");
    group.sample_size(50);

    for (label, query) in [
        ("exact", "budget_report_00042.xlsx"),
        ("prefix", "budget"),
        ("broad", "report"),
        ("fuzzy_typo", "bdgt"),
        ("short", "re"),
        ("multi", "budget report 2024"),
        ("not_found", "xyznonexistent"),
    ] {
        let req = SearchRequest {
            id: "bench".to_string(),
            query: query.to_string(),
            max_results: 20,
            scopes: vec![],
            command: None,
            kind_filter: None,
            content_batch: vec![],
            fuzzy: true,
        };
        group.bench_with_input(BenchmarkId::new(label, query), &req, |b, req| {
            b.to_async(&rt).iter(|| async {
                let mut rx = pipeline.search(req.clone()).await;
                while let Some(_batch) = rx.recv().await {}
            });
        });
    }
    group.finish();
}

// ═════════════════════════════════════════════════════════════════════════════
// 3. INCREMENTAL UPDATE
// ═════════════════════════════════════════════════════════════════════════════

fn bench_incremental_update(c: &mut Criterion) {
    let corpus = generate_corpus(100_000);
    let mut group = c.benchmark_group("incremental");

    // Add single file
    group.bench_function("add_one", |b| {
        let mut idx = build_index_from_corpus(&corpus);
        let path = PathBuf::from("/tmp/snyd_bench/new_file.txt");
        std::fs::write(&path, "test").ok();
        b.iter(|| {
            idx.add(black_box(&path));
        });
        let _ = std::fs::remove_file(&path);
    });

    // Remove single file
    group.bench_function("remove_one", |b| {
        let mut idx = build_index_from_corpus(&corpus);
        let path = PathBuf::from("/tmp/snyd_bench/0/budget_00000.xlsx");
        b.iter(|| {
            idx.remove(black_box(&path));
        });
    });

    // Update burst (100 cycles)
    group.bench_function("update_burst_100", |b| {
        let mut idx = build_index_from_corpus(&corpus);
        let paths: Vec<PathBuf> = (0..100)
            .map(|i| PathBuf::from(format!("/tmp/snyd_bench/{}/burst_{}.txt", i % 100, i)))
            .collect();
        for p in &paths {
            std::fs::write(p, "test").ok();
        }
        b.iter(|| {
            for p in &paths {
                idx.update(black_box(p));
            }
        });
        for p in &paths {
            let _ = std::fs::remove_file(p);
        }
    });

    group.finish();
}

// ═════════════════════════════════════════════════════════════════════════════
// 4. PERSIST
// ═════════════════════════════════════════════════════════════════════════════

fn bench_persist(c: &mut Criterion) {
    let corpus_50k = generate_corpus(50_000);
    let mut group = c.benchmark_group("persist");

    group.bench_function("save_50k", |b| {
        let idx = build_index_from_corpus(&corpus_50k);
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("snyd");
        std::fs::create_dir_all(&cache_dir).unwrap();
        let scopes: Vec<PathBuf> = vec![PathBuf::from("/tmp/snyd_bench")];
        b.iter(|| {
            let _ = snyd::persist::save(black_box(&idx), &scopes, &cache_dir);
        });
    });

    group.bench_function("load_50k", |b| {
        let idx = build_index_from_corpus(&corpus_50k);
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("snyd");
        std::fs::create_dir_all(&cache_dir).unwrap();
        let scopes: Vec<PathBuf> = vec![PathBuf::from("/tmp/snyd_bench")];
        snyd::persist::save(&idx, &scopes, &cache_dir).unwrap();
        b.iter(|| {
            let _ = snyd::persist::load(&scopes, &cache_dir);
        });
    });

    group.finish();
}

// ═════════════════════════════════════════════════════════════════════════════
// REGRESSION GUARD (CI baseline)
// ═════════════════════════════════════════════════════════════════════════════

fn bench_regression_guard(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let corpus = generate_corpus(100_000);
    let index = Arc::new(RwLock::new(build_index_from_corpus(&corpus)));
    let app_cache = Arc::new(RwLock::new(AppCache::load(&[])));
    let pipeline = Arc::new(SearchPipeline::new(index, app_cache, vec![]));

    let mut group = c.benchmark_group("regression_guard");
    group.sample_size(100);
    group.measurement_time(std::time::Duration::from_secs(10));

    for (label, query) in [
        ("exact", "budget_report_00042.xlsx"),
        ("broad", "report"),
        ("fuzzy", "bdgt"),
    ] {
        let req = SearchRequest {
            id: "bench".to_string(),
            query: query.to_string(),
            max_results: 20,
            scopes: vec![],
            command: None,
            kind_filter: None,
            content_batch: vec![],
            fuzzy: true,
        };
        group.bench_with_input(BenchmarkId::new(label, query), &req, |b, req| {
            b.to_async(&rt).iter(|| async {
                let mut rx = pipeline.search(req.clone()).await;
                while let Some(_batch) = rx.recv().await {}
            });
        });
    }
    group.finish();
}

// ═════════════════════════════════════════════════════════════════════════════
// CRITERION GROUPS
// ═════════════════════════════════════════════════════════════════════════════

// Expected latency ranges (measured on Apple M1 Pro, release build):
// - exact query:    < 20 ms
// - broad query:    < 40 ms
// - fuzzy typo:     < 25 ms
// - index build 50K: < 500 ms
// - save 50K:        < 200 ms
// - load 50K:        < 100 ms
// If any benchmark exceeds 2× these ranges, investigate before merging.

// ═════════════════════════════════════════════════════════════════════════════
// 6. HEAD-TO-HEAD: snyd vs find vs mdfind (comprehensive)
// ═════════════════════════════════════════════════════════════════════════════

fn setup_head_to_head_dir(size: usize) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("snyd_bench_head_to_head_{}", size));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let names = [
        ("budget", "report", "xlsx"),
        ("meeting", "notes", "docx"),
        ("MyReact", "Component", "tsx"),
        ("User", "Controller", "java"),
        ("image", "001", "png"),
        ("photo", "20240315", "jpg"),
        ("README", "", "md"),
        ("package", "", "json"),
        ("Cargo", "", "toml"),
        ("main", "", "rs"),
        ("index", "", "html"),
        ("styles", "", "css"),
        ("app", "", "swift"),
        ("server", "config", "yaml"),
        ("test", "spec", "js"),
    ];

    for i in 0..size {
        let (a, b, ext) = names[i % names.len()];
        let name = if b.is_empty() {
            format!("{}_{:05}.{}", a, i, ext)
        } else {
            format!("{}_{}_{:05}.{}", a, b, i, ext)
        };
        std::fs::write(dir.join(&name), "test").unwrap();
    }
    dir
}

fn bench_head_to_head(c: &mut Criterion) {
    for size in [1_000usize, 10_000usize] {
        let dir = setup_head_to_head_dir(size);
        let index = TrigramIndex::build(&[dir.clone()]);

        let mut group = c.benchmark_group(format!("head_to_head_{}K", size / 1000));
        group.sample_size(20);
        group.measurement_time(std::time::Duration::from_secs(5));

        // ── Exact match ──────────────────────────────────────────────────
        group.bench_function(format!("snyd_exact/{}", size), |b| {
            b.iter(|| {
                let _results = black_box(index.query("budget_report_00000.xlsx", 20, true));
            })
        });

        group.bench_function(format!("find_exact/{}", size), |b| {
            b.iter(|| {
                let output = std::process::Command::new("find")
                    .arg(&dir)
                    .arg("-name")
                    .arg("budget_report_00000.xlsx")
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .output()
                    .unwrap();
                black_box(output.stdout.len());
            })
        });

        group.bench_function(format!("mdfind_exact/{}", size), |b| {
            b.iter(|| {
                let output = std::process::Command::new("mdfind")
                    .arg("-onlyin")
                    .arg(&dir)
                    .arg("kMDItemDisplayName == 'budget_report_00000.xlsx'cd")
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .output()
                    .unwrap();
                black_box(output.stdout.len());
            })
        });

        // ── Prefix match ─────────────────────────────────────────────────
        group.bench_function(format!("snyd_prefix/{}", size), |b| {
            b.iter(|| {
                let _results = black_box(index.query("budget", 20, true));
            })
        });

        group.bench_function(format!("find_prefix/{}", size), |b| {
            b.iter(|| {
                let output = std::process::Command::new("find")
                    .arg(&dir)
                    .arg("-name")
                    .arg("*budget*")
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .output()
                    .unwrap();
                black_box(output.stdout.len());
            })
        });

        group.bench_function(format!("mdfind_prefix/{}", size), |b| {
            b.iter(|| {
                let output = std::process::Command::new("mdfind")
                    .arg("-onlyin")
                    .arg(&dir)
                    .arg("kMDItemDisplayName == '*budget*'cd")
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .output()
                    .unwrap();
                black_box(output.stdout.len());
            })
        });

        // ── Fuzzy typo ───────────────────────────────────────────────────
        group.bench_function(format!("snyd_fuzzy/{}", size), |b| {
            b.iter(|| {
                let _results = black_box(index.query("bdgt", 20, true));
            })
        });

        group.bench_function(format!("find_fuzzy/{}", size), |b| {
            b.iter(|| {
                let output = std::process::Command::new("find")
                    .arg(&dir)
                    .arg("-name")
                    .arg("*bdgt*")
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .output()
                    .unwrap();
                black_box(output.stdout.len());
            })
        });

        group.bench_function(format!("mdfind_fuzzy/{}", size), |b| {
            b.iter(|| {
                let output = std::process::Command::new("mdfind")
                    .arg("-onlyin")
                    .arg(&dir)
                    .arg("kMDItemDisplayName == '*bdgt*'cd")
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .output()
                    .unwrap();
                black_box(output.stdout.len());
            })
        });

        // ── Broad match (high candidate count) ───────────────────────────
        group.bench_function(format!("snyd_broad/{}", size), |b| {
            b.iter(|| {
                let _results = black_box(index.query("report", 20, true));
            })
        });

        group.bench_function(format!("find_broad/{}", size), |b| {
            b.iter(|| {
                let output = std::process::Command::new("find")
                    .arg(&dir)
                    .arg("-name")
                    .arg("*report*")
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .output()
                    .unwrap();
                black_box(output.stdout.len());
            })
        });

        group.bench_function(format!("mdfind_broad/{}", size), |b| {
            b.iter(|| {
                let output = std::process::Command::new("mdfind")
                    .arg("-onlyin")
                    .arg(&dir)
                    .arg("kMDItemDisplayName == '*report*'cd")
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .output()
                    .unwrap();
                black_box(output.stdout.len());
            })
        });

        // ── Not found ────────────────────────────────────────────────────
        group.bench_function(format!("snyd_notfound/{}", size), |b| {
            b.iter(|| {
                let _results = black_box(index.query("xyznonexistent", 20, true));
            })
        });

        group.bench_function(format!("find_notfound/{}", size), |b| {
            b.iter(|| {
                let output = std::process::Command::new("find")
                    .arg(&dir)
                    .arg("-name")
                    .arg("*xyznonexistent*")
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .output()
                    .unwrap();
                black_box(output.stdout.len());
            })
        });

        group.bench_function(format!("mdfind_notfound/{}", size), |b| {
            b.iter(|| {
                let output = std::process::Command::new("mdfind")
                    .arg("-onlyin")
                    .arg(&dir)
                    .arg("kMDItemDisplayName == '*xyznonexistent*'cd")
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .output()
                    .unwrap();
                black_box(output.stdout.len());
            })
        });

        group.finish();

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }
}

criterion_group!(
    benches,
    bench_index_build,
    bench_from_docs,
    bench_query_latency,
    bench_incremental_update,
    bench_persist,
    bench_regression_guard,
    bench_head_to_head
);
criterion_main!(benches);
