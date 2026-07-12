//! Golden-corpus + bridge tests against a REAL Otzaria dataset.
//!
//! These pass as a no-op when no dataset is found, so `cargo test` is green on a
//! machine without the library. To actually exercise them, point at the data:
//!   OTZARIA_DATA_DIR=/path/to/otzaria_latest cargo test -p otzaria-mcp -- --nocapture

use otzaria_mcp::db::Db;
use otzaria_mcp::paths::{PathArgs, Paths};
use otzaria_mcp::reload::Library;
use otzaria_mcp::search::Mode;

fn resolve_paths() -> Option<Paths> {
    PathArgs::default().with_env().resolve().ok()
}

/// resolve_ref must return the right (book_id, line_index) across Tanakh, Mishnah,
/// Talmud (daf notation), Halacha and abbreviations. These are golden values pinned
/// to the current library; update them if the dataset content version changes.
#[test]
fn resolve_ref_golden_corpus() {
    let Some(paths) = resolve_paths() else {
        eprintln!("skip resolve_ref_golden_corpus: no OTZARIA dataset found");
        return;
    };
    let db = Db::open(&paths).expect("open seforim.db");

    // (reference, expected_book_id, expected_line_index)
    let corpus: &[(&str, i64, i64)] = &[
        ("בראשית א א", 1, 2),
        ("שמות כ ב", 2, 541),
        ("משנה ברכות א א", 40, 2),
        ("ברכות ב:", 103, 17),
        ("שולחן ערוך אורח חיים א א", 382, 3),
        ("שו\"ע או\"ח א ב", 382, 4),
        ("תהילים קיט א", 28, 2143),
    ];

    for (reference, expected_book, expected_line) in corpus {
        let cands = db.resolve_ref(reference, 5).expect("resolve_ref");
        assert!(!cands.is_empty(), "no candidate for {reference:?}");
        let top = &cands[0];
        assert_ne!(
            top.match_kind, "fallback",
            "{reference:?} only matched via LIKE fallback"
        );
        assert_eq!(
            top.book_id, *expected_book,
            "{reference:?} resolved to the wrong book"
        );
        assert_eq!(
            top.line_index, *expected_line,
            "{reference:?} resolved to the wrong line"
        );
    }
}

/// A full-text hit must carry a canonical (book_id, line_index) that get_text can
/// resolve to the same line — the core Search→DB bridge.
#[test]
fn full_text_search_to_get_text_bridge() {
    let Some(paths) = resolve_paths() else {
        eprintln!("skip bridge test: no OTZARIA dataset found");
        return;
    };
    let lib = Library::open(paths).expect("open library");
    let snap = lib.current();
    let Some(search) = &snap.search else {
        eprintln!("skip bridge test: no Tantivy index");
        return;
    };

    let resp = search
        .search(
            "ואהבת לרעך כמוך".to_string(),
            Mode::Advanced,
            5,
            0,
            None,
            "catalogue",
        )
        .expect("search");
    assert!(resp.total > 0, "expected full-text hits");

    let hit = resp
        .hits
        .iter()
        .find(|h| h.book_id.is_some() && h.line_index.is_some())
        .expect("at least one text (non-PDF) hit");
    let bid = hit.book_id.unwrap();
    let li = hit.line_index.unwrap();

    let text = snap
        .db
        .get_text(bid, li, 1, true)
        .expect("get_text")
        .expect("book_id from a hit must exist");
    assert_eq!(text.lines.len(), 1);
    assert_eq!(
        text.lines[0].line_index, li,
        "segment must equal line_index"
    );
}
