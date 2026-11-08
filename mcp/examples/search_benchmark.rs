//! Small synthetic search benchmark. Use the same source and fixture for both
//! revisions; these numbers do not estimate performance of a full library.
#[path = "../tests/support/search_contract.rs"]
#[allow(dead_code)]
mod contract;

fn main() -> anyhow::Result<()> {
    let fixture = contract::Fixture::new()?;
    let search = fixture.search()?;
    let cases = contract::requests();
    for _ in 0..5 {
        for (_, request) in &cases {
            std::hint::black_box(search.search(request)?);
        }
    }
    let mut samples = Vec::with_capacity(100 * cases.len());
    let mut total_hits = 0_u64;
    let start = std::time::Instant::now();
    for _ in 0..100 {
        for (_, request) in &cases {
            let before = std::time::Instant::now();
            let result = search.search(request)?;
            samples.push(before.elapsed().as_secs_f64() * 1000.0);
            total_hits += u64::from(result.total);
            std::hint::black_box(result);
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    samples.sort_by(f64::total_cmp);
    let percentile = |p: f64| samples[((samples.len() - 1) as f64 * p).ceil() as usize];
    let rss_kib = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status.lines().find_map(|line| {
                line.strip_prefix("VmHWM:")?
                    .split_whitespace()
                    .next()?
                    .parse::<u64>()
                    .ok()
            })
        });
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "fixture": "17 deterministic scenarios / 15 indexed lines / sparse stable IDs",
            "build_profile": if cfg!(debug_assertions) { "dev" } else { "release" },
            "requests": samples.len(), "total_hit_checksum": total_hits,
            "elapsed_s": elapsed, "throughput_requests_per_s": samples.len() as f64 / elapsed,
            "latency_p50_ms": percentile(0.50), "latency_p95_ms": percentile(0.95),
            "latency_p99_ms": percentile(0.99), "peak_rss_kib": rss_kib,
        "rss_scope": "whole process high-water mark, including fixture indexing and warmup",
            "queue": "uncontended, serialized search; use admission_bench for queue behavior",
        }))?
    );
    Ok(())
}
