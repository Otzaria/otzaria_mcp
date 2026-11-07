# Shared-core migration validation — 2026-11-07

The reconstructed MCP migration passes the full local MCP suite against the
published canonical core at `b22aab7c7addd845d69919272074d437408e5cbe`. It retains
the merged admission fixes, removes the copied engine, and preserves the tested
search responses. This report separates new evidence from the earlier session's
unavailable migration checkout. The design and merge order are in
[SHARED_CORE_MIGRATION.md](SHARED_CORE_MIGRATION.md).

## Provenance

The baseline is untouched MCP main
`249ae4e2aa80734a7e0b1ed08dbf7a98dd3e608b`, tree
`3bdb6192793bd254e1f66d9ee66c1c4697afa1df`. It already contains admission PR #4 and
the subsequent metadata/maintenance corrections. Its tracked source and lockfile
were checked before and after measurement; both remained unchanged. Test/example
harness files were added only as untracked files in that baseline checkout.

The baseline and migrated binaries were rebuilt in separate target directories,
`target-mcp-baseline` and `target-mcp`, using Rust 1.99.1 on x86-64 Linux. This
avoids relying on earlier preliminary output from a shared Cargo target. Only
the isolated builds and final Git dependency checks are used below.

The same fixture and benchmark source bytes were used on both sides. The contract
example differs only in rustfmt line wrapping. Source, binary, golden, lockfile
and raw-result SHA-256 values are recorded in
[benchmark provenance](benchmarks/shared-core-2026-11-07/provenance.json) and
[baseline contract provenance](validation/2026-11-07/baseline-contract-provenance.json).
The core/FST dependency is fetched by its exact Git revision; no sibling path
override is needed to build the committed manifests.

Measurements used the earlier published `72a64d0` pin. The final `b22aab7` pin
corrects Windows test coverage and Dart hook compatibility. A
[recorded source comparison](validation/2026-11-07/core-pin-production-equivalence.txt)
confirms no changes to production Rust or its dependency manifests/lockfile
between the two exact source trees. Benchmark files retain their original pin
and binary hashes; final locked MCP tests and Clippy were repeated with `b22aab7`.

The earlier migration commit was not recovered from the unavailable workspace.
Its claimed local results are not treated as evidence for this implementation.
The independently generated schema-10 index was recovered unchanged from the
preserved core Git tree and is explicitly identified in its
[fixture README](../mcp/tests/fixtures/canonical-7.0.1/README.md).

## Correctness and concurrency

| Check | Result | Evidence |
| --- | --- | --- |
| `cargo test --locked -p otzaria-mcp` against the Git dependency | 84 substantive tests passed; 9 external-data test functions returned early because no library was supplied | [Full test log](validation/2026-11-07/git-pin-tests.log) |
| `cargo clippy --locked -p otzaria-mcp --all-targets --no-deps -- -D warnings` | Passed | [Clippy log](validation/2026-11-07/git-pin-clippy.log) |
| `cargo fmt -p otzaria-mcp --check` | Passed | CI repeats the check |
| Normal dependency graph | No Flutter bridge runtime, `allo-isolate`, `dart-sys`, `ort` or `ort-sys`; annotation proc-macro remains | [Dependency graph](validation/2026-11-07/git-runtime-dependencies.txt) |
| Independent QA rerun of the focused MCP contracts | 37 tests passed in separately invoked test processes | [QA log and binary hashes](validation/2026-11-07/qa-mcp-independent.log) |
| Independent QA execution of the untouched baseline contract | Exact match to the committed 17-scenario golden | [Captured baseline JSON](validation/2026-11-07/qa-baseline-contract.json) |

The 84 substantive MCP tests comprise 53 unit tests, 14 admission tests, one
current-note search test, one preserved-index test, eight library-refresh tests,
three MCP protocol tests, three older reload regressions and one search contract
test. The harness reports 93 passes because the nine external-data functions
return successfully when their environment is absent; this is not evidence of a
production-library run.

The strict search contract checks 17 scenarios on a 15-line fixture with sparse
SQLite IDs, distinct index insertion/catalogue/generation orders, exact and fuzzy
search, advanced negative/prefix/distance queries, facets, grouping, pagination,
and all serialized snippet/highlight fields. Responses are compared without
normalization. A separate assertion verifies that relevance pagination equals
the corresponding slice of the complete relevance ordering.

An early synthetic relevance fixture had two equal-score rows: the canonical
text field stores basic postings, so repeated terms do not provide the intended
frequency distinction. The fixture was corrected to give the five relevance
hits distinct token lengths/ranks before the final baseline was captured. Three
fresh baseline processes then produced identical bytes. No production sort was
changed and no differing result was normalized away.

The preserved schema-10 fixture was built by the unmodified canonical engine at
`225a6d5d7265c478a64d3a609a536d0deed433e7`. Its 26 files are copied as data, not
regenerated by the implementation under test. Sixteen searches assert the
independently captured results after conversion to MCP wire fields. Opening,
searching and closing preserve every fixture byte; a subsequent directory rename
checks reader shutdown. New fixtures are built by the current schema-11 engine.

Schema compatibility is an intentional change: the old MCP copy used schema 7;
the current canonical engine/core accepts 10 and 11. The application still pins
the older `2da2309` engine revision with schema 10; it has not been assumed to
adopt the new core revision. Tests reject 7, 8, 9, unsupported newer
versions, missing metadata on a populated index, and a missing index directory
without rebuilding them. SQLite-only tools stay available. Existing application
indexes at schema 10 remain readable; a schema-7 MCP index needs an application
rebuild.

The concurrency tests retain the search/SQLite mutexes and the admission tests
for finite FIFO queues, cancellation before worker start, closure-owned permits,
reserved maintenance capacity, and blocking-pool saturation. Refresh tests cover
WAL commits and checkpoints, empty/absent WAL equivalence, SQLite permission
changes, snapshot mode, stale queued observations and replacement with the same
size/modification time. Sixteen concurrent requests run during a DB/index
replacement; mismatched rows are marked stale without false highlights.

Independent QA identified a redundant refresh race: two callers could observe
one changed generation before admission and both reopen it. The worker now
rechecks the published fingerprint after admission. A deterministic test queues
two refreshes behind a held search permit and verifies exactly one publication.
The same-path live replacement tests are Unix-specific; local results do not
establish live Windows replacement semantics. The portable preserved-index test
does check replacement after the reader closes.

Current `hasMarker` flags, compressed note bodies and legacy marker/TEXT notes
are tested separately. Corrupt compressed/UTF-8 bodies and invalid current marker
flags fail with context and release the SQLite transaction. Base rows and notes
for each read batch share a transaction; note insertion does not shift original
link-anchor coordinates.

## Shared-core evidence retained with this migration

The independent QA pass also exercised core read-only/open/refresh contracts,
the existing-index fixture, writer lifecycle guards, semantic read-only guards,
the cold-search Rayon deadlock regression, and explicit source refresh. These
executions and their binary hashes are preserved in the
[core QA log](validation/2026-11-07/qa-core-independent.log). They were performed
against implementation `3173eeededad79aeced96453aee9cae71cec2741`, whose production
Rust matches the later 72a64d0/b22aab7 revisions; subsequent documentation and
test changes mean the complete Git trees differ. The MCP test log above
separately verifies fetching and using the final Git package. The independent
[final QA report](validation/2026-11-07/qa-final-report.txt) records findings,
fixes, performance-artifact checks and remaining platform limits.

Dependency-tree evidence includes the
[upstream Flutter defaults](validation/2026-11-07/qa-upstream_default-tree.txt),
[shared-engine Flutter defaults](validation/2026-11-07/qa-shared_default-tree.txt),
and [headless facade defaults](validation/2026-11-07/qa-headless_default-tree.txt).
The MCP/Tokio wrapper is not pulled into Flutter. These graphs support dependency
scope, not a measured release-binary size claim. Cross-platform release builds
and the core's feature-matrix checks run in their respective GitHub workflows;
local Linux results are not represented as those CI outcomes.

## Measured search behavior

Each of three process pairs ran the same 17 requests for five warmup cycles and
100 measured cycles: 85 warmups and 1,700 measured searches per process. All six
reported the same total-hit checksum, 5,700. Process order alternated between
baseline-first and migration-first. Builds used the dev profile, debug
information disabled, incremental compilation disabled, and three build jobs.
No other agent compilation or benchmark ran during these six processes.

| Median across three processes | Untouched MCP main | Shared-core migration |
| --- | ---: | ---: |
| Measured requests | 1,700 | 1,700 |
| Elapsed time | 1.660 s | 1.304 s |
| Completed searches / second | 1,024.01 | 1,303.55 |
| Request latency p50 | 0.852 ms | 0.698 ms |
| Request latency p95 | 2.061 ms | 1.396 ms |
| Request latency p99 | 3.844 ms | 2.400 ms |
| Whole-process high-water RSS | 57,740 KiB | 33,124 KiB |

Throughput ranged from 962.30–1,091.69 searches/s in the baseline and
1,292.04–1,316.52 in the migration; process p95 ranged from 1.776–2.521 ms and
1.389–1.532 ms respectively. The
[raw six JSON results, stderr logs, summary and provenance](benchmarks/shared-core-2026-11-07/)
are committed so the aggregate can be checked independently.

These are observations from a tiny, warm, uncontended synthetic fixture on a
shared host. Each revision builds its fixture with its own engine/schema (7 in
the baseline, 11 in the migration). Engine evolution, schema, dependency graph
and implementation all differ. The results do not isolate a causal benefit of
the facade, estimate production speed or establish full-library memory usage.
Linux `VmHWM` covers the entire process, including fixture indexing and warmup;
it is not search-only memory. No release-mode performance claim is made.

This search probe deliberately has no queue contention. Queue time, admission
latency, worker fan-out, rejection counts, throughput and memory under saturation
remain covered by the dedicated
[admission probe and recorded measurements](admission-control.md). The migration
keeps its scheduling logic unchanged and reruns its concurrency regressions.

## Reproduce

Run the checks in [the migration guide](SHARED_CORE_MIGRATION.md#validation-commands).
For a paired comparison, use separate clean checkouts and target directories.
Add the same `mcp/tests/support/search_contract.rs` and examples to the untouched
baseline as untracked harness files; do not modify its source or lockfile.
Build both `search_contract` and `search_benchmark` with `--locked` and the same
profile. Run each example in a fresh process, preserve stdout and stderr, and
verify identical fixture source hashes and contract JSON before comparing timings.
For this capture the relevant build settings were:

```sh
CARGO_BUILD_JOBS=3 CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0 \
  CARGO_TARGET_DIR=/path/to/variant-target \
  cargo build --locked -p otzaria-mcp --example search_contract --example search_benchmark
```

The preserved schema-10 fixture is intentionally not regenerated as part of
this procedure.
