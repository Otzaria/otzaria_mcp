# Vendored: otzaria_search_engine (rust crate `search_engine`)

This directory holds a **vendored copy** of the Rust crate from the
`otzaria_search_engine` Flutter package: version **4.1.0 plus merged stable-book-id
support**, commit `30e8f1682428d5cc252b3e610f2c9cc06c04b81f` on the official
`https://github.com/Otzaria/otzaria_search_engine` `refactor` branch. This includes
PRs #52, #56 and #57; no new version or release tag has been published yet.

## Why vendored, and why this exact version
- The crate is not published to crates.io; it ships inside the pub.dev package.
- The engine must match the app that builds the Tantivy index. This version writes
  **index schema version 7** and reads only schema 7 (`MIN_READABLE_SCHEMA_VERSION`):
  an index built by an older app (schema 5/6) is refused by `check_index_compatibility`
  until the app rebuilds it. Official books normally read line text back from
  `seforim.db` through the engine's *library line source*. Books with restored
  `line_note` bodies keep that reconstructed text in the index so search results
  include the same text that was indexed.
- Schema 7 ids are `(bookNumber << 32) + ordinal + 1`, with book numbers the engine hands
  out on a book's first write. The catalogue and generation order are no longer in the
  ids or in a `generation` field: the app writes them to `otzaria_book_order.json` in the
  index directory (`set_book_order`). The MCP never writes that file; every engine,
  including the read-only one, loads it on open and re-reads it when it changes on disk
  (checked on every catalogue/generation search). Without the file, or for a book missing
  from it, results sort by book number (books in the mapping first).
- Base DB text, commentaries and link snippets use the upstream public
  `inline_notes::{BookNote, rehydrate_inline_notes}` renderer, so restored bodies
  follow exactly the same marker, UTF-16 and block-boundary rules as indexing.
- Vendoring exact bytes keeps the MCP self-contained and reproducible.

## Source
Copied from `git archive 30e8f1682428d5cc252b3e610f2c9cc06c04b81f rust LICENSE CHANGELOG.md`
(`rust/Cargo.toml`, `src/`, `resources/`, `vendor/tantivy-fst/`, plus `LICENSE` and
`CHANGELOG.md`). Omitted: the Flutter plugin scaffolding, the crate's `tests/`,
`examples/` and `Cargo.lock` (not needed to build the lib). To verify, from a clone of the
engine that has the commit:

```sh
mkdir /tmp/engine && git archive 30e8f1682428d5cc252b3e610f2c9cc06c04b81f rust | tar -x -C /tmp/engine
diff -r /tmp/engine/rust <mcp>/vendor/otzaria_search_engine/rust
```

Apart from the omitted files, the differences are the patches below.

The crate pulls two git dependencies at pinned revisions (fetched by cargo):
`Otzaria/tantivy@8b88320` (via the workspace `[patch.crates-io]`, which must repeat the
crate's own patch because dependency patches are ignored) and
`Otzaria/otzaria-semantic-search@3aeb94e` (default features off — only the chunk-key
recipe and formats; no ONNX backend is compiled, since the MCP does not enable the
crate's `semantic*` features). Cargo warns that the crate's own `[patch]` and
`[profile.release]` are ignored as a non-root package; that is expected.

## Local modifications (not upstream)
Two additive patches in `rust/src/api/search_engine.rs` and `rust/src/line_source.rs`:

- **`SearchEngine::open_read_only(path) -> Result<Self>`** — opens an existing index
  with `Index::open` (never `open_or_create`), acquires **no writer lock** and writes
  **no** `otzaria_index_meta.json`, so the MCP never mutates the app's index. It reuses
  the crate's own `from_read_only_index` (upstream uses it for benchmarks on an index
  another process owns), whose `#[cfg(test)]` gate was removed for this purpose. That
  constructor loads `otzaria_book_order.json`, so sorted searches follow the app's order.
  `mcp/src/search.rs` calls it after `check_index_compatibility`; the unit test
  `read_only_open_does_not_create_index_metadata` guards the no-write behaviour.

- **`refresh_line_source(path)`** — replaces the configured path and closes the
  current SQLite connection and row caches under the source mutex, even when the path
  is unchanged. Upstream `configure_line_source` reopens a replaced file only when its
  identity changes, and off unix every file at a path has the same identity, so it is
  still needed. It also ends running whole-book scans and waits for them, as a path
  change does. `Search::open` uses it after an atomic database replacement. Active
  row windows finish before refresh; later windows reopen the current file without
  a temporary suspended/unavailable state. The `reload_regression` integration test
  covers replacement during concurrent searches and stale-text reporting.

## Upgrading
When the app moves to a newer engine, re-vendor the matching version here, re-apply the
patches above, and confirm compatibility by comparing the crate's `INDEX_SCHEMA_VERSION`
against the index's `otzaria_index_meta.json` (`check_index_compatibility`).
Also re-copy the crate's `[patch.crates-io]` entries into the workspace `Cargo.toml`.
