# Vendored: otzaria_search_engine (rust crate `search_engine`)

This directory holds a **vendored copy** of the Rust crate from the
`otzaria_search_engine` Flutter package, pub.dev version **0.6.7**.

## Why vendored, and why this exact version
- The crate is not published to crates.io, and the upstream GitHub repo
  (`github.com/Otzaria/otzaria_search_engine`) has no release tags — its default
  branches are either older (schema v1) or newer (a refactor with a changed
  Tantivy schema). Neither can open the index the installed app actually built.
- The installed Otzaria app depends on `otzaria_search_engine: ^0.6.7`, and the
  on-disk Tantivy index carries `schema_version: 2`, `tantivy_version: 0.26.1`.
  The 0.6.7 crate's `current_schema()` matches that index exactly, so
  `SearchEngine::new` opens it without a schema-mismatch panic.
- Vendoring the exact 0.6.7 bytes makes the MCP self-contained and reproducible
  for anyone building it, without depending on an elusive upstream commit.

## Source
Copied from `~/.pub-cache/hosted/pub.dev/otzaria_search_engine-0.6.7/rust`
(only the `rust/` crate is needed; the Flutter plugin scaffolding is omitted).

## Local modifications (not in upstream 0.6.7)
This copy carries **one** additive, upstream-safe patch — it adds a new method and
changes nothing existing:

- **`SearchEngine::open_read_only(path) -> Result<Self>`** in
  `rust/src/api/search_engine.rs`. Unlike the stock `SearchEngine::new`, it opens
  the index with `Index::open` (never `open_or_create`), acquires **no writer
  lock**, and writes **no** `otzaria_index_meta.json` sidecar — so the MCP is truly
  read-only and never mutates the user's index. It also returns a `Result` (schema
  mismatch → `Err`) instead of panicking. The MCP's `mcp/src/search.rs` calls this
  instead of `new`; a unit test (`read_only_open_does_not_create_index_metadata`)
  guards the no-write behavior.

This method cannot live in the `mcp` crate because it needs `SearchEngine`'s private
fields, hence the in-place patch here.

## Upgrading
If a future Otzaria release rebuilds the index with a newer schema, re-vendor the
matching `otzaria_search_engine` version here and rebuild. Confirm compatibility by
comparing the crate's `INDEX_SCHEMA_VERSION` / `current_schema()` against the index's
`otzaria_index_meta.json` and `meta.json`.

**IMPORTANT:** a plain re-vendor will drop the `open_read_only` patch above and break
the build (`mcp/src/search.rs` calls it). After re-vendoring, re-apply that method
(or, if upstream has since added an equivalent read-only open, point `search.rs` at it).
