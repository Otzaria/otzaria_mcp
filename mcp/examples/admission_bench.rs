//! Process-isolated saturation probe: identical serialized work, two schedulers.
//!
//! cargo build -p otzaria-mcp --example admission_bench
//! target/debug/examples/admission_bench baseline 256
//! target/debug/examples/admission_bench bounded 256 255  # all requests accepted
//! target/debug/examples/admission_bench bounded 256 32   # production queue bound
//!
//! Each request locks the same mutex; the first holds it for 100 ms, others 2 ms.
//! No production data or artificial per-worker memory allocations are used.
//! This measures scheduling under serialization, not search-engine speed.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use otzaria_mcp::admission::{Config, Error, Gate};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Probe {
    active: AtomicUsize,
    peak_workers: AtomicUsize,
    serialized: Mutex<usize>,
}

struct Worker<'a>(&'a Probe);
impl Drop for Worker<'_> {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Probe {
    fn operation(&self, requested: Instant) -> u64 {
        let active = self.active.fetch_add(1, Ordering::Relaxed) + 1;
        self.peak_workers.fetch_max(active, Ordering::Relaxed);
        let _worker = Worker(self);
        let mut ordinal = self.serialized.lock().unwrap();
        let queue_us = micros(requested.elapsed());
        std::thread::sleep(Duration::from_millis(if *ordinal == 0 { 100 } else { 2 }));
        *ordinal += 1;
        queue_us
    }
}

#[derive(Default, Clone, Copy, Serialize)]
struct Memory {
    rss_kib: u64,
    high_water_rss_kib: u64,
    virtual_kib: u64,
    threads: u64,
}
impl Memory {
    fn read() -> Option<Self> {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let get = |name: &str| {
            status
                .lines()
                .find_map(|line| {
                    let suffix = line.strip_prefix(name)?;
                    suffix.split_whitespace().next()?.parse::<u64>().ok()
                })
                .unwrap_or(0)
        };
        Some(Self {
            rss_kib: get("VmRSS:"),
            high_water_rss_kib: get("VmHWM:"),
            virtual_kib: get("VmSize:"),
            threads: get("Threads:"),
        })
    }
    fn maximum(self, other: Self) -> Self {
        Self {
            rss_kib: self.rss_kib.max(other.rss_kib),
            high_water_rss_kib: self.high_water_rss_kib.max(other.high_water_rss_kib),
            virtual_kib: self.virtual_kib.max(other.virtual_kib),
            threads: self.threads.max(other.threads),
        }
    }
}

fn micros(duration: Duration) -> u64 {
    duration.as_micros().min(u64::MAX as u128) as u64
}

fn quantiles(mut samples: Vec<u64>) -> serde_json::Value {
    samples.sort_unstable();
    if samples.is_empty() {
        return serde_json::Value::Null;
    }
    let at = |percent: usize| samples[(samples.len() * percent).div_ceil(100).saturating_sub(1)];
    serde_json::json!({ "p50_us": at(50), "p95_us": at(95), "max_us": samples.last().unwrap() })
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("bounded");
    anyhow::ensure!(
        matches!(mode, "baseline" | "bounded"),
        "mode must be baseline or bounded"
    );
    let requests = args
        .get(2)
        .map(|n| n.parse::<usize>())
        .transpose()?
        .unwrap_or(256);
    anyhow::ensure!((1..=512).contains(&requests), "requests must be 1..=512");
    let queue_capacity = args
        .get(3)
        .map(|n| n.parse::<usize>())
        .transpose()?
        .unwrap_or(32);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(512)
        .enable_all()
        .build()?;
    let memory_before = Memory::read();
    let stop = Arc::new(AtomicBool::new(false));
    let sample_stop = stop.clone();
    let sampler = std::thread::spawn(move || {
        let mut maximum = Memory::read();
        while !sample_stop.load(Ordering::Relaxed) {
            if let Some(now) = Memory::read() {
                maximum = Some(maximum.unwrap_or_default().maximum(now));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        maximum
    });
    let probe = Arc::new(Probe::default());
    let gate = Gate::new(Config {
        workers: 1,
        queue_capacity,
        queue_timeout: Duration::from_secs(30),
    });
    let began = Instant::now();
    let results = runtime.block_on(async {
        let barrier = Arc::new(tokio::sync::Barrier::new(requests + 1));
        let mut tasks = Vec::with_capacity(requests);
        for _ in 0..requests {
            let barrier = barrier.clone();
            let probe = probe.clone();
            let gate = gate.clone();
            let bounded = mode == "bounded";
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                let requested = Instant::now();
                let result = if bounded {
                    match gate.acquire(CancellationToken::new()).await {
                        Ok(admitted) => admitted.run(move || probe.operation(requested)).await,
                        Err(e) => Err(e),
                    }
                } else {
                    tokio::task::spawn_blocking(move || probe.operation(requested))
                        .await
                        .map_err(Error::Join)
                };
                (result, micros(requested.elapsed()))
            }));
        }
        barrier.wait().await;
        let mut results = Vec::with_capacity(requests);
        for task in tasks {
            results.push(task.await.unwrap());
        }
        results
    });
    let elapsed = began.elapsed();
    stop.store(true, Ordering::Relaxed);
    let sampled_peak = sampler.join().unwrap();
    let memory_after = Memory::read();
    let mut queue = Vec::new();
    let mut completed_latency = Vec::new();
    let mut rejected_latency = Vec::new();
    let mut errors = Vec::new();
    for (result, latency) in results {
        match result {
            Ok(wait) => {
                queue.push(wait);
                completed_latency.push(latency);
            }
            Err(Error::Full) => rejected_latency.push(latency),
            Err(e) => errors.push(e.to_string()),
        }
    }
    let completed = queue.len();
    let rejected = rejected_latency.len();
    let output = serde_json::json!({
        "mode": mode,
        "requests": requests,
        "queue_capacity": if mode == "bounded" { Some(queue_capacity) } else { None },
        "workload": { "first_serial_operation_ms": 100, "remaining_operation_ms": 2 },
        "completed": completed,
        "rejected": rejected,
        "errors": errors,
        "elapsed_us": micros(elapsed),
        "completed_per_second": completed as f64 / elapsed.as_secs_f64(),
        "max_blocking_workers": probe.peak_workers.load(Ordering::Relaxed),
        "queue": quantiles(queue),
        "completed_latency": quantiles(completed_latency),
        "rejected_latency": quantiles(rejected_latency),
        "memory_before": memory_before,
        "memory_sampled_peak": sampled_peak,
        "memory_after": memory_after,
        "gate": if mode == "bounded" { Some(gate.metrics()) } else { None },
    });
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}
