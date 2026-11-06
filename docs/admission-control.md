# Bounded admission for MCP blocking work

## Problem and resource model

Before this change every tool handler called `spawn_blocking` immediately and
captured `Library::current()` first. Search calls then serialized on
`Search.inner: Mutex<SearchEngine>`; SQLite calls serialized on `Db.inner`.
A request burst therefore created many blocking tasks that waited for the same
mutex. Those closures also retained the DB/index snapshot captured on arrival,
even if a library replacement happened before execution.

The search mutex is retained. `SearchEngine` operations use mutable state and
reader/cache state; the vendored engine also has a process-global line source.
`Db` retains its mutex because one SQLite connection and its decoding state are
shared. This PR changes scheduling around those ownership boundaries; it does
not establish that either underlying resource can be accessed without its lock.

Each `Library` owns two admission lanes:

| Lane | Users | Running/admitted worker limit | Additional accepted requests |
| --- | --- | ---: | ---: |
| `read` | The 12 DB/navigation/metadata tools | 1 | 32 |
| `search` | `full_text_search`, `search_facet_counts`, background index refresh | 1 | 32 requests + 1 reserved refresh |

The separate read lane allows short navigation reads while a long search is
running. Search may still briefly contend with reads while resolving DB metadata;
that contention is bounded by the two worker lanes. Index refresh takes the same
search lane, including publication, so it cannot open another large engine while
a search is using the current one. The watcher awaits each refresh and cannot
build a backlog of refresh workers. A single reserved maintenance slot joins
the same FIFO worker queue without a start deadline, preventing a full request
queue from starving refresh. A running operation must still finish first.

## Admission and cancellation

`Gate::acquire` first takes one of `workers + queue_capacity` finite capacity
permits without waiting. When full, it rejects the call before creating a blocking
task. Accepted callers await Tokio's fair semaphore asynchronously. The default
start deadline is 30 seconds, including time spent waiting for a Tokio blocking
pool thread after admission. Expired requests that have not started are discarded.

Only after admission does `Admitted::run` create `spawn_blocking`. The closure
captures the library snapshot when it actually starts. Neither queue retains a
retired snapshot. A response uses the same snapshot as the completed operation.

The closure owns both permits. Cancelling or dropping the RPC future cannot
release the worker permit while synchronous work is running. A pending-to-running
atomic transition arbitrates the start/cancellation race; a drop guard also aborts
a pool-queued task. Already running operations finish and release their permits
on return or panic. No attempt is made to stop SQLite or Tantivy in the middle of
an operation, and the deadline stops applying once execution starts. Identical
queries from unrelated callers are never superseded.

All 14 tool handlers consume the cancellation token supplied by rmcp's request
context. On transport completion, the server closes both gates and aborts the
watcher; pending work is cancelled, while running work retains its lease until
safe completion. rmcp may drain outstanding responses before transport completion.
This change does not impose a kill deadline on synchronous operations.

The Tokio behavior this design relies on is documented in
[`spawn_blocking`](https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html):
started blocking tasks cannot be aborted, and its pool can queue tasks after the
thread limit. Semaphore fairness is documented in
[`Semaphore`](https://docs.rs/tokio/latest/tokio/sync/struct.Semaphore.html).

## Runtime safety and errors

Response metadata is read from SQLite inside the admitted blocking worker, so
WAL updates are visible without taking the SQLite mutex on a Tokio runtime
thread. `db_schema_version` is read for `library_info`; each successful tool
reads `db_version` for its response. Large refresh builds run on the admitted blocking
worker. Releasing the retired snapshot occurs outside the short publication
`RwLock`, because closing DB/index handles can take time.

Admission errors use MCP `internal_error` with a machine-readable `data.reason`:

| Reason | Meaning |
| --- | --- |
| `queue_full` | No finite queue slot is available; back off and retry if still needed. |
| `queue_expired` | The request did not start before its deadline. |
| `cancelled` | The request was cancelled before its response was ready. |
| `shutting_down` | The library gates have closed. |
| `worker_failed` | The blocking task panicked or otherwise failed to join. |

Running cancellation may discard the response; it does not claim the synchronous
operation was interrupted. These tools are read-only. The existing DB, search,
validation, and result payload errors remain intact.

## Observability

`library_info.admission.read` and `.search` expose independent process-lifetime
observations. They include accepted/rejected counts; cancellation and expiry
before start; normally returned and panicked closures; completions after
cancellation; current and peak queue/in-flight/running counts; and completed work
per second since lane creation. The search lane observations include the reserved refresh slot, so its in-flight
count can briefly exceed the client capacity by one. `blocking_tasks` counts worker leases, including
a held lease or a pool-queued closure, not all Tokio process threads.

Queue, execution, and accepted-request latency use constant-space logarithmic
microsecond histograms. Their p50/p95 values are **bucket upper bounds**, not exact
samples. Queue time covers accepted work which starts; cancelled/expired work is
counted separately. Latency covers accepted work until closure completion or
release of its admission record, excluding serialization and transport. A normally
returned application error counts as a completed closure; `failed` counts panics.

Counters are sampled independently with relaxed atomics, so a concurrent snapshot
is approximate. Metrics store no query text, book IDs, or per-request vectors.
A cancelled pool-queued closure can retain its permits until Tokio disposes of it;
this preserves the resource bound even when another subsystem saturates the pool.

## Validation and reproducible saturation probe

`mcp/tests/admission_control.rs` exercises FIFO order and finite capacity, no
mutex-waiting worker fan-out, queue expiry, explicit cancellation and dropped
callers before/after start, expiry and cancellation in Tokio's own pool queue,
shutdown, panic release, and preservation of a running operation after its start
deadline. Tests also check the reserved refresh slot and blocking-pool saturation.
`mcp/tests/mcp_admission_regression.rs` exercises real MCP calls with WAL metadata
updates, invalid arguments under a full queue, and a watcher refresh under load.
A server unit test holds SQLite's mutex while formatting a response on a
current-thread runtime and fails if formatting waits for the database.

Build and run the process-isolated probe:

```sh
cargo test --locked -p otzaria-mcp
cargo build --locked -p otzaria-mcp --example admission_bench
target/debug/examples/admission_bench baseline 256
target/debug/examples/admission_bench bounded 256 255
target/debug/examples/admission_bench bounded 256 32
```

Run each mode in a fresh process, at least three times. All modes perform the same
serialized operation behind one mutex: the first holds it for 100 ms and every
subsequent operation holds it for 2 ms. The all-accepted bounded comparison uses
255 queue slots; the production-default comparison uses 32 and reports rejections
separately. Comparing only latency of completed requests under load shedding would
hide rejected work, so the JSON includes completed and rejected counts, rejection
latency, total wall time, and completed throughput.

The probe records exact queue/response p50/p95, maximum live blocking closures
(including mutex waiters), and Linux RSS/high-water RSS/virtual memory/thread count
before, during, and after the burst. No artificial per-worker buffer is allocated.
On other platforms process memory observations are null. Sampling may miss very
short memory peaks; VmHWM provides the kernel's process high-water measurement.

This isolates the scheduling/resource problem; it does **not** measure Tantivy
query speed or promise higher search throughput. Actual search-result compatibility
is verified separately by the engine/MCP regression suites and the shared-core
migration contract. The measured results below separate this focused verification from full-crate
integration verification.

### Measured saturation results (2026-11-05)

These are the original pre-review results; they do not measure the later metadata
refresh change. Each entry is the median of three separate processes. Raw per-run JSON and build
scope/environment metadata are in
[`benchmarks/admission-2026-11-05`](benchmarks/admission-2026-11-05/).

| Measurement | Immediate spawn (baseline pattern) | Bounded, all accepted (255 queue slots) | Bounded, default (32 queue slots) |
| --- | ---: | ---: | ---: |
| Completed / rejected requests | 256 / 0 | 256 / 0 | 33 / 223 |
| Maximum simultaneous blocking operations, including mutex waiters | 256 | 1 | 1 |
| Queue p50 | 390.910 ms | 420.706 ms | 136.512 ms |
| Queue p95 | 661.469 ms | 706.867 ms | 174.115 ms |
| Completed-request latency p50 | 393.043 ms | 422.865 ms | 138.726 ms |
| Completed-request latency p95 | 664.275 ms | 709.050 ms | 176.296 ms |
| Completed operations / second | 353.5 | 347.1 | 182.9 |
| Peak resident memory / kernel high-water RSS | 5,520 KiB | 2,844 KiB | 2,756 KiB |
| Peak virtual address space | 5,193,248 KiB | 343,616 KiB | 276,016 KiB |
| Peak process thread count | 260 | 6 | 5 |

The all-accepted comparison shows the intended resource improvement with a modest
scheduling cost: median completion throughput was about 1.8% lower and queue p95
was about 6.9% higher. It does not support a search-speed claim. Default admission
reduces accepted tail latency by rejecting 223/256 calls (87.1%); clients must
back off. Its lower operations/second also reflects the deliberately expensive
first operation being amortized over only 33 completed operations. Rejection p95
rounded to zero at this probe's microsecond precision.

Process threads include runtime, main, sampler, and any idle blocking-pool threads.
Tokio may retain an extra idle blocking thread around a handoff even though only
one operation owns the resource lane at a time. Resident and virtual memory are
reported separately; reserving a thread stack does not make every reserved page
resident. Absolute RSS here is for a small scheduling probe, not the memory usage
of a loaded search index. The shared container was not an isolated performance
lab, and three runs are a small sample.

The 11 admission tests first passed using a temporary minimal Rust harness that
references the exact committed admission source and tests by path. The same harness
built this exact example because the initial full build exhausted shared-container
disk space. It uses Tokio 1.53.1 and tokio-util 0.7.20, matching the application
lockfile; other harness dependency versions are in `environment.json`.

### Full application verification

After the build environment became available, the full application checks also
completed successfully, with one Cargo build job, no debug information, and
incremental compilation disabled:

| Checkout and command | Passed | Failed | Existing ignored tests |
| --- | ---: | ---: | ---: |
| Baseline `7971f5afcac7b09d9122e1668d98cef18b172d47`: `cargo test --offline --locked --workspace -- --test-threads=1` | 776 | 0 | 24 |
| Admission branch: `cargo test --offline --locked -p otzaria-mcp -- --test-threads=1` | 71 | 0 | 0 |

The 71 application tests include all 59 pre-existing MCP tests, the 11 admission
concurrency/cancellation tests, and the new response-metadata SQLite contention
regression. Serial test-suite execution does not make the admission tests serial
workloads: those tests explicitly run concurrent callers, a saturated blocking
pool, cancellation races, and overlapping operations. The unchanged vendored
engine's 717 passing tests are included in the baseline workspace total.

A deterministic 21-case search contract was additionally captured by the actual
baseline checkout for the subsequent shared-core migration. It covers seven query
forms under catalogue, generation, and relevance ordering, with paginated results,
book counts, facets, snippets, and highlights. An independently created persistent
fixture index written by the same old engine produced a byte-identical contract;
it is available to verify opening an existing index during migration. This
admission change does not edit the vendored engine.

### Metadata freshness cost (review follow-up)

On 2026-11-06, a local macOS ARM debug-build probe compared PR head
`37aba102` with the follow-up metadata fix. Three fresh processes per variant
served 600 sequential `get_text` MCP requests after 50 warmups, using the same
synthetic 10,000-line SQLite database. The median of process medians was
0.872 ms before vs 0.857 ms after for one line, and 1.734 ms before vs
1.740 ms after for 50 lines. The median process p95 was 1.181 vs 0.907 ms and
1.963 vs 2.022 ms respectively. These small, noisy samples show no clear
regression in this scenario; they do not measure release builds, concurrent
loads, compressed production data, or Tantivy searches.

To reproduce with two separately built binaries (build into separate target
directories):

```sh
python3 docs/benchmarks/metadata-latency.py /path/to/original-binary /path/to/fixed-binary
```
