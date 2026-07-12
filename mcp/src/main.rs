//! otzaria-mcp — an offline MCP server over the Otzaria library.
//! Thin binary: parse args, resolve paths, open the library, serve over stdio.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use rmcp::transport::stdio;
use rmcp::ServiceExt;

use otzaria_mcp::paths::PathArgs;
use otzaria_mcp::reload::Library;
use otzaria_mcp::server::OtzariaServer;

const USAGE: &str = "\
otzaria-mcp — MCP server over the Otzaria library (read-only, offline)

USAGE:
    otzaria-mcp [OPTIONS]

Transport: stdio (MCP JSON-RPC on stdin/stdout; logs go to stderr).

PATH OPTIONS (precedence: flags > env > --data-dir > per-OS defaults):
    --db <path>           Path to seforim.db            [env: OTZARIA_DB]
    --index <path>        Path to the Tantivy index dir [env: OTZARIA_INDEX]
    --lexical-db <path>   Path to lexical.db (fuzzy)    [env: OTZARIA_LEXICAL_DB]
    --data-dir <root>     Otzaria data root; two layouts auto-detected:
                            <root>/otzaria/seforim.db + <root>/index   (download root)
                            <root>/seforim.db          + <root>/index   (library dir)
                                                        [env: OTZARIA_DATA_DIR]
    --snapshot            Open the DB with immutable=1 (closed snapshot; ignores WAL)
    -h, --help            Print this help
    -V, --version         Print version
";

#[tokio::main]
async fn main() -> Result<()> {
    let args = match parse_args()? {
        Some(args) => args,
        None => return Ok(()), // --help / --version already printed
    };

    let paths = args.with_env().resolve()?;
    eprintln!(
        "otzaria-mcp: db={} index={} lexical={} snapshot={}",
        paths.db.display(),
        paths
            .index
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(none)".into()),
        paths
            .lexical_db
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(none)".into()),
        paths.snapshot,
    );

    // Library holds the current DB+index snapshot and hot-reloads it when the
    // underlying files change (opening the index logs to stderr internally).
    let lib = Library::open(paths)?;
    {
        let snap = lib.current();
        eprintln!(
            "otzaria-mcp: schema_version={:?} data_version={}",
            snap.db.schema_version(),
            snap.fingerprint
        );
    }
    tokio::spawn(Arc::clone(&lib).watch());

    let server = OtzariaServer::new(lib);
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// Parse CLI args into `PathArgs`. Returns `Ok(None)` when help/version was printed.
fn parse_args() -> Result<Option<PathArgs>> {
    let mut args = PathArgs::default();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                eprint!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                eprintln!("otzaria-mcp {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--snapshot" => args.snapshot = true,
            "--db" => args.db = Some(PathBuf::from(next_value(&mut it, "--db")?)),
            "--index" => args.index = Some(PathBuf::from(next_value(&mut it, "--index")?)),
            "--lexical-db" => {
                args.lexical_db = Some(PathBuf::from(next_value(&mut it, "--lexical-db")?))
            }
            "--data-dir" => args.data_dir = Some(PathBuf::from(next_value(&mut it, "--data-dir")?)),
            other => anyhow::bail!("unknown argument: {other} (try --help)"),
        }
    }
    Ok(Some(args))
}

fn next_value(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<String> {
    it.next()
        .ok_or_else(|| anyhow::anyhow!("missing value for {flag}"))
}
