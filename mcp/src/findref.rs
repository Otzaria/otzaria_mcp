//! Find-ref across the library, personal books and attached databases — the
//! implementation behind the `resolve_ref` tool.

use anyhow::Result;

use crate::db::Db;
use crate::refs::{rank_candidates, RefCandidate, ResolveOptions};
use crate::secondary::{SecondarySet, Source};

/// What a find-ref call asks for besides the reference itself.
#[derive(Debug, Clone)]
pub struct FindRefRequest {
    pub limit: i64,
    /// `sources` as passed by the client ("library", "user", "attached[:slug]", "all").
    pub filters: Vec<String>,
    /// For the library; secondary databases never take the heRef fallback.
    pub opts: ResolveOptions,
}

impl FindRefRequest {
    pub fn new(limit: Option<u32>, sources: Option<Vec<String>>) -> FindRefRequest {
        FindRefRequest {
            limit: limit.unwrap_or(10).clamp(1, 50) as i64,
            filters: sources
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| vec!["library".to_string()]),
            opts: ResolveOptions::default(),
        }
    }

    fn wants(&self, src: &Source) -> bool {
        self.filters.iter().any(|f| f.trim() == "all")
            || self.filters.iter().any(|f| src.matches_filter(f))
    }
}

/// One reference, every source asked for, one ranking (see `refs::rank_candidates`).
pub fn resolve(
    db: &Db,
    secondary: &SecondarySet,
    reference: &str,
    req: &FindRefRequest,
) -> Result<Vec<RefCandidate>> {
    let mut out: Vec<RefCandidate> = Vec::new();
    if req.wants(&Source::Library) {
        out.extend(db.resolve_ref_with(reference, req.limit, req.opts)?);
    }
    let sec_opts = ResolveOptions {
        heref_fallback: false,
        ..req.opts
    };
    for sec in &secondary.dbs {
        if !req.wants(&sec.spec.source) {
            continue;
        }
        let Some(sdb) = sec.db() else { continue };
        // one broken database must not sink the lookup
        match sdb.resolve_ref_with(reference, req.limit, sec_opts) {
            Ok(cands) => out.extend(cands.into_iter().map(|mut c| {
                c.set_source(&sec.spec.source);
                c
            })),
            Err(e) => eprintln!(
                "otzaria-mcp: resolve_ref in {} failed: {e:#}",
                sec.spec.path.display()
            ),
        }
    }
    let mut out = rank_candidates(reference, out);
    out.truncate(req.limit as usize);
    Ok(out)
}
