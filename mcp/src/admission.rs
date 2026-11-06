//! Bounded admission before creating Tokio blocking workers.
//!
//! A worker permit belongs to the closure, not its caller. Dropping or cancelling
//! an RPC therefore cannot admit another worker while the first is still using
//! a synchronous DB/engine mutex. Only work which has not started is discarded.

use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::{oneshot, OwnedSemaphorePermit, Semaphore};
use tokio::task::AbortHandle;
use tokio_util::sync::CancellationToken;

const PENDING: u8 = 0;
const RUNNING: u8 = 1;
const CANCELLED: u8 = 2;
const EXPIRED: u8 = 3;
const HISTOGRAM_BUCKETS: usize = 32;

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub workers: usize,
    /// Maximum additional accepted requests waiting for a worker.
    pub queue_capacity: usize,
    /// Deadline to START work, including time in Tokio's blocking pool queue.
    /// Once a synchronous operation starts it is allowed to finish safely.
    pub queue_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            workers: 1,
            queue_capacity: 32,
            queue_timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Debug)]
pub enum Error {
    Full,
    Expired,
    Cancelled,
    Closed,
    Join(tokio::task::JoinError),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => write!(f, "request queue is full; retry with lower concurrency"),
            Self::Expired => write!(f, "request expired before execution; retry if still needed"),
            Self::Cancelled => write!(f, "request cancelled before its response was ready"),
            Self::Closed => write!(f, "server is shutting down"),
            Self::Join(e) => write!(f, "blocking task join error: {e}"),
        }
    }
}
impl std::error::Error for Error {}

/// One serialized resource lane. Clones share both limits and observations.
#[derive(Clone)]
pub struct Gate {
    shared: Arc<Shared>,
}

struct Shared {
    config: Config,
    capacity: Arc<Semaphore>,
    workers: Arc<Semaphore>,
    maintenance: Arc<Semaphore>,
    shutdown: CancellationToken,
    since: Instant,
    metrics: Metrics,
}

impl Default for Gate {
    fn default() -> Self {
        Self::new(Config::default())
    }
}

impl Gate {
    pub fn new(config: Config) -> Self {
        assert!(config.workers > 0, "at least one worker is required");
        assert!(
            config.queue_timeout > Duration::ZERO,
            "queue timeout must be positive"
        );
        let total = config
            .workers
            .checked_add(config.queue_capacity)
            .expect("admission capacity overflow");
        Self {
            shared: Arc::new(Shared {
                config,
                capacity: Arc::new(Semaphore::new(total)),
                workers: Arc::new(Semaphore::new(config.workers)),
                maintenance: Arc::new(Semaphore::new(1)),
                shutdown: CancellationToken::new(),
                since: Instant::now(),
                metrics: Metrics::default(),
            }),
        }
    }

    /// Admission happens before callers capture a library snapshot or allocate a
    /// blocking task. Tokio's worker semaphore queues accepted callers FIFO.
    pub async fn acquire(&self, cancellation: CancellationToken) -> Result<Admitted, Error> {
        self.acquire_inner(cancellation, false).await
    }

    /// One reserved slot for the library watcher. It joins the same FIFO worker
    /// queue as requests but cannot be rejected or expire under sustained load.
    pub async fn acquire_maintenance(&self) -> Result<Admitted, Error> {
        self.acquire_inner(CancellationToken::new(), true).await
    }

    async fn acquire_inner(
        &self,
        cancellation: CancellationToken,
        maintenance: bool,
    ) -> Result<Admitted, Error> {
        let requested = Instant::now();
        let shared = &self.shared;
        if shared.shutdown.is_cancelled() {
            return Err(Error::Closed);
        }
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let capacity = if maintenance {
            tokio::select! {
                biased;
                _ = shared.shutdown.cancelled() => return Err(Error::Closed),
                permit = shared.maintenance.clone().acquire_owned() => permit.map_err(|_| Error::Closed)?,
            }
        } else {
            shared.capacity.clone().try_acquire_owned().map_err(|_| {
                shared.metrics.rejected.fetch_add(1, Ordering::Relaxed);
                Error::Full
            })?
        };
        let metrics = &shared.metrics;
        metrics.accepted.fetch_add(1, Ordering::Relaxed);
        let in_flight = metrics.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
        metrics
            .peak_in_flight
            .fetch_max(in_flight, Ordering::Relaxed);
        let queued = metrics.queued.fetch_add(1, Ordering::Relaxed) + 1;
        metrics.peak_queued.fetch_max(queued, Ordering::Relaxed);
        let state = Arc::new(AtomicU8::new(PENDING));
        let cancellation = cancellation.child_token();
        let record = Record {
            shared: shared.clone(),
            requested,
            started: None,
            succeeded: false,
            state: state.clone(),
            cancellation: cancellation.clone(),
            _capacity: capacity,
        };
        let deadline = (!maintenance)
            .then(|| tokio::time::Instant::from_std(requested + shared.config.queue_timeout));
        let worker = tokio::select! {
            biased;
            _ = shared.shutdown.cancelled() => return Err(Error::Closed),
            _ = cancellation.cancelled() => return Err(Error::Cancelled),
            _ = wait_for_deadline(deadline) => {
                state.store(EXPIRED, Ordering::Release);
                return Err(Error::Expired);
            },
            worker = shared.workers.clone().acquire_owned() => worker.map_err(|_| Error::Closed)?,
        };
        metrics.blocking_tasks.fetch_add(1, Ordering::Relaxed);
        Ok(Admitted {
            record,
            _worker: WorkerPermit {
                shared: shared.clone(),
                _permit: worker,
            },
            deadline,
        })
    }

    /// Wake queued requests, discard not-started closures, and let running work
    /// release its own permits. No attempt is made to interrupt synchronous I/O.
    pub fn close(&self) {
        self.shared.shutdown.cancel();
    }

    pub fn metrics(&self) -> MetricsSnapshot {
        let m = &self.shared.metrics;
        let elapsed = self.shared.since.elapsed().as_secs_f64();
        let completed = m.completed.load(Ordering::Relaxed);
        MetricsSnapshot {
            workers: self.shared.config.workers,
            queue_capacity: self.shared.config.queue_capacity,
            queue_timeout_ms: self.shared.config.queue_timeout.as_millis() as u64,
            accepted: m.accepted.load(Ordering::Relaxed),
            rejected: m.rejected.load(Ordering::Relaxed),
            cancelled_before_start: m.cancelled.load(Ordering::Relaxed),
            expired_before_start: m.expired.load(Ordering::Relaxed),
            completed,
            failed: m.failed.load(Ordering::Relaxed),
            completed_after_cancellation: m.detached.load(Ordering::Relaxed),
            in_flight: m.in_flight.load(Ordering::Relaxed),
            queued: m.queued.load(Ordering::Relaxed),
            blocking_tasks: m.blocking_tasks.load(Ordering::Relaxed),
            running: m.running.load(Ordering::Relaxed),
            peak_in_flight: m.peak_in_flight.load(Ordering::Relaxed),
            peak_queued: m.peak_queued.load(Ordering::Relaxed),
            peak_running: m.peak_running.load(Ordering::Relaxed),
            completed_per_second: completed as f64 / elapsed.max(f64::EPSILON),
            queue: m.queue.snapshot(),
            execution: m.execution.snapshot(),
            latency: m.latency.snapshot(),
        }
    }
}

/// Admission lease. Always pass ownership into `run`, never keep it on the
/// async caller while creating an unrelated blocking task.
pub struct Admitted {
    record: Record,
    _worker: WorkerPermit,
    deadline: Option<tokio::time::Instant>,
}

impl Admitted {
    pub async fn run<T, F>(self, f: F) -> Result<T, Error>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let state = self.record.state.clone();
        let cancellation = self.record.cancellation.clone();
        let shutdown = self.record.shared.shutdown.clone();
        let deadline = self.deadline;
        let (started_tx, mut started_rx) = oneshot::channel();
        let mut task = tokio::task::spawn_blocking(move || {
            // Drop the accounting record before releasing the worker permit,
            // including on panic, so a successor cannot overlap its metrics.
            let mut admitted = self;
            let record = &mut admitted.record;
            if record.cancellation.is_cancelled() || record.shared.shutdown.is_cancelled() {
                let _ = record.state.compare_exchange(
                    PENDING,
                    CANCELLED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            } else if deadline.is_some_and(|deadline| Instant::now() >= deadline.into_std()) {
                let _ = record.state.compare_exchange(
                    PENDING,
                    EXPIRED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            }
            if let Err(reason) =
                record
                    .state
                    .compare_exchange(PENDING, RUNNING, Ordering::AcqRel, Ordering::Acquire)
            {
                return Err(if reason == EXPIRED {
                    Error::Expired
                } else {
                    Error::Cancelled
                });
            }
            record.start();
            let _ = started_tx.send(());
            let result = f();
            record.succeeded = true;
            Ok(result)
        });
        let mut guard = CancelOnDrop {
            token: cancellation.clone(),
            state: state.clone(),
            abort: task.abort_handle(),
            armed: true,
        };
        let result = tokio::select! {
            biased;
            result = &mut task => result.map_err(Error::Join)?,
            _ = shutdown.cancelled() => return Err(Error::Closed),
            _ = cancellation.cancelled() => return Err(Error::Cancelled),
            _ = &mut started_rx => {
                // The deadline applies only until start. Cancellation may detach
                // a running operation; its closure still owns both permits.
                tokio::select! {
                    biased;
                    result = &mut task => result.map_err(Error::Join)?,
                    _ = shutdown.cancelled() => return Err(Error::Closed),
                    _ = cancellation.cancelled() => return Err(Error::Cancelled),
                }
            },
            _ = wait_for_deadline(deadline) => {
                if state.compare_exchange(PENDING, EXPIRED, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                    return Err(Error::Expired);
                }
                // The worker won the start race; never time out running work.
                tokio::select! {
                    biased;
                    result = &mut task => result.map_err(Error::Join)?,
                    _ = shutdown.cancelled() => return Err(Error::Closed),
                    _ = cancellation.cancelled() => return Err(Error::Cancelled),
                }
            },
        };
        guard.armed = false;
        result
    }
}

async fn wait_for_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

struct CancelOnDrop {
    token: CancellationToken,
    state: Arc<AtomicU8>,
    abort: AbortHandle,
    armed: bool,
}
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.state.compare_exchange(
                PENDING,
                CANCELLED,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            self.token.cancel();
            self.abort.abort(); // Tokio can abort a queued closure, never a running one.
        }
    }
}

struct WorkerPermit {
    shared: Arc<Shared>,
    _permit: OwnedSemaphorePermit,
}
impl Drop for WorkerPermit {
    fn drop(&mut self) {
        self.shared
            .metrics
            .blocking_tasks
            .fetch_sub(1, Ordering::Relaxed);
    }
}

struct Record {
    shared: Arc<Shared>,
    requested: Instant,
    started: Option<Instant>,
    succeeded: bool,
    state: Arc<AtomicU8>,
    cancellation: CancellationToken,
    _capacity: OwnedSemaphorePermit,
}
impl Record {
    fn start(&mut self) {
        let now = Instant::now();
        self.started = Some(now);
        let m = &self.shared.metrics;
        m.queued.fetch_sub(1, Ordering::Relaxed);
        let running = m.running.fetch_add(1, Ordering::Relaxed) + 1;
        m.peak_running.fetch_max(running, Ordering::Relaxed);
        m.queue.observe(now.duration_since(self.requested));
    }
}
impl Drop for Record {
    fn drop(&mut self) {
        let m = &self.shared.metrics;
        m.in_flight.fetch_sub(1, Ordering::Relaxed);
        m.latency.observe(self.requested.elapsed());
        if let Some(started) = self.started {
            m.running.fetch_sub(1, Ordering::Relaxed);
            m.execution.observe(started.elapsed());
            if self.succeeded {
                m.completed.fetch_add(1, Ordering::Relaxed);
            } else {
                m.failed.fetch_add(1, Ordering::Relaxed);
            }
            if self.cancellation.is_cancelled() || self.shared.shutdown.is_cancelled() {
                m.detached.fetch_add(1, Ordering::Relaxed);
            }
        } else {
            m.queued.fetch_sub(1, Ordering::Relaxed);
            if self.state.load(Ordering::Acquire) == EXPIRED {
                m.expired.fetch_add(1, Ordering::Relaxed);
            } else {
                m.cancelled.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

#[derive(Default)]
struct Metrics {
    accepted: AtomicU64,
    rejected: AtomicU64,
    cancelled: AtomicU64,
    expired: AtomicU64,
    completed: AtomicU64,
    failed: AtomicU64,
    detached: AtomicU64,
    in_flight: AtomicUsize,
    queued: AtomicUsize,
    blocking_tasks: AtomicUsize,
    running: AtomicUsize,
    peak_in_flight: AtomicUsize,
    peak_queued: AtomicUsize,
    peak_running: AtomicUsize,
    queue: Histogram,
    execution: Histogram,
    latency: Histogram,
}

/// Constant-space logarithmic histograms. Quantiles are bucket upper bounds,
/// not exact samples. No query text, identifiers, or unbounded vectors are kept.
struct Histogram {
    buckets: [AtomicU64; HISTOGRAM_BUCKETS],
    count: AtomicU64,
    sum_us: AtomicU64,
    max_us: AtomicU64,
}
impl Default for Histogram {
    fn default() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            count: AtomicU64::new(0),
            sum_us: AtomicU64::new(0),
            max_us: AtomicU64::new(0),
        }
    }
}
impl Histogram {
    fn observe(&self, duration: Duration) {
        let micros = duration.as_micros().min(u64::MAX as u128) as u64;
        let bucket = (u64::BITS - micros.saturating_sub(1).leading_zeros()) as usize;
        self.buckets[bucket.min(HISTOGRAM_BUCKETS - 1)].fetch_add(1, Ordering::Relaxed);
        self.sum_us.fetch_add(micros, Ordering::Relaxed);
        self.max_us.fetch_max(micros, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
    }
    fn snapshot(&self) -> TimingSnapshot {
        let count = self.count.load(Ordering::Relaxed);
        let quantile = |numerator: u64| {
            if count == 0 {
                return 0;
            }
            let target = count.saturating_mul(numerator).div_ceil(100);
            let mut seen = 0;
            for (i, bucket) in self.buckets.iter().enumerate() {
                seen += bucket.load(Ordering::Relaxed);
                if seen >= target {
                    return if i == HISTOGRAM_BUCKETS - 1 {
                        u64::MAX
                    } else {
                        1u64 << i
                    };
                }
            }
            self.max_us.load(Ordering::Relaxed)
        };
        TimingSnapshot {
            count,
            sum_us: self.sum_us.load(Ordering::Relaxed),
            max_us: self.max_us.load(Ordering::Relaxed),
            p50_us_upper_bound: quantile(50),
            p95_us_upper_bound: quantile(95),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct TimingSnapshot {
    pub count: u64,
    pub sum_us: u64,
    pub max_us: u64,
    pub p50_us_upper_bound: u64,
    pub p95_us_upper_bound: u64,
}

#[derive(Debug, Serialize)]
pub struct MetricsSnapshot {
    pub workers: usize,
    pub queue_capacity: usize,
    pub queue_timeout_ms: u64,
    pub accepted: u64,
    pub rejected: u64,
    pub cancelled_before_start: u64,
    pub expired_before_start: u64,
    /// Closures which returned normally (including application-level errors).
    pub completed: u64,
    /// Closures which panicked. Ordinary DB/engine errors remain RPC errors.
    pub failed: u64,
    pub completed_after_cancellation: u64,
    pub in_flight: usize,
    /// Accepted requests not yet started, including Tokio's blocking pool queue.
    pub queued: usize,
    /// Leases held by admitted callers, pool-queued closures, or running closures.
    pub blocking_tasks: usize,
    pub running: usize,
    pub peak_in_flight: usize,
    pub peak_queued: usize,
    pub peak_running: usize,
    pub completed_per_second: f64,
    pub queue: TimingSnapshot,
    pub execution: TimingSnapshot,
    /// Accepted admission-to-worker-completion/drop latency; excludes transport.
    pub latency: TimingSnapshot,
}
