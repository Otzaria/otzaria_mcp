# MCP migration to the canonical Rust core

The MCP now depends on `otzaria-search-core` from the canonical
`Otzaria/otzaria_search_engine` repository. The old engine source under `vendor/`
is removed. The facade reuses the same search implementation as Flutter without
the Flutter bridge runtime. The MCP keeps its own protocol, admission and response
formatting; Flutter keeps its FRB wrapper. No MCP or Tokio dependency is added to
Flutter. The engine's annotation proc-macro remains a build-time dependency.

This describes the canonical engine's two adapters. The application's audited
`dev` at `3168cc9e` pins engine revision
`b78d36eaffcb2a5fa679d34a52133512ee55c084` in both pubspec files. That revision
still uses schema 10 and adds a WorkGate backport to the earlier `2da2309` pin;
no application dependency upgrade is assumed by this migration.

## Revisions and merge sequence

1. Admission control: [otzaria_mcp #4](https://github.com/Otzaria/otzaria_mcp/pull/4),
   already merged into `249ae4e2aa80734a7e0b1ed08dbf7a98dd3e608b`.
2. Shared core: [otzaria_search_engine #84](https://github.com/Otzaria/otzaria_search_engine/pull/84),
   the canonical engine facade and its compatibility tests.
3. This MCP migration: pin the core and `tantivy-fst` patch to the same accessible
   canonical engine commit, and retain the reviewed lockfile.

The core and FST patch are pinned together to
`4761356749815d283ede41ef956c64b3dbffe048` from #84. This is a reachable published
commit, not a local checkout or a presumed future merge. Its source tree matches
the reviewed local core `25d1073` exactly. This version incorporates current
`refactor` at `8ef29fd592793fbc9d5af0299fdd7a8607789bac`, including the upstream
`9f671907` ScanMode follow-up to the cold-search WorkGate fix. That follow-up
changes production Rust. Validation records keep the earlier `72a64d0`/`b22aab7`
captures separate from the new pin and do not claim production equivalence to
those older revisions.

This work was reconstructed on 2026-11-07 from the current MCP main after the
previous temporary workspace became unavailable. Its prior local migration commit
was not recovered. The new test outputs are captured independently; the earlier
report's local test counts and timings are not used as validation of this tree.
The existing schema-10 index fixture was recovered byte-for-byte from the
preserved core Git tree, with its independent baseline provenance intact.

The current engine base already includes writer lifecycle PR
[otzaria_search_engine #77](https://github.com/Otzaria/otzaria_search_engine/pull/77).
This migration consumes that lifecycle rather than depending on an unmerged PR.
Opening an MCP handle permanently disables writer acquisition. The engine's
`writer_closed` behavior for app handles is preserved by the core change.

Cargo ignores `[patch.crates-io]` in dependencies, so the workspace root retains
the canonical engine's exact Tantivy fork and FST patch. Updating only the facade
revision without its FST patch is not a supported update procedure.

## Index and identifier contract

Current canonical code opens schemas 10 and 11 after checking the exact stored
Tantivy schema for the declared version. Index schemas 7–9 require an application
rebuild; a newer schema requires an engine update. The package version alone is
not a compatibility check: both schema-10 and schema-11 engine revisions are
labelled 7.0.1 upstream. The read-only open performs the canonical check and does
not rewrite metadata, create an index, recover a sidecar, or open an index writer.
The narrow canonical exception for missing metadata remains limited to an empty
index whose stored Tantivy schema exactly matches the current one.

An incompatible or missing index disables full-text tools with an explanation in
`library_info`; SQLite-only tools stay available. The MCP never rebuilds the
application's index. Tantivy's metadata coordination lock file may be created by
an otherwise read-only reader; it is not a writer lock or index mutation.

Official SQLite book IDs come only from `file_path = id:<bookId>`, never from the
engine's internal book number. Sparse IDs and differing insertion, catalogue and
generation orders are tested. User, external and PDF results retain their existing
source classifications and do not acquire false SQLite IDs.

## Concurrency and refresh

The admission gates remain before `spawn_blocking`. The merged admission change's
reserved FIFO maintenance slot, live metadata reads inside workers, and parameter
validation before admission are retained. Search and maintenance use the same
lane; queued work captures the current snapshot only after admission. Queries
that have already started keep their permit until completion.

The search mutex remains: the engine has mutable caches and a process-wide line
source. The SQLite mutex remains: one connection and mutable decompression context
are shared. Removing either requires separate safety analysis and measurement.

The watcher debounces DB, non-empty WAL, index metadata and book-order changes,
then opens and publishes handles on an admitted blocking worker. It checks the
file fingerprint before and after opening. File identity catches an atomic
replacement even if size and modification time were preserved. Empty WAL and
absent WAL are equivalent; only WAL `ctime` is ignored on Unix because SQLite can
change it while synchronizing file permissions without changing content. WAL
identity, size and modification time are still tracked, as are all DB/index
timestamps. Explicit immutable snapshot mode ignores WAL as SQLite does.

A snapshot swap is not a transaction across the DB and index. SQLite readers
see live WAL commits and the engine source is process-wide. The index's content
checks therefore remain essential: a current row that differs from the indexed
row is marked `stale` and is not highlighted as a match. `data_version` is a
file-generation hint, not a transaction identifier. Fresh DB schema metadata is
still read in the admitted worker, even before the watcher reopens handles.

## Notes and response compatibility

The DB adapter understands current `line_note.hasMarker` flags and dictionary-
compressed note bodies, while preserving legacy `marker`/TEXT layout support.
It uses the canonical note renderer. Malformed compressed bodies, UTF-8 or current
marker flags produce contextual errors; NULL bodies and malformed note indices
follow the engine's skip rules. Base text and notes for each read batch share one
SQLite transaction, with failures releasing it. Link offsets continue to
address the original base text rather than inserted note bodies.

Regression coverage includes a 17-scenario MCP golden from current main before
migration, exact catalogue/generation/relevance ordering, a real fuzzy typo,
pagination, facets, grouping, negative/advanced queries, and all snippet fields.
The relevance fixture has distinct ranks and was verified in three independent
baseline processes without normalization. A separate preserved schema-10 index
checks 16 wrapper responses against the old canonical engine's captured results
and verifies that opening, searching and closing leave its bytes unchanged.

`library_reload` tests exercise WAL commits/checkpoints, file identity, stale
queued refreshes, concurrent queries, full DB/index replacement, and stale-hit
highlight suppression. `schema7_tests` retains legacy note coverage; current
layout tests cover compressed notes and corruption handling.

## Validation commands

```sh
cargo fmt -p otzaria-mcp --check
cargo clippy --locked -p otzaria-mcp --all-targets --no-deps -- -D warnings
cargo test --locked -p otzaria-mcp
cargo tree --locked -p otzaria-mcp --edges normal --prefix none
cargo run --locked -p otzaria-mcp --example search_benchmark
```

CI runs these quality gates and builds release binaries on both Linux and macOS
architectures and Windows x86-64. Real-data tests require an external library;
the synthetic fixtures run unconditionally. The synthetic search benchmark is
for reproducible comparisons of these paths, not a forecast for a full library.
Admission queue/latency/throughput/memory results remain documented separately in
[`admission-control.md`](admission-control.md).

The fresh test counts, independent QA evidence, paired measurements and their
limitations are recorded in [MIGRATION_VALIDATION.md](MIGRATION_VALIDATION.md).
