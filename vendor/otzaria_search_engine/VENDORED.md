# Vendored: otzaria_search_engine (rust crate `search_engine`)

This directory holds a **vendored copy** of the Rust crate from the
`otzaria_search_engine` Flutter package, pub.dev version **3.1.0** (2026-10-05).

## Why vendored, and why this exact version
- The crate is not published to crates.io; it ships inside the pub.dev package.
- The current Otzaria app depends on `otzaria_search_engine: ^3.1.0`
  (`pubspec.lock` resolves 3.1.0) and builds its Tantivy index with it. That index
  carries **index schema version 5** (`chunkKey`, `lineCheck`, `textStored`,
  `lineFirst`/`lineLast`/`lineEdge`, …) and — for official books — keeps **no line
  text in the index**: results read each line back from `seforim.db` through the
  engine's *library line source* (`configure_line_source`). The previous vendored
  0.6.7 engine reads only schema 2 and cannot open such an index.
- Vendoring the exact 3.1.0 bytes keeps the MCP self-contained and reproducible.

## Source
Copied from `~/.pub-cache/hosted/pub.dev/otzaria_search_engine-3.1.0/rust`
(`Cargo.toml`, `src/`, `resources/`, `vendor/tantivy-fst/`), plus the package's
`LICENSE` and `CHANGELOG.md`. Omitted: the Flutter plugin scaffolding, the crate's
`tests/`, `examples/`, `Cargo.lock` and `cargokit.yaml` (not needed to build the lib).

The package archive is `https://pub.dev/api/archives/otzaria_search_engine-3.1.0.tar.gz`,
sha256 `e7be185d01492383ffa31189637ac0fa5adf1f360c76271c4d1fa981f3abfa96` (the hash the
app's `pubspec.lock` pins). To verify, extract it and run
`diff -r <archive>/rust vendor/otzaria_search_engine/rust`: apart from the omitted files,
the differences are the patches below.

The crate pulls two git dependencies at pinned revisions (fetched by cargo):
`Otzaria/tantivy@85b76fe` (via the workspace `[patch.crates-io]`, which must repeat the
crate's own patch because dependency patches are ignored) and
`Otzaria/otzaria-semantic-search@977d267` (default features off — only the chunk-key
recipe and formats; no ONNX backend is compiled, since the MCP does not enable the
crate's `semantic*` features).

## Local modifications (not in upstream 3.1.0)
Two additive patches in `rust/src/api/search_engine.rs` and `rust/src/line_source.rs`:

- **`SearchEngine::open_read_only(path) -> Result<Self>`** — opens an existing index
  with `Index::open` (never `open_or_create`), acquires **no writer lock** and writes
  **no** `otzaria_index_meta.json`, so the MCP never mutates the app's index. It reuses
  the crate's own `from_read_only_index` (upstream uses it for benchmarks on an index
  another process owns), whose `#[cfg(test)]` gate was removed for this purpose.
  `mcp/src/search.rs` calls it after `check_index_compatibility`; the unit test
  `read_only_open_does_not_create_index_metadata` guards the no-write behaviour.

- **`refresh_line_source(path)`** — replaces the configured path and closes the
  current SQLite connection and row caches under the source mutex, even when the path
  is unchanged. `Search::open` uses it after an atomic database replacement. Active
  row windows finish before refresh; later windows reopen the current file without
  a temporary suspended/unavailable state. The `reload_regression` integration test
  covers replacement during concurrent searches and stale-text reporting.

## Upgrading
When the app moves to a newer engine, re-vendor the matching version here, re-apply the
patches above, and confirm compatibility by comparing the crate's `INDEX_SCHEMA_VERSION`
against the index's `otzaria_index_meta.json` (`check_index_compatibility`).
Also re-copy the crate's `[patch.crates-io]` entries into the workspace `Cargo.toml`.
