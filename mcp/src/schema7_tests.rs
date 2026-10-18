//! Schema-7 reading uses the same restoration as indexing, without shifting anchor coordinates.
use crate::db::Db;
use crate::fixture_tests::fixture_db_with;
use rusqlite::Connection;

fn with_notes(setup: impl FnOnce(&Connection)) -> Db {
    fixture_db_with(|conn| {
        conn.execute_batch(
            "CREATE TABLE line_note (lineId INTEGER, noteIndex INTEGER, charPos INTEGER, marker TEXT, body TEXT);\n             CREATE INDEX notes_by_line ON line_note(lineId, noteIndex);",
        ).unwrap();
        setup(conn);
    })
}

#[test]
fn schema7_notes_reach_ranges_commentaries_and_link_snippets() {
    let db = with_notes(|conn| {
        conn.execute_batch(
        "INSERT INTO line_note VALUES (20, 0, 5, NULL, 'גוף ראשון'), (21, 0, 4, NULL, 'גוף שני');"
    ).unwrap()
    });
    let comments = db.get_commentaries(1, 2, 2, None, 10, 5).unwrap();
    assert!(comments.items[0].text.contains("גוף ראשון"));
    assert!(comments.items[0].text.contains("גוף שני"));
    let links = db
        .get_links(1, Some((0, 2)), None, None, "outgoing", 10, 0)
        .unwrap();
    assert!(links[0].snippet.contains("גוף ראשון"));
    let legacy = db.legacy_link_fields(&[100]).unwrap();
    assert!(legacy[&100]["target_snippet"]
        .as_str()
        .unwrap()
        .contains("גוף ראשון"));
}

#[test]
fn schema7_notes_do_not_change_anchor_offsets_after_body() {
    let db = with_notes(|conn| {
        conn.execute_batch(
        "UPDATE line_content SET content = 'אב<sup class=\"footnote-marker\">א</sup>גד' WHERE id = 10;\n         INSERT INTO line_note VALUES (10, 0, 3, 'א', 'הערה ארוכה');\n         UPDATE link_anchor SET charStart = 3, charEnd = 5 WHERE linkId = 100 AND side = 0;"
    ).unwrap()
    });
    assert!(db.get_text(1, 0, 1, true, None).unwrap().unwrap().lines[0]
        .content
        .contains("הערה ארוכה"));
    let anchor = db.get_line_anchors(1, 0).unwrap().unwrap();
    assert_eq!(anchor.text, "אבאגד");
    assert_eq!(anchor.visible_length, 5);
    assert_eq!(anchor.anchors[0].anchored_text.as_deref(), Some("גד"));
}

#[test]
fn schema7_notes_only_restore_actual_base_rows_of_an_edition() {
    let db = with_notes(|conn| {
        conn.execute_batch(
        "UPDATE line SET heRef = NULL WHERE id = 10;\n         INSERT INTO line_note VALUES (10, 0, 0, NULL, 'כותרת בסיס'), (11, 0, 0, NULL, 'הערת בסיס'), (12, 0, 0, NULL, 'בסיס חסר');\n         INSERT INTO version_line VALUES (5, 11, '<i class=\"footnote\">הערת נוסח</i>נוסח אחר', 0);"
    ).unwrap()
    });
    let text = db.get_text(1, 0, 3, true, Some("Plain")).unwrap().unwrap();
    assert!(text.lines[0].content.contains("כותרת בסיס"));
    assert_eq!(text.lines[1].content, "הערת נוסחנוסח אחר");
    assert_eq!(text.lines[2].content, "");
}

#[test]
fn schema7_notes_restore_base_fallback_for_null_edition_content() {
    let db = with_notes(|conn| {
        conn.execute_batch(
        "UPDATE version_line SET content = NULL WHERE lineId = 10;\n         INSERT INTO line_note VALUES (10, 0, 0, NULL, 'גוף בסיס');"
    ).unwrap()
    });
    let text = db.get_text(1, 0, 1, true, Some("Plain")).unwrap().unwrap();
    assert!(text.lines[0].content.contains("גוף בסיס"));
}

#[test]
fn schema7_notes_support_compressed_content_and_note_order() {
    let db = with_notes(|conn| {
        let samples: Vec<Vec<u8>> = (0..200)
            .map(|i| format!("בראשית ברא אלהים את השמים ואת הארץ {i}").into_bytes())
            .collect();
        let dict = zstd::dict::from_samples(&samples, 4096).unwrap();
        conn.execute_batch("CREATE TABLE zstd_dict (id INTEGER PRIMARY KEY, dict BLOB);")
            .unwrap();
        conn.execute("INSERT INTO zstd_dict VALUES (1, ?1)", [&dict])
            .unwrap();
        let mut codec = zstd::bulk::Compressor::with_dictionary(3, &dict).unwrap();
        let frame = codec.compress("טקסט".as_bytes()).unwrap();
        conn.execute(
            "UPDATE line_content SET content = ?1 WHERE id = 10",
            [&frame],
        )
        .unwrap();
        // Deliberately inserted out of order; body is TEXT even with compressed lines.
        conn.execute_batch("INSERT INTO line_note VALUES (10, 1, 2, NULL, 'שני'), (10, 0, 2, NULL, 'ראשון'), (11, 0, 0, NULL, 'לא שייך');").unwrap();
        let edition = codec
            .compress("נוסח<i class=\"footnote\">משלו</i>".as_bytes())
            .unwrap();
        conn.execute("INSERT INTO version_line VALUES (5, 11, ?1, 0)", [&edition])
            .unwrap();
    });
    let text = db.get_text(1, 0, 1, false, None).unwrap().unwrap();
    assert_eq!(
        text.lines[0].content,
        "טק<i class=\"footnote\">ראשון</i><i class=\"footnote\">שני</i>סט"
    );
    assert_eq!(
        db.get_text(1, 1, 1, false, Some("Plain"))
            .unwrap()
            .unwrap()
            .lines[0]
            .content,
        "נוסח<i class=\"footnote\">משלו</i>"
    );
}

#[test]
fn schema7_notes_marker_and_block_boundary_parity() {
    let cases = [
        ("<p><b><sup class=\"footnote-marker\"></sup>המשך</b></p>", 0, Some(""), "<p><b><sup class=\"footnote-marker\"></sup><i class=\"footnote\">גוף</i>המשך</b></p>"),
        ("<sup class=\"footnote-marker\"><i class=\"footnote\">א</i></sup>המשך", 1, Some("א"), "<sup class=\"footnote-marker\"><i class=\"footnote\">א</i></sup><i class=\"footnote\">גוף</i>המשך"),
        ("<p>א</p><p>ב</p>", 1, None, "<p>א<i class=\"footnote\">גוף</i></p><p>ב</p>"),
        ("א<i class=\"footnote\">קיים</i>ב", 1, None, "א<i class=\"footnote\">קיים</i>ב"),
        ("😀&amp;ב", 3, None, "😀&amp;<i class=\"footnote\">גוף</i>ב"),
    ];
    for (html, pos, marker, expected) in cases {
        let db = with_notes(|conn| {
            conn.execute("UPDATE line_content SET content = ?1 WHERE id = 10", [html])
                .unwrap();
            conn.execute(
                "INSERT INTO line_note VALUES (10, 0, ?1, ?2, 'גוף')",
                rusqlite::params![pos, marker],
            )
            .unwrap();
        });
        assert_eq!(
            db.get_text(1, 0, 1, false, None).unwrap().unwrap().lines[0].content,
            expected,
            "{html}"
        );
    }
}

#[test]
fn schema7_notes_batch_more_than_sqlite_variable_limit_without_losing_rows() {
    let db = with_notes(|conn| {
        for id in 1000..2001 {
            conn.execute("INSERT INTO line VALUES (?1, 1, ?1, NULL, 0)", [id])
                .unwrap();
            conn.execute("INSERT INTO line_content VALUES (?1, 'טקסט')", [id])
                .unwrap();
            conn.execute("INSERT INTO line_note VALUES (?1, 0, 0, NULL, 'גוף')", [id])
                .unwrap();
        }
    });
    let mut inner = db.lock();
    let ids: Vec<_> = (1000..2001).chain([1000]).collect();
    let text = db.texts_by_line_id(&mut inner, &ids).unwrap();
    assert_eq!(text.len(), 1001);
    assert!(text.values().all(|v| v.contains("גוף")));
}

#[test]
fn schema7_notes_absent_or_partial_table_keeps_legacy_text() {
    for ddl in ["", "CREATE TABLE line_note(lineId INTEGER, body TEXT);"] {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE book(id INTEGER, title TEXT, totalLines INTEGER); CREATE TABLE line(id INTEGER, bookId INTEGER, lineIndex INTEGER, heRef TEXT, content TEXT); INSERT INTO book VALUES (1, 'ספר', 1); INSERT INTO line VALUES (1, 1, 0, 'ספר א', '<b>טקסט</b>');").unwrap();
        conn.execute_batch(ddl).unwrap();
        let db = Db::from_connection(conn).unwrap();
        assert_eq!(
            db.get_text(1, 0, 1, true, None).unwrap().unwrap().lines[0].content,
            "טקסט"
        );
    }
}

#[test]
fn schema7_notes_failed_read_releases_snapshot() {
    let db = with_notes(|_| {});
    {
        let inner = db.lock();
        inner
            .conn
            .execute_batch("PRAGMA query_only=OFF; DROP TABLE line_note; PRAGMA query_only=ON;")
            .unwrap();
    }
    assert!(db.get_text(1, 0, 1, false, None).is_err());
    assert!(db.lock().conn.is_autocommit());
    let mut inner = db.lock();
    assert!(db.lines_in_range(&mut inner, 1, 0, 1).is_err());
    assert!(inner.conn.is_autocommit());
    assert!(db.texts_by_line_id(&mut inner, &[10]).is_err());
    assert!(inner.conn.is_autocommit());
}
