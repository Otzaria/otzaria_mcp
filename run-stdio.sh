#!/usr/bin/env bash
# Run otzaria-mcp over stdio (for Claude Desktop / MCP clients).
# Builds a release binary on first run, then execs it. Extra args pass through,
# e.g.:  ./run-stdio.sh --data-dir /path/to/otzaria_latest
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="$DIR/target/release/otzaria-mcp"
if [ ! -x "$BIN" ]; then
  echo "otzaria-mcp: building release binary (first run, this may take a few minutes)…" >&2
  cargo build --release --manifest-path "$DIR/Cargo.toml" -p otzaria-mcp >&2
fi
exec "$BIN" "$@"
