# Shared-core migration validation — 2026-11-07

The reconstructed MCP migration passes the full local MCP suite against the
published canonical core at `4761356749815d283ede41ef956c64b3dbffe048`. It retains
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

The baseline and migrated binaries were built in separate target directories,
`target-mcp-baseline` and `target-mcp`, using Rust 1.99.1 on x86-64 Linux. The final
Git-pinned `cargo test` rebuilt the migrated examples. Their exact hashed Cargo
artifacts, dependency fingerprints and dependency paths resolve through both the
MCP adapter and facade to the clean `4761356` engine checkout; see
[artifact mapping](validation/2026-11-07-integrated/example-artifacts.json).
The original baseline binaries were preserved byte-for-byte before removing
their build cache to reclaim space. Their SHA-256 values match the earlier
isolated-build record; neither baseline source nor its lockfile changed.

The same fixture and benchmark source bytes were used on both sides. The contract
example differs only in rustfmt line wrapping. Source, binary, golden, lockfile
and raw-result SHA-256 values are recorded in
[final benchmark provenance](benchmarks/shared-core-2026-11-07-integrated/provenance.json) and
[baseline contract provenance](validation/2026-11-07/baseline-contract-provenance.json).
The core/FST dependency is fetched by its exact Git revision; no sibling path
override is needed to build the committed manifests.

Earlier measurements used the published `72a64d0` pin. The subsequent `b22aab7`
pin corrected Windows test coverage and Dart hook compatibility. A historical
[recorded source comparison](validation/2026-11-07/core-pin-production-equivalence.txt)
confirms no changes to production Rust or its dependency manifests/lockfile
between those two source trees. The
[earlier benchmark files](benchmarks/shared-core-2026-11-07/) retain their original
pin and binary hashes.

Upstream then advanced to `8ef29fd5`, including the `9f671907` ScanMode follow-up
to the cold-search WorkGate fix. This changes production Rust. The final
`4761356` pin incorporates that upstream work, and the full MCP suite, Clippy,
dependency checks, exact contract examples and three process pairs were rerun.
The earlier production-equivalence record is not applied to this new pin.
New logs and provenance live in
[`validation/2026-11-07-integrated`](validation/2026-11-07-integrated/).

The earlier migration commit was not recovered from the unavailable workspace.
Its claimed local results are not treated as evidence for this implementation.
The independently generated schema-10 index was recovered unchanged from the
preserved core Git tree and is explicitly identified in its
[fixture README](../mcp/tests/fixtures/canonical-7.0.1/README.md).

## Correctness and concurrency

| Check | Result | Evidence |
| --- | --- | --- |
| `cargo test --locked -p otzaria-mcp` against the final Git dependency | 84 substantive tests passed; 9 external-data test functions returned early because no library was supplied | [Full test log](validation/2026-11-07-integrated/git-pin-tests.log) |
| `cargo clippy --locked --offline -p otzaria-mcp --all-targets --no-deps -- -D warnings` | Passed using a separate check-only target in `/dev/shm` after the root filesystem filled | [Clippy log](validation/2026-11-07-integrated/git-pin-clippy.log) |
| `cargo fmt -p otzaria-mcp --check` | Passed | CI repeats the check |
| Normal dependency graph | No Flutter bridge runtime, `allo-isolate`, `dart-sys`, `ort` or `ort-sys`; annotation proc-macro remains | [Dependency graph](validation/2026-11-07-integrated/git-runtime-dependencies.txt) |
| Final baseline and migrated contract examples | Both JSON files are byte-identical to the committed 17-scenario golden | [Baseline](validation/2026-11-07-integrated/baseline-contract-final.json), [migrated](validation/2026-11-07-integrated/migrated-contract-final.json) |
| Independent QA of the MCP adapter before the upstream rebase | 37 tests passed in separately invoked test processes; adapter source is unchanged in the final pin update | [Earlier QA log and binary hashes](validation/2026-11-07/qa-mcp-independent.log) |
| Independent QA of the actual rebased core production delta | 24 scan/WorkGate/cold-search tests passed | [New core QA log](validation/2026-11-07-integrated/qa-core-rebased-independent.log) |

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
the current canonical engine/core accepts 10 and 11. The application's audited
`dev` at `3168cc9e` pins `b78d36eaffcb2a5fa679d34a52133512ee55c084` in both
pubspec files. This adds a WorkGate backport to the earlier `2da2309` pin and
still uses schema 10 (minimum readable schema 10); the application is not assumed
to have adopted the new core revision. Tests reject 7, 8, 9, unsupported newer
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
Rust matches the later `72a64d0`/`b22aab7` revisions; subsequent documentation and
test changes mean those complete Git trees differ. This is retained evidence
from before the upstream rebase. The earlier independent
[QA report](validation/2026-11-07/qa-final-report.txt) records its findings,
fixes, performance-artifact checks and platform limits without being relabeled.

After integrating upstream, independent QA separately executed 10 scan tests,
13 WorkGate tests and the cold-primer regression on the rebased production
implementation, all passing. The last regression exercises one, two and four
Rayon workers. The [new log](validation/2026-11-07-integrated/qa-core-rebased-independent.log)
and [binary/source provenance](validation/2026-11-07-integrated/qa-core-rebased-independent.json)
identify the tested source and executables. The full MCP test log above verifies
fetching and using the final `4761356` Git package.

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
No other agent compilation or benchmark ran during these six processes; process
checks before and after every sample found no Rust build process. Both variants
used the same `TMPDIR` under `/dev/shm`, so their fresh fixtures used tmpfs. This
avoided the full root filesystem. The new capture is kept separate from the
older disk-backed capture, and results from the two environments are not pooled.

| Median across three processes | Untouched MCP main | Shared-core migration |
| --- | ---: | ---: |
| Measured requests | 1,700 | 1,700 |
| Elapsed time | 1.474 s | 1.159 s |
| Completed searches / second | 1,153.12 | 1,466.61 |
| Request latency p50 | 0.792 ms | 0.639 ms |
| Request latency p95 | 1.585 ms | 1.180 ms |
| Request latency p99 | 2.694 ms | 1.975 ms |
| Whole-process high-water RSS | 60,516 KiB | 32,572 KiB |

Throughput ranged from 980.02–1,239.95 searches/s in the baseline and
1,460.77–1,475.40 in the migration; process p95 ranged from 1.488–2.085 ms and
1.154–1.203 ms respectively. The
[new raw six JSON results, stderr logs, summary and provenance](benchmarks/shared-core-2026-11-07-integrated/)
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
Use the same temporary filesystem for both variants. The final capture used a
common tmpfs `TMPDIR` after compilation and before starting any measured process;
the earlier disk-backed capture remains preserved separately.
For this capture the relevant build settings were:

```sh
CARGO_BUILD_JOBS=3 CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0 \
  CARGO_TARGET_DIR=/path/to/variant-target \
  cargo build --locked -p otzaria-mcp --example search_contract --example search_benchmark
```

The preserved schema-10 fixture is intentionally not regenerated as part of
this procedure.
