use std::env;
use std::sync::Arc;

use bench_vortex::reader::{
    take_parquet, take_parquet_object_store, take_vortex_object_store, take_vortex_tokio,
};
use bench_vortex::taxi_data::{taxi_data_parquet, taxi_data_vortex};
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use mimalloc::MiMalloc;
use object_store::aws::AmazonS3Builder;
use object_store::local::LocalFileSystem;
use object_store::ObjectStore;
use tokio::runtime::Runtime;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

const INDICES: [u64; 6] = [10, 11, 12, 13, 100_000, 3_000_000];

/// Benchmarks against object stores require setting
/// * AWS_ACCESS_KEY_ID
/// * AWS_SECRET_ACCESS_KEY
/// * AWS_BUCKET
/// * AWS_ENDPOINT
///
/// environment variables and assume files to read are already present
fn random_access_vortex(c: &mut Criterion) {
    let mut group = c.benchmark_group("random-access");

    let taxi_vortex = taxi_data_vortex();
    group.bench_function("vortex-tokio-local-disk", |b| {
        b.to_async(Runtime::new().unwrap())
            .iter(|| async { black_box(take_vortex_tokio(&taxi_vortex, &INDICES).await.unwrap()) })
    });

    group.bench_function("vortex-local-fs", |b| {
        let local_fs = Arc::new(LocalFileSystem::new()) as Arc<dyn ObjectStore>;
        let local_fs_path = object_store::path::Path::from_filesystem_path(&taxi_vortex).unwrap();
        b.to_async(Runtime::new().unwrap()).iter(|| async {
            black_box(
                take_vortex_object_store(local_fs.clone(), local_fs_path.clone(), &INDICES)
                    .await
                    .unwrap(),
            )
        })
    });

    // everything below here is a lot slower, so we'll run fewer samples
    group.sample_size(10);

    let taxi_parquet = taxi_data_parquet();
    group.bench_function("parquet-tokio-local-disk", |b| {
        b.to_async(Runtime::new().unwrap())
            .iter(|| async { black_box(take_parquet(&taxi_parquet, &INDICES).await.unwrap()) })
    });

    if env::var("AWS_ACCESS_KEY_ID").is_ok() {
        group.bench_function("vortex-r2", |b| {
            let r2_fs =
                Arc::new(AmazonS3Builder::from_env().build().unwrap()) as Arc<dyn ObjectStore>;
            let r2_path = object_store::path::Path::from_url_path(
                taxi_vortex.file_name().unwrap().to_str().unwrap(),
            )
            .unwrap();

            b.to_async(Runtime::new().unwrap()).iter(|| async {
                black_box(
                    take_vortex_object_store(r2_fs.clone(), r2_path.clone(), &INDICES)
                        .await
                        .unwrap(),
                )
            })
        });

        group.bench_function("parquet-r2", |b| {
            let r2_fs = Arc::new(AmazonS3Builder::from_env().build().unwrap());
            let r2_parquet_path = object_store::path::Path::from_url_path(
                taxi_parquet.file_name().unwrap().to_str().unwrap(),
            )
            .unwrap();

            b.to_async(Runtime::new().unwrap()).iter(|| async {
                black_box(
                    take_parquet_object_store(r2_fs.clone(), &r2_parquet_path, &INDICES)
                        .await
                        .unwrap(),
                )
            })
        });
    }
}

criterion_group!(benches, random_access_vortex);
criterion_main!(benches);
