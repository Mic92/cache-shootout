mod common;

use common::servers::{RunningCache, Server};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use futures::StreamExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Returns the relative NAR URL advertised in the narinfo.
///
/// Never sends `Accept-Encoding`: narinfos are tiny and we need the plaintext
/// to parse the `URL:` line, while reqwest is built without a zstd decoder.
async fn fetch_narinfo(client: &reqwest::Client, base: &str, hash: &str) -> String {
    let url = format!("{base}/{hash}.narinfo");
    let resp = client.get(&url).send().await.expect("narinfo request");
    assert!(
        resp.status().is_success(),
        "{url}: status {}",
        resp.status()
    );
    let text = resp.text().await.expect("narinfo body");
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("URL: ") {
            return v.trim().to_string();
        }
    }
    panic!("{url}: missing URL field");
}

/// GET the NAR and drain the body without buffering it, returning wire bytes
/// read. With `accept_encoding` set, this is the compressed size.
async fn drain_nar(
    client: &reqwest::Client,
    base: &str,
    accept_encoding: Option<&str>,
    nar_url: &str,
) -> u64 {
    let url = format!("{base}/{nar_url}");
    let mut req = client.get(&url);
    if let Some(enc) = accept_encoding {
        req = req.header(reqwest::header::ACCEPT_ENCODING, enc);
    }
    let resp = req.send().await.expect("nar request");
    assert!(
        resp.status().is_success(),
        "{url}: status {}",
        resp.status()
    );
    let mut stream = resp.bytes_stream();
    let mut total = 0u64;
    while let Some(chunk) = stream.next().await {
        total += chunk.expect("nar chunk").len() as u64;
    }
    total
}

/// Resolve narinfos and do one full NAR pass so caches that populate lazily
/// (ncps) and servers with cold OS page cache start from a comparable state.
async fn prepare(
    client: &reqwest::Client,
    cache: &RunningCache,
    hashes: &[String],
) -> (Vec<String>, u64) {
    let mut urls = Vec::with_capacity(hashes.len());
    for h in hashes {
        urls.push(fetch_narinfo(client, &cache.base_url, h).await);
    }
    let mut bytes = 0u64;
    for u in &urls {
        bytes += drain_nar(client, &cache.base_url, cache.accept_encoding, u).await;
    }
    (urls, bytes)
}

/// One fully-prepared (closure, server) combination ready to be benchmarked.
struct Target {
    closure: String,
    cache: RunningCache,
    hashes: Arc<Vec<String>>,
    nar_urls: Arc<Vec<String>>,
    /// Wire bytes for one full NAR pass, used for criterion throughput.
    bytes: u64,
}

fn benchmark(c: &mut Criterion) {
    let closures = common::build_closures();

    let rt = tokio::runtime::Runtime::new().unwrap();
    // Generous pool so the concurrent benches don't queue on the client side.
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(64)
        .build()
        .unwrap();

    // Bring up every (closure, server) pair first. Starting servers inside a
    // criterion group would attribute push/warm-up time to the wrong place and
    // makes it impossible to share one `benchmark_group` across servers.
    let mut targets: Vec<Target> = Vec::new();
    for (cname, croot) in &closures {
        let paths = common::closure_paths(croot);
        let hashes: Arc<Vec<String>> =
            Arc::new(paths.iter().map(|p| common::store_path_hash(p)).collect());
        eprintln!("closure '{cname}': {} store paths", hashes.len());

        for &srv in Server::all() {
            eprintln!("== [{cname}] starting {} ==", srv.name());
            let cache = rt.block_on(srv.start(&client, croot));
            let (urls, bytes) = rt.block_on(prepare(&client, &cache, &hashes));
            eprintln!(
                "[{cname}] {}: {} paths, {:.2} MiB on the wire (warm)",
                cache.name,
                urls.len(),
                bytes as f64 / 1024.0 / 1024.0,
            );
            targets.push(Target {
                closure: cname.clone(),
                cache,
                hashes: hashes.clone(),
                nar_urls: Arc::new(urls),
                bytes,
            });
        }
    }

    // narinfo latency per closure.
    for (cname, _) in &closures {
        let mut group = c.benchmark_group(format!("narinfo_all/{cname}"));
        group.sample_size(10);
        for t in targets.iter().filter(|t| &t.closure == cname) {
            let base = t.cache.base_url.clone();
            let hashes = t.hashes.clone();
            group.bench_with_input(BenchmarkId::from_parameter(t.cache.name), &(), |b, _| {
                b.iter_custom(|iters| {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        let start = Instant::now();
                        rt.block_on(async {
                            for h in hashes.iter() {
                                fetch_narinfo(&client, &base, h).await;
                            }
                        });
                        total += start.elapsed();
                    }
                    total
                })
            });
        }
        group.finish();
    }

    // NAR throughput per closure × concurrency.
    for &conc in &[1usize, 4, 16] {
        for (cname, _) in &closures {
            let mut group = c.benchmark_group(format!("nar_download_c{conc}/{cname}"));
            group.sample_size(10);
            group.measurement_time(Duration::from_secs(10));

            for t in targets.iter().filter(|t| &t.closure == cname) {
                group.throughput(Throughput::Bytes(t.bytes));
                let base = t.cache.base_url.clone();
                let enc = t.cache.accept_encoding;
                let urls = t.nar_urls.clone();
                group.bench_with_input(BenchmarkId::from_parameter(t.cache.name), &(), |b, _| {
                    b.iter_custom(|iters| {
                        let mut total = Duration::ZERO;
                        for _ in 0..iters {
                            let start = Instant::now();
                            rt.block_on(run_pass(&client, &base, enc, &urls, conc));
                            total += start.elapsed();
                        }
                        total
                    })
                });
            }
            group.finish();
        }
    }

    drop(targets);
}

/// One full pass over `urls` with `conc` workers pulling from a shared cursor.
/// NAR sizes are heavily skewed; static partitioning would idle workers.
async fn run_pass(
    client: &reqwest::Client,
    base: &str,
    enc: Option<&'static str>,
    urls: &Arc<Vec<String>>,
    conc: usize,
) {
    if conc == 1 {
        for u in urls.iter() {
            drain_nar(client, base, enc, u).await;
        }
        return;
    }
    let next = Arc::new(AtomicUsize::new(0));
    let mut tasks = Vec::with_capacity(conc);
    for _ in 0..conc {
        let client = client.clone();
        let base = base.to_string();
        let urls = urls.clone();
        let next = next.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(u) = urls.get(i) else { break };
                drain_nar(&client, &base, enc, u).await;
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
}

criterion_group!(benches, benchmark);
criterion_main!(benches);
