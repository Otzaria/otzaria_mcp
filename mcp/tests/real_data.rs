//! Integration tests against a REAL Otzaria `seforim.db` (library v32, schema 6).
//!
//! They pass as a no-op when no database is configured, so `cargo test` is green on a
//! machine (or CI) without the library. To run them, point at a seforim.db copy:
//!   OTZARIA_TEST_DB=/path/to/seforim.db cargo test -p otzaria-mcp -- --nocapture
//! (`OTZARIA_DB` is accepted too). Nothing else is probed — in particular not the app's
//! own data folders.
//!
//! Full-text search needs an index built by the same engine. Unless `OTZARIA_TEST_INDEX`
//! names one, the search tests build a tiny index in a temp folder from three v32 books,
//! exactly the way the app indexes official books (`TextStorage::LibraryDb`: line text is
//! read back from seforim.db through the engine's line source at search time).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use otzaria_mcp::db::{Db, Era};
use otzaria_mcp::paths::Paths;
use otzaria_mcp::reload::Library;
use otzaria_mcp::search::{Mode, Search, SearchHitSource, SearchRequest};
use search_engine::api::search_engine::{configure_line_source, SearchEngine, TextStorage};

fn db_path() -> Option<PathBuf> {
    let p = std::env::var_os("OTZARIA_TEST_DB").or_else(|| std::env::var_os("OTZARIA_DB"))?;
    let p = PathBuf::from(p);
    p.is_file().then_some(p)
}

fn paths(db: PathBuf, index: Option<PathBuf>) -> Paths {
    Paths {
        db,
        index,
        lexical_db: None,
        dictionaries_dir: None,
        snapshot: false,
    }
}

fn open_db() -> Option<Db> {
    let Some(p) = db_path() else {
        eprintln!("skip: set OTZARIA_TEST_DB to a seforim.db to run the real-data tests");
        return None;
    };
    Some(Db::open(&paths(p, None)).expect("open seforim.db"))
}

#[test]
fn v32_schema_and_capabilities() {
    let Some(db) = open_db() else { return };
    let version = db.schema_version().expect("schema_meta.db_version");
    assert!(
        version >= 32,
        "expected a library of db_version >= 32, got {version}"
    );
    assert!(db.db_schema_version().unwrap_or(0) >= 6);
    let c = &db.caps;
    assert!(
        c.split_content && c.compressed,
        "schema 6 keeps zstd text in line_content"
    );
    assert!(c.line_ref && c.line_dh && c.link_anchor && c.link_coverage && c.link_suppressed_side);
    let stats = db.library_stats().unwrap();
    assert!(stats.books > 7000, "{stats:?}");
    assert!(stats.connection_types.iter().any(|t| t == "LINKER"));
}

#[test]
fn get_text_decodes_compressed_lines_and_versions() {
    let Some(db) = open_db() else { return };
    let t = db.get_text(1, 2, 2, true, None).unwrap().expect("Genesis");
    assert_eq!(t.lines[0].line_index, 2);
    assert_eq!(t.lines[0].he_ref.as_deref(), Some("בראשית, א, א"));
    let plain = otzaria_mcp::refs::normalize_ref(&t.lines[0].content);
    assert!(
        plain.contains("בראשית ברא אלהים"),
        "{:?}",
        t.lines[0].content
    );
    assert_eq!(t.next_line_index, Some(4));
    assert!(t.total_lines > 1500);

    // An alternate edition stored in version_line.
    let info = db.get_book_info(1).unwrap().unwrap();
    let v = info
        .versions
        .iter()
        .find(|v| v.has_text)
        .expect("Genesis has text versions");
    let vt = db
        .get_text(1, 2, 1, true, Some(&v.version_title))
        .unwrap()
        .unwrap();
    assert!(!vt.lines[0].content.is_empty());
    assert!(db.get_text(1, 2, 1, true, Some("no such version")).is_err());
}

#[test]
fn book_info_has_v32_metadata() {
    let Some(db) = open_db() else { return };
    let rashi = db.get_book_info(3695).unwrap().expect("רש\"י על בראשית");
    assert_eq!(rashi.dependence_type.as_deref(), Some("commentary"));
    assert_eq!(rashi.era, Era::Rishonim);
    assert!(rashi.base_texts.iter().any(|b| b.book_id == 1) || !rashi.authors.is_empty());
    let genesis = db.get_book_info(1).unwrap().unwrap();
    assert_eq!(
        genesis.category_path.first().map(|c| c.title.as_str()),
        Some("תנ״ך")
    );
    assert!(!genesis.default_commentators.is_empty());
    assert!(!genesis.pub_dates.is_empty() || !genesis.versions.is_empty());
    assert!(genesis.alt_structures.iter().any(|a| a.key == "Parasha"));
}

/// resolve_ref must return the right (book_id, line_index) across Tanakh, Mishnah,
/// Talmud (daf notation), Halacha, abbreviations, locator words and parasha headings.
/// Golden values pinned to library v32.
#[test]
fn resolve_ref_golden_corpus_v32() {
    let Some(db) = open_db() else { return };
    let corpus: &[(&str, i64, i64)] = &[
        ("בראשית א א", 1, 2),
        ("בְּרֵאשִׁית א, א", 1, 2),
        ("בראשית פרק א פסוק ב", 1, 3),
        ("בראשית,א,א-ג", 1, 2),
        ("בראשית,א:א", 1, 2),
        ("שמות כ ב", 2, 541),
        ("משנה ברכות א א", 40, 2),
        ("ברכות ב.", 103, 2),
        ("ברכות ב:", 103, 17),
        ("ברכות ב ע\"א", 103, 2),
        ("ברכות ב., ב", 103, 3),
        ("ברכות,ב.-ג.", 103, 2),
        ("שולחן ערוך אורח חיים א א", 382, 4),
        ("שו\"ע או\"ח א ב", 382, 5),
        ("תהילים קיט א", 28, 2143),
        ("תהילים ע\"א א", 28, 1160),
        ("תהלים ע\"ב א", 28, 1185),
        ("תהילים פרק ע\"א פסוק א", 28, 1160),
        ("בראשית נח", 1, 153),
    ];
    for (reference, book, line) in corpus {
        let cands = db.resolve_ref(reference, 5).expect("resolve_ref");
        assert!(!cands.is_empty(), "no candidate for {reference:?}");
        let top = &cands[0];
        assert_ne!(
            top.match_kind, "fallback",
            "{reference:?} only matched via LIKE fallback"
        );
        assert_eq!(
            (top.book_id, top.line_index),
            (*book, *line),
            "{reference:?} → {top:?}"
        );
        assert!(!top.snippet.is_empty(), "{reference:?} has no snippet");
    }
}

#[test]
fn lookup_title_resolves_acronyms() {
    let Some(db) = open_db() else { return };
    let m = db.lookup_title("שו\"ע או\"ח", 10).unwrap();
    assert!(m.iter().any(|b| b.book_id == 382), "{m:?}");
    let m = db.lookup_title("ספר בראשית", 5).unwrap();
    assert_eq!(m[0].book_id, 1);
}

#[test]
fn links_commentaries_and_anchors_follow_the_app_model() {
    let Some(db) = open_db() else { return };
    // Genesis 1:1 — Rashi (3695) is a text-dependent commentary stored base → dependant.
    let c = db.get_commentaries(1, 2, 2, None, 200, 3).unwrap();
    assert!(c
        .items
        .iter()
        .any(|i| i.commentator_book_id == 3695 && !i.text.is_empty()));
    assert!(!c.default_commentators.is_empty());

    let list = db.list_commentators(1, Some((2, 2))).unwrap();
    assert!(list.commentator_count > 5, "{}", list.commentator_count);
    let orders: Vec<u8> = list.eras.iter().map(|g| g.era_order).collect();
    assert!(
        orders.windows(2).all(|w| w[0] < w[1]),
        "eras ordered: {orders:?}"
    );
    assert!(list
        .eras
        .iter()
        .any(|g| g.commentators.iter().any(|c| c.book_id == 3695)));

    let out = db
        .get_links(1, Some((2, 2)), None, None, "outgoing", 200, 0)
        .unwrap();
    assert!(out
        .iter()
        .any(|l| l.connection_type == "COMMENTARY" && l.book_id == 3695));
    assert!(out
        .iter()
        .all(|l| l.direction == "outgoing" && l.at_line_index == 2));

    // From Rashi's side the same link is the virtual SOURCE pointing back at Genesis.
    let rashi_line = out.iter().find(|l| l.book_id == 3695).unwrap().line_index;
    let back = db
        .get_links(
            3695,
            Some((rashi_line, rashi_line)),
            None,
            Some(&[1]),
            "incoming",
            50,
            0,
        )
        .unwrap();
    assert!(back
        .iter()
        .any(|l| l.display_type == "SOURCE" && l.book_id == 1 && l.line_index == 2));

    // Type filter, incl. the automatic LINKER links.
    let types = vec!["COMMENTARY".to_string()];
    let only = db
        .get_links(1, Some((2, 2)), Some(&types), None, "outgoing", 50, 0)
        .unwrap();
    assert!(only.iter().all(|l| l.connection_type == "COMMENTARY"));
    // LINKER links are stored from the citing book; asked for explicitly they are also
    // returned from the cited side (Genesis 1:1 is cited thousands of times).
    let linker = vec!["LINKER".to_string()];
    let cited = db
        .get_links(1, Some((2, 2)), Some(&linker), None, "incoming", 20, 0)
        .unwrap();
    assert_eq!(cited.len(), 20);
    assert!(cited
        .iter()
        .all(|l| l.connection_type == "LINKER" && l.direction == "incoming"));

    // Shulchan Aruch OC 1:1 has printed commentary letters as point anchors.
    let a = db.get_line_anchors(382, 4).unwrap().expect("line exists");
    assert!(a
        .anchors
        .iter()
        .any(|x| x.label.as_deref() == Some("א") && x.context.is_some()));
    assert!(a
        .anchors
        .iter()
        .all(|x| x.char_start as usize <= a.visible_length));
}

#[test]
fn categories_and_toc() {
    let Some(db) = open_db() else { return };
    let roots = db.browse_categories(None, None, 10).unwrap();
    assert!(roots
        .subcategories
        .iter()
        .any(|c| c.title == "תנ״ך" && c.book_count > 20));
    let torah = db.browse_categories(None, Some("תנ\"ך/תורה"), 10).unwrap();
    assert!(torah.books.iter().any(|b| b.book_id == 1));
    assert_eq!(db.category_facet(2).unwrap().as_deref(), Some("/תנ״ך/תורה"));

    let toc = db.get_book_toc(1, 10, Some("Parasha"), None).unwrap();
    assert!(toc
        .alt_entries
        .iter()
        .any(|e| e.text == "נח" && e.line_index == Some(153)));
}

// ── full-text search over a tiny index built from v32 books ─────────────────

const INDEXED_BOOKS: &[i64] = &[1, 103, 382];

/// Build (once per test process) a tiny schema-5 index the way the app indexes official
/// books, or use `OTZARIA_TEST_INDEX`. Returns the index dir.
fn test_index(db_path: &Path, db: &Db) -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        if let Some(p) = std::env::var_os("OTZARIA_TEST_INDEX") {
            return PathBuf::from(p);
        }
        // Under cargo's per-target scratch dir, rebuilt on every run (never the app's index).
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("v32-test-index");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        configure_line_source(db_path.to_string_lossy().into_owned()).unwrap();
        let mut engine = SearchEngine::new(dir.to_str().unwrap());
        for (order, &book_id) in INDEXED_BOOKS.iter().enumerate() {
            let info = db.get_book_info(book_id).unwrap().unwrap();
            let t = db
                .get_text(book_id, 0, info.total_lines, false, None)
                .unwrap()
                .unwrap();
            let text = t
                .lines
                .iter()
                .map(|l| l.content.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let topics = format!(
                "{}/id:{book_id}",
                db.category_facet(info.category_id).unwrap().unwrap()
            );
            let mut extra = vec![format!("/era/{}", info.era.hebrew_name())];
            if info.is_base_book {
                extra.push("/base".into());
            }
            let added = engine
                .add_text_book(
                    info.title.clone(),
                    topics,
                    format!("id:{book_id}"),
                    order as u32,
                    info.era.order() as u32,
                    text,
                    Some(extra),
                    TextStorage::LibraryDb,
                    None,
                )
                .unwrap();
            assert_eq!(added as i64, info.total_lines, "one document per line");
        }
        engine.commit().unwrap();
        drop(engine);
        dir
    })
    .clone()
}

fn open_search() -> Option<(Db, Search)> {
    let p = db_path()?;
    let db = Db::open(&paths(p.clone(), None)).unwrap();
    let index = test_index(&p, &db);
    let search = Search::open(&index, None, &p, None).expect("open the test index read-only");
    Some((db, search))
}

#[test]
fn full_text_search_maps_hits_to_lines_and_reads_text_from_the_db() {
    let Some((db, search)) = open_search() else {
        eprintln!("skip: no OTZARIA_TEST_DB");
        return;
    };
    assert!(
        search.info.found_schema_version.unwrap_or(0) >= 5
            || search.info.status.contains("compatible")
    );

    let mut req = SearchRequest::new("בראשית ברא אלהים", Mode::Exact);
    req.limit = 5;
    let resp = search.search(&req).unwrap();
    assert!(resp.total >= 1, "{resp:?}");
    let hit = resp
        .hits
        .iter()
        .find(|h| h.book_id == Some(1))
        .expect("Genesis hit");
    assert_eq!(hit.source, SearchHitSource::Official);
    assert_eq!(hit.line_index, Some(2));
    assert_eq!(
        hit.text_status, "ok",
        "line read back from the compressed db"
    );
    assert!(hit.snippet.contains('«'), "highlighted: {}", hit.snippet);

    // Bridge: the hit's (book_id, line_index) is a get_text address.
    let t = db.get_text(1, 2, 1, true, None).unwrap().unwrap();
    assert_eq!(t.lines[0].line_index, 2);

    // Facets: restrict to a category / a book / an era.
    let mut req = SearchRequest::new("אמר רבי", Mode::Advanced);
    req.facets = vec![db.category_facet(13).unwrap().unwrap()]; // סדר זרעים (Berakhot)
    let r = search.search(&req).unwrap();
    assert!(r.total > 0 && r.hits.iter().all(|h| h.book_id == Some(103)));
    req.facets = vec!["/era/תורה שבכתב".into()];
    assert_eq!(
        search.search(&req).unwrap().total,
        0,
        "Genesis has no אמר רבי"
    );

    // Per-category and per-book counts.
    let req = SearchRequest::new("ברכה", Mode::Advanced);
    let counts: Vec<(String, u64)> = search
        .facet_counts(&req, "/")
        .unwrap()
        .into_iter()
        .map(|c| (c.path, c.count))
        .collect();
    assert!(
        counts.iter().any(|(p, n)| p == "/תלמוד בבלי" && *n > 0),
        "{counts:?}"
    );
    let by_book = search.count_by_book(&req).unwrap();
    assert!(by_book.get("id:103").copied().unwrap_or(0) > 0);

    // Advanced word options (prefixes) widen the match; fuzzy tolerates a typo.
    let plain = search
        .search(&SearchRequest::new("ברא", Mode::Advanced))
        .unwrap()
        .total;
    let mut pre = SearchRequest::new("ברא", Mode::Advanced);
    pre.options.prefixes = true;
    assert!(search.search(&pre).unwrap().total >= plain);
    let fuzzy = search
        .search(&SearchRequest::new("בראשיט", Mode::Fuzzy))
        .unwrap();
    assert!(fuzzy.total > 0);

    // Paging and grouping.
    let mut page = SearchRequest::new("אלהים", Mode::Advanced);
    page.limit = 3;
    let p1 = search.search(&page).unwrap();
    page.offset = 3;
    let p2 = search.search(&page).unwrap();
    assert_eq!(p1.total, p2.total);
    assert_ne!(p1.hits[0].line_index, p2.hits[0].line_index);
    page.grouping = "same_section".into();
    assert!(search.search(&page).unwrap().group_count.is_some());
}

#[test]
fn library_opens_with_the_index_and_reports_it() {
    let Some(p) = db_path() else { return };
    let db = Db::open(&paths(p.clone(), None)).unwrap();
    let index = test_index(&p, &db);
    let lib = Library::open(paths(p, Some(index))).expect("open library");
    let snap = lib.current();
    assert!(snap.search.is_some(), "{:?}", snap.search_error);
    assert!(snap.search.as_ref().unwrap().doc_count() > 1000);
}
