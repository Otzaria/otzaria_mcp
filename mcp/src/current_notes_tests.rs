//! Current note layout and compressed bodies must use the canonical renderer,
//! while get_text keeps supporting the legacy marker/TEXT layout.
use crate::db::Db;
use crate::fixture_tests::fixture_db_with;
use rusqlite::{params, Connection};

fn current_notes(setup: impl FnOnce(&Connection)) -> Db {
    fixture_db_with(|conn| {
        conn.execute_batch(
            "CREATE TABLE line_note(lineId INTEGER, noteIndex INTEGER, charPos INTEGER, hasMarker INTEGER, body BLOB);",
        ).unwrap();
        setup(conn);
    })
}

fn dictionary(conn: &Connection) -> Vec<u8> {
    let samples: Vec<_> = (0..200)
        .map(|i| format!("טקסט לדוגמה לצורך בדיקת דחיסה וחיפוש במסד הנתונים {i}").into_bytes())
        .collect();
    let dict = zstd::dict::from_samples(&samples, 4096).unwrap();
    conn.execute_batch("CREATE TABLE zstd_dict(id INTEGER PRIMARY KEY, dict BLOB);")
        .unwrap();
    conn.execute("INSERT INTO zstd_dict VALUES(1,?1)", [&dict])
        .unwrap();
    dict
}

#[test]
fn numeric_marker_and_compressed_notes_keep_order_and_visible_offsets() {
    let db = current_notes(|conn| {
        let dict = dictionary(conn);
        let mut compressor = zstd::bulk::Compressor::with_dictionary(3, &dict).unwrap();
        let frame = compressor.compress("גוף דחוס".as_bytes()).unwrap();
        conn.execute_batch(
            "UPDATE line_content SET content='אב<sup class=\"footnote-marker\">א</sup>גד' WHERE id=10;
             UPDATE link_anchor SET charStart=3, charEnd=5 WHERE linkId=100 AND side=0;
             INSERT INTO line_note VALUES(10,1,3,0,'שני');",
        ).unwrap();
        conn.execute("INSERT INTO line_note VALUES(10,0,3,1,?1)", [frame])
            .unwrap();
    });
    assert!(db.caps.line_notes && db.caps.note_has_marker);
    let text = db.get_text(1, 0, 1, false, None).unwrap().unwrap();
    assert_eq!(text.lines[0].content,
        "אב<sup class=\"footnote-marker\">א</sup><i class=\"footnote\">גוף דחוס</i><i class=\"footnote\">שני</i>גד");
    let anchor = db.get_line_anchors(1, 0).unwrap().unwrap();
    assert_eq!(anchor.text, "אבאגד");
    assert_eq!(anchor.anchors[0].anchored_text.as_deref(), Some("גד"));
}

#[test]
fn compressed_notes_reach_range_and_link_responses() {
    let db = current_notes(|conn| {
        let dict = dictionary(conn);
        let mut compressor = zstd::bulk::Compressor::with_dictionary(3, &dict).unwrap();
        for (line_id, body) in [(20, "גוף ראשון"), (21, "גוף שני")] {
            let frame = compressor.compress(body.as_bytes()).unwrap();
            conn.execute(
                "INSERT INTO line_note VALUES(?1,0,0,0,?2)",
                params![line_id, frame],
            )
            .unwrap();
        }
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
fn current_marker_takes_precedence_over_legacy_column() {
    let db = current_notes(|conn| {
        conn.execute_batch(
            "ALTER TABLE line_note ADD COLUMN marker TEXT;
             UPDATE line_content SET content='אבגד' WHERE id=10;
             INSERT INTO line_note VALUES(10,0,2,0,'גוף','א');",
        )
        .unwrap();
    });
    assert_eq!(
        db.get_text(1, 0, 1, false, None).unwrap().unwrap().lines[0].content,
        "אב<i class=\"footnote\">גוף</i>גד"
    );
}

#[test]
fn malformed_current_marker_errors_and_releases_transaction() {
    for flag in ["2", "NULL", "'bad'"] {
        let db = current_notes(|conn| {
            conn.execute_batch(&format!(
                "INSERT INTO line_note VALUES(10,0,0,{flag},'body');"
            ))
            .unwrap();
        });
        let error = db.get_text(1, 0, 1, false, None).unwrap_err().to_string();
        assert!(error.contains("hasMarker"), "{error}");
        assert!(db.lock().conn.is_autocommit());
    }
}

#[test]
fn malformed_compressed_or_utf8_body_is_reported_not_dropped() {
    for invalid_utf8 in [false, true] {
        let db = current_notes(|conn| {
            let dict = dictionary(conn);
            let body = if invalid_utf8 {
                zstd::bulk::Compressor::with_dictionary(3, &dict)
                    .unwrap()
                    .compress(&[0xff, 0xfe])
                    .unwrap()
            } else {
                vec![0, 1, 2, 3]
            };
            conn.execute("INSERT INTO line_note VALUES(10,0,0,0,?1)", [body])
                .unwrap();
        });
        let error = db.get_text(1, 0, 1, false, None).unwrap_err();
        let error = format!("{error:#}");
        assert!(
            error.contains("line 10 note 0 cannot be decoded"),
            "{error}"
        );
        assert!(
            error.contains(if invalid_utf8 { "UTF-8" } else { "zstd" }),
            "{error}"
        );
        assert!(db.lock().conn.is_autocommit());
    }
    let db = current_notes(|conn| {
        conn.execute_batch("INSERT INTO line_note VALUES(10,0,0,0,CAST(X'FFFE' AS TEXT));")
            .unwrap();
    });
    assert!(format!("{:#}", db.get_text(1, 0, 1, false, None).unwrap_err()).contains("UTF-8"));
}

#[test]
fn null_body_and_bad_index_follow_source_skip_rules() {
    let db = current_notes(|conn| {
        conn.execute_batch(
            "INSERT INTO line_note VALUES(10,0,0,2,NULL),(10,'bad',0,0,'ignored'),(10,1,0,0,'visible');",
        ).unwrap();
    });
    let text = db.get_text(1, 0, 1, true, None).unwrap().unwrap();
    assert!(text.lines[0].content.starts_with("visible"));
    assert!(!text.lines[0].content.contains("ignored"));
}
