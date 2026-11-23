//! Unit tests over a schema-6-shaped in-memory fixture: text in `line_content`, a range
//! link with coverage, a suppressed side, anchors, an edition and an alternate TOC.

use crate::db::Db;
use rusqlite::Connection;

pub(crate) fn fixture_db() -> Db {
    fixture_db_with(|_| {})
}

pub(crate) fn fixture_db_with(setup: impl FnOnce(&Connection)) -> Db {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE category (id INTEGER PRIMARY KEY, parentId INTEGER, title TEXT NOT NULL, level INTEGER NOT NULL DEFAULT 0, orderIndex INTEGER NOT NULL DEFAULT 999, heShortDesc TEXT, heDesc TEXT);
         CREATE TABLE category_closure (ancestorId INTEGER NOT NULL, descendantId INTEGER NOT NULL);
         CREATE TABLE source (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         CREATE TABLE book (id INTEGER PRIMARY KEY, categoryId INTEGER NOT NULL, sourceId INTEGER NOT NULL, title TEXT NOT NULL, heRef TEXT, heShortDesc TEXT, heDesc TEXT, orderIndex INTEGER NOT NULL DEFAULT 999, totalLines INTEGER NOT NULL DEFAULT 0, isBaseBook INTEGER NOT NULL DEFAULT 0, hasTargumConnection INTEGER NOT NULL DEFAULT 0, hasReferenceConnection INTEGER NOT NULL DEFAULT 0, hasSourceConnection INTEGER NOT NULL DEFAULT 0, hasCommentaryConnection INTEGER NOT NULL DEFAULT 0, hasOtherConnection INTEGER NOT NULL DEFAULT 0, hasAltStructures INTEGER NOT NULL DEFAULT 0, hasTeamim INTEGER NOT NULL DEFAULT 0, hasNekudot INTEGER NOT NULL DEFAULT 0, dependenceType TEXT, collectiveTitleHe TEXT, collectiveTitleEn TEXT);
         CREATE TABLE author (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         CREATE TABLE book_author (bookId INTEGER NOT NULL, authorId INTEGER NOT NULL);
         CREATE TABLE topic (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         CREATE TABLE book_topic (bookId INTEGER NOT NULL, topicId INTEGER NOT NULL);
         CREATE TABLE book_acronym (bookId INTEGER NOT NULL, term TEXT NOT NULL);
         CREATE TABLE generation (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         CREATE TABLE book_generation (bookId INTEGER NOT NULL, generationId INTEGER NOT NULL);
         CREATE TABLE line (id INTEGER PRIMARY KEY, bookId INTEGER NOT NULL, lineIndex INTEGER NOT NULL, heRef TEXT, charCount INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE line_content (id INTEGER PRIMARY KEY, content TEXT NOT NULL);
         CREATE TABLE book_version (id INTEGER PRIMARY KEY, bookId INTEGER NOT NULL, versionTitle TEXT NOT NULL, heVersionTitle TEXT, versionSource TEXT, priority REAL, license TEXT, versionNotes TEXT, heVersionNotes TEXT, hasContent INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE version_line (versionId INTEGER NOT NULL, lineId INTEGER NOT NULL, content TEXT, charCount INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE connection_type (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         CREATE TABLE link (id INTEGER PRIMARY KEY, sourceBookId INTEGER NOT NULL, targetBookId INTEGER NOT NULL, sourceLineId INTEGER NOT NULL, targetLineId INTEGER NOT NULL, targetLineIndex INTEGER NOT NULL, targetBookOrderIndex INTEGER NOT NULL, connectionTypeId INTEGER NOT NULL, baseProvenance INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE link_range (linkId INTEGER NOT NULL, side INTEGER NOT NULL, endLineId INTEGER NOT NULL, endLineIndex INTEGER NOT NULL);
         CREATE TABLE link_coverage (lineId INTEGER NOT NULL, linkId INTEGER NOT NULL, side INTEGER NOT NULL);
         CREATE TABLE link_suppressed_side (linkId INTEGER NOT NULL, side INTEGER NOT NULL, reasonMask INTEGER NOT NULL);
         CREATE TABLE link_anchor (linkId INTEGER NOT NULL, side INTEGER NOT NULL DEFAULT 0, charStart INTEGER NOT NULL, charEnd INTEGER, label TEXT);
         CREATE TABLE default_commentator (bookId INTEGER NOT NULL, commentatorBookId INTEGER NOT NULL, position INTEGER NOT NULL);
         CREATE TABLE default_targum (bookId INTEGER NOT NULL, targumBookId INTEGER NOT NULL, position INTEGER NOT NULL);
         CREATE TABLE tocText (id INTEGER PRIMARY KEY, text TEXT NOT NULL);
         CREATE TABLE tocEntry (id INTEGER PRIMARY KEY, bookId INTEGER NOT NULL, parentId INTEGER, textId INTEGER NOT NULL, level INTEGER NOT NULL, lineId INTEGER, isLastChild INTEGER NOT NULL DEFAULT 0, hasChildren INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE alt_toc_structure (id INTEGER PRIMARY KEY, bookId INTEGER NOT NULL, key TEXT NOT NULL, title TEXT, heTitle TEXT);
         CREATE TABLE alt_toc_entry (id INTEGER PRIMARY KEY, structureId INTEGER NOT NULL, parentId INTEGER, textId INTEGER NOT NULL, level INTEGER NOT NULL, lineId INTEGER, isLastChild INTEGER NOT NULL DEFAULT 0, hasChildren INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE schema_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);

         INSERT INTO schema_meta VALUES ('db_version', '32'), ('db_schema_version', '6');
         INSERT INTO category VALUES (1, NULL, 'תנ״ך', 0, 1, 'קצר', 'ארוך'), (2, 1, 'תורה', 1, 1, NULL, NULL);
         INSERT INTO category_closure VALUES (1,1), (2,2), (1,2);
         INSERT INTO source VALUES (1, 'Sefaria');
         INSERT INTO book (id, categoryId, sourceId, title, heRef, orderIndex, totalLines, isBaseBook) VALUES (1, 2, 1, 'בסיס', 'בסיס', 1, 3, 1);
         INSERT INTO book (id, categoryId, sourceId, title, heRef, orderIndex, totalLines, isBaseBook, dependenceType, collectiveTitleHe) VALUES (2, 2, 1, 'מפרש', 'מפרש', 2, 2, 0, 'Commentary', 'מפרש');
         INSERT INTO book (id, categoryId, sourceId, title, heRef, orderIndex, totalLines) VALUES (3, 2, 1, 'מצטט', 'מצטט', 3, 1);
         INSERT INTO generation VALUES (1, 'ראשונים'), (2, 'חז\"ל');
         INSERT INTO book_generation VALUES (2, 1), (3, 2);
         INSERT INTO line VALUES (10, 1, 0, 'בסיס א', 0), (11, 1, 1, 'בסיס ב', 0), (12, 1, 2, 'בסיס ג', 0), (20, 2, 0, 'מפרש א', 0), (21, 2, 1, 'מפרש ב', 0), (30, 3, 0, 'מצטט א', 0);
         INSERT INTO line_content VALUES (10, '<b>בראשית</b> ברא'), (11, 'טקסט ב'), (12, 'טקסט ג'), (20, 'פירוש ראשון'), (21, 'המשך הפירוש'), (30, 'ציטוט');
         INSERT INTO book_version VALUES (5, 1, 'Plain', 'פשוט', NULL, 1.0, NULL, NULL, NULL, 1);
         INSERT INTO version_line VALUES (5, 10, 'נוסח אחר', 0);
         INSERT INTO connection_type VALUES (1, 'COMMENTARY'), (2, 'QUOTATION');
         INSERT INTO link VALUES (100, 1, 2, 10, 20, 0, 2, 1, 2);
         INSERT INTO link_range VALUES (100, 0, 12, 2), (100, 1, 21, 1);
         INSERT INTO link_coverage VALUES (11, 100, 0), (12, 100, 0), (21, 100, 1);
         INSERT INTO link_anchor VALUES (100, 0, 0, 7, NULL), (100, 0, 9, NULL, 'א');
         INSERT INTO link VALUES (200, 1, 3, 11, 30, 0, 3, 2, 0);
         INSERT INTO link_suppressed_side VALUES (200, 0, 1);
         INSERT INTO default_commentator VALUES (1, 2, 0);
         INSERT INTO tocText VALUES (1, 'פרק א'), (2, 'פרשה א');
         INSERT INTO tocEntry VALUES (1, 1, NULL, 1, 0, 10, 0, 0);
         INSERT INTO alt_toc_structure VALUES (7, 1, 'Parasha', 'Parasha', 'פרשה');
         INSERT INTO alt_toc_entry VALUES (8, 7, NULL, 2, 0, 11, 1, 0);",
    )
    .unwrap();
    setup(&conn);
    Db::from_connection(conn).unwrap()
}

#[test]
fn range_links_are_found_from_every_covered_line_once() {
    let db = fixture_db();
    // base line 1 is only covered by the range link that starts at line 0
    let out = db
        .get_links(1, Some((1, 1)), None, None, "outgoing", 10, 0)
        .unwrap();
    assert_eq!(out.len(), 1, "{out:?}");
    let l = &out[0];
    assert_eq!(
        (l.link_id, l.book_id, l.line_index, l.line_end_index),
        (100, 2, 0, Some(1))
    );
    assert_eq!(l.at_line_index, 1);
    assert!(
        l.anchors.is_empty(),
        "anchors belong to the anchored line only"
    );
    assert_eq!(l.snippet, "פירוש ראשון");

    let all = db
        .get_links(1, Some((0, 2)), None, None, "outgoing", 10, 0)
        .unwrap();
    assert_eq!(all.len(), 1, "a range link is listed once");
    assert_eq!(all[0].anchors.len(), 2);
}

#[test]
fn suppressed_sides_are_hidden_and_inverse_is_virtual_source() {
    let db = fixture_db();
    let types = vec!["QUOTATION".to_string()];
    let out = db
        .get_links(1, None, Some(&types), None, "outgoing", 10, 0)
        .unwrap();
    assert!(out.is_empty(), "side 0 of the quotation is suppressed");
    let back = db
        .get_links(3, None, None, None, "incoming", 10, 0)
        .unwrap();
    assert_eq!(back.len(), 1, "side 1 is not");

    // the commentary's second line is covered by the link (side 1)
    let src = db
        .get_links(2, Some((1, 1)), None, None, "incoming", 10, 0)
        .unwrap();
    assert_eq!(src.len(), 1);
    assert_eq!(
        (
            src[0].display_type.as_str(),
            src[0].book_id,
            src[0].line_end_index
        ),
        ("SOURCE", 1, Some(2))
    );
    let typed = vec!["SOURCE".to_string()];
    assert_eq!(
        db.get_links(2, None, Some(&typed), None, "incoming", 10, 0)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn commentaries_join_multi_line_comments_and_commentators_group_by_era() {
    let db = fixture_db();
    let c = db.get_commentaries(1, 2, 2, None, 10, 5).unwrap();
    assert_eq!(c.items.len(), 1);
    assert_eq!(c.items[0].text, "פירוש ראשון\nהמשך הפירוש");
    assert_eq!(c.items[0].on_line_index, 2);
    assert_eq!(c.default_commentators[0].book_id, 2);

    let list = db.list_commentators(1, None).unwrap();
    assert_eq!(list.commentator_count, 1);
    assert_eq!(list.eras[0].era.hebrew_name(), "ראשונים");
    assert_eq!(
        list.eras[0].commentators[0].types.get("COMMENTARY"),
        Some(&1)
    );
    assert_eq!(list.eras[0].commentators[0].default_position, Some(0));
    let ranged = db.list_commentators(1, Some((2, 2))).unwrap();
    assert_eq!(ranged.link_count, 1, "found through coverage");
}

#[test]
fn line_anchors_resolve_visible_text() {
    let db = fixture_db();
    let a = db.get_line_anchors(1, 0).unwrap().unwrap();
    assert_eq!(a.text, "בראשית ברא");
    assert_eq!(a.anchors[0].anchored_text.as_deref(), Some("בראשית "));
    assert!(a.anchors[1].context.as_deref().unwrap().contains("⟨א⟩"));
    assert!(db.get_line_anchors(1, 99).unwrap().is_none());
}

#[test]
fn text_versions_toc_categories_and_info() {
    let db = fixture_db();
    let t = db.get_text(1, 0, 2, true, None).unwrap().unwrap();
    assert_eq!(t.lines[0].content, "בראשית ברא");
    assert_eq!(t.next_line_index, Some(2));
    let v = db.get_text(1, 0, 2, true, Some("פשוט")).unwrap().unwrap();
    assert_eq!(v.lines[0].content, "נוסח אחר");
    assert_eq!(
        v.lines[1].content, "",
        "a line missing from the edition is empty"
    );

    let toc = db.get_book_toc(1, 10, Some("Parasha"), None).unwrap();
    assert_eq!(toc.alt_entries[0].line_index, Some(1));
    assert!(db.get_book_toc(1, 10, Some("Nope"), None).is_err());

    let view = db.browse_categories(None, Some("תנ\"ך/תורה"), 10).unwrap();
    assert_eq!(view.books.len(), 3);
    let roots = db.browse_categories(None, None, 10).unwrap();
    assert_eq!(roots.subcategories[0].book_count, 3);
    assert_eq!(db.category_facet(2).unwrap().as_deref(), Some("/תנ״ך/תורה"));

    let info = db.get_book_info(2).unwrap().unwrap();
    assert_eq!(info.dependence_type.as_deref(), Some("Commentary"));
    assert_eq!(info.era.hebrew_name(), "ראשונים");
    assert_eq!(info.category_path.len(), 2);
    assert_eq!(db.schema_version(), Some(32));
    assert_eq!(db.db_schema_version(), Some(6));

    let hits = db.search_books("מפ", None, 10).unwrap();
    assert_eq!(hits[0].book_id, 2);
}

#[test]
fn links_starting_on_the_line_come_before_links_covering_it() {
    let db = fixture_db();
    // A commentary that starts on base line 1, from a book later in catalogue order than
    // the range link (lines 0-2) that only covers line 1.
    db.lock()
        .conn
        .execute_batch(
            "PRAGMA query_only=OFF; INSERT INTO link VALUES (300, 1, 3, 11, 30, 0, 3, 1, 0); PRAGMA query_only=ON;",
        )
        .unwrap();
    let out = db
        .get_links(1, Some((1, 1)), None, None, "outgoing", 10, 0)
        .unwrap();
    let ids: Vec<i64> = out.iter().map(|l| l.link_id).collect();
    assert_eq!(ids, [300, 100], "{out:?}");
    let first = db
        .get_links(1, Some((1, 1)), None, None, "outgoing", 1, 0)
        .unwrap();
    assert_eq!(
        first[0].link_id, 300,
        "the covering link must not crowd it out"
    );
}

#[test]
fn range_link_is_deduplicated_before_link_pagination() {
    let db = fixture_db();
    {
        let inner = db.lock();
        inner
            .conn
            .execute_batch(
                "PRAGMA query_only=OFF;
             INSERT INTO link VALUES (101, 1, 2, 11, 21, 1, 2, 1, 0);
             PRAGMA query_only=ON;",
            )
            .unwrap();
    }

    let p1 = db
        .get_links(1, Some((0, 2)), None, None, "outgoing", 1, 0)
        .unwrap();
    let p2 = db
        .get_links(1, Some((0, 2)), None, None, "outgoing", 1, 1)
        .unwrap();
    assert_eq!(p1.len(), 1);
    assert_eq!(p1[0].link_id, 100);
    assert_eq!(p1[0].at_line_index, 0, "keep the first covered anchor");
    assert_eq!(p2.len(), 1, "the next distinct link must be on page two");
    assert_eq!(p2[0].link_id, 101);
}

#[test]
fn range_commentaries_are_deduplicated_before_limit() {
    let db = fixture_db();
    {
        let inner = db.lock();
        inner
            .conn
            .execute_batch(
                "PRAGMA query_only=OFF;
             INSERT INTO link VALUES (101, 1, 2, 11, 21, 1, 2, 1, 0);
             PRAGMA query_only=ON;",
            )
            .unwrap();
    }

    // Coverage duplicates used to consume the SQL limit before the second
    // commentary could be returned.
    let comments = db.get_commentaries(1, 0, 2, None, 2, 5).unwrap();
    assert_eq!(comments.items.len(), 2);
    assert_eq!(comments.items[0].target_line_index, 0);
    assert_eq!(comments.items[1].target_line_index, 1);
}

#[test]
fn both_directions_use_one_offset_over_a_stable_combined_list() {
    let db = fixture_db();
    {
        let inner = db.lock();
        inner
            .conn
            .execute_batch(
                "PRAGMA query_only=OFF;
             INSERT INTO link VALUES (300, 2, 3, 20, 30, 0, 3, 2, 0);
             INSERT INTO link VALUES (301, 2, 3, 21, 30, 0, 3, 2, 0);
             INSERT INTO link_range VALUES (300, 0, 21, 1);
             INSERT INTO link_coverage VALUES (21, 300, 0);
             PRAGMA query_only=ON;",
            )
            .unwrap();
    }

    let all = db
        .get_links(2, Some((0, 1)), None, None, "both", 10, 0)
        .unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].direction, "outgoing");
    assert_eq!(all[1].direction, "outgoing");
    assert_eq!(all[2].direction, "incoming");
    let p1 = db
        .get_links(2, Some((0, 1)), None, None, "both", 1, 0)
        .unwrap();
    let p2 = db
        .get_links(2, Some((0, 1)), None, None, "both", 1, 1)
        .unwrap();
    let p3 = db
        .get_links(2, Some((0, 1)), None, None, "both", 1, 2)
        .unwrap();
    let p4 = db
        .get_links(2, Some((0, 1)), None, None, "both", 1, 3)
        .unwrap();
    assert_eq!(p1.len(), 1);
    assert_eq!(p1[0].direction, "outgoing");
    assert_eq!(p2.len(), 1);
    assert_eq!(p2[0].direction, "outgoing");
    assert_eq!(p3.len(), 1, "the incoming link must be reachable");
    assert_eq!(p3[0].direction, "incoming");
    assert_eq!(p3[0].link_id, all[2].link_id);
    assert!(p4.is_empty());
}
