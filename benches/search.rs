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
        .map(|(path, name, mtime)| DocEntry {
            path: path.clone(),
            name_lower: name.to_lowercase(),
            path_dir_lower: String::new(),
            acronym: String::new(),
            tokens: Vec::new(),
            body_lower: String::new(),
            body_tokens: Vec::new(),
            kind: snyd::protocol::ResultKind::File,
            mtime: *mtime,
            size: 1024,
            deleted: false,
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

criterion_group!(
    benches,
    bench_index_build,
    bench_from_docs,
    bench_query_latency,
    bench_incremental_update,
    bench_persist,
    bench_regression_guard
);
criterion_main!(benches);
