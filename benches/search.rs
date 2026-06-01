use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::runtime::Runtime;
use tokio::sync::RwLock;

use snyd::index::TrigramIndex;
use snyd::apps::AppCache;
use snyd::pipeline::SearchPipeline;
use snyd::protocol::SearchRequest;

fn bench_index_build(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    let mut group = c.benchmark_group("index_build");
    for size in [1_000, 10_000, 50_000] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        // Create N files
        for i in 0..size {
            let dir = root.join(format!("dir{}", i % 100));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("file{}.txt", i)), "hello world").unwrap();
        }

        group.bench_with_input(BenchmarkId::new("files", size), &root, |b, root| {
            b.iter(|| {
                let _ = TrigramIndex::build(&[root.clone()]);
            });
        });
    }
    group.finish();
}

fn bench_search_exact(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();

    // Build 100k file corpus
    for i in 0..100_000 {
        let dir = root.join(format!("dir{}", i % 1000));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("document_{}_report.txt", i)), "sample content").unwrap();
    }

    let index = Arc::new(RwLock::new(TrigramIndex::build(&[root.clone()])));
    let app_cache = Arc::new(RwLock::new(AppCache::load(&[])));
    let pipeline = Arc::new(SearchPipeline::new(index, app_cache, vec![root]));

    let mut group = c.benchmark_group("search_exact");
    for query in ["document_500", "report", "document_99999"] {
        let req = SearchRequest {
            id: "bench".to_string(),
            query: query.to_string(),
            max_results: 50,
            scopes: vec![],
            command: None,
            kind_filter: None,
            content_batch: vec![],
        };
        group.bench_with_input(BenchmarkId::new("query", query), &req, |b, req| {
            b.to_async(&rt).iter(|| async {
                let mut rx = pipeline.search(req.clone()).await;
                while let Some(_batch) = rx.recv().await {}
            });
        });
    }
    group.finish();
}

fn bench_fuzzy(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();

    for i in 0..50_000 {
        let dir = root.join(format!("proj{}", i % 500));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("budget_{}_2024.pdf", i)), "content").unwrap();
    }

    let index = Arc::new(RwLock::new(TrigramIndex::build(&[root.clone()])));
    let app_cache = Arc::new(RwLock::new(AppCache::load(&[])));
    let pipeline = Arc::new(SearchPipeline::new(index, app_cache, vec![root]));

    let mut group = c.benchmark_group("fuzzy_search");
    for query in ["bdgt", "budge", "budget_25000", "2024"] {
        let req = SearchRequest {
            id: "bench".to_string(),
            query: query.to_string(),
            max_results: 20,
            scopes: vec![],
            command: None,
            kind_filter: None,
            content_batch: vec![],
        };
        group.bench_with_input(BenchmarkId::new("query", query), &req, |b, req| {
            b.to_async(&rt).iter(|| async {
                let mut rx = pipeline.search(req.clone()).await;
                while let Some(_batch) = rx.recv().await {}
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_index_build, bench_search_exact, bench_fuzzy);
criterion_main!(benches);
