# otzaria-mcp

שרת **MCP** (Model Context Protocol) שחושף את ספריית **אוצריא** ל-AI — לחיפוש, ניווט,
פענוח הפניות, קישורים ומפרשים — **אופליין לחלוטין וקריאה-בלבד**. בהשראת
[zayit-mcp](https://github.com/Sivan22/zayit-mcp), אך בנוי מעל שני עולמות הנתונים של אוצריא:

- **`seforim.db`** (SQLite, סכימת v15) — ספרים, שורות, הפניות (heRef), עץ קטגוריות,
  ראשי-תיבות, קישורים (15 סוגים) ומטא-דאטה.
- **אינדקס Tantivy** (Rust) — חיפוש טקסט מלא עם מורפולוגיה עברית. המנוע המקורי של
  אוצריא (`otzaria_search_engine` 0.6.7) עבר **vendoring** ומשמש ישירות, כך שהחיפוש
  נאמן במדויק לזה שבאפליקציה (regex-phrase, ניקוד/טעמים, facets, PDF) — בלי שכתוב.

## התקנה למשתמש הסופי

יש שני מסלולים. **בשני המקרים** צריך רק שני דברים חוץ מה-MCP:

1. **אוצריא מותקנת** על אותה מכונה — ה-MCP קורא את נתוניה: `seforim.db`, אינדקס
   Tantivy, ואופציונלית `lexical.db` (למצב fuzzy). לא צריך את קוד המקור של אוצריא, רק
   שהנתונים קיימים (למשל תיקיית `otzaria_latest`).
2. **לקוח MCP** (כמו Claude Desktop) עם רישום ה-MCP ב-config (ראו למטה).

### מסלול א' — בינארי מוכן (מומלץ; **בלי Rust**)

הורידו את הבינארי לפלטפורמה שלכם מעמוד ה-Releases (נבנה אוטומטית ע"י ה-CI: לינוקס
x86_64/aarch64, macOS Intel/Apple-Silicon, Windows x86_64). לאחר ההורדה:

```bash
chmod +x otzaria-mcp-macos-aarch64          # הרשאת הרצה (Linux/macOS)
xattr -d com.apple.quarantine otzaria-mcp-* 2>/dev/null || true   # macOS: הסרת הסגר Gatekeeper (בינארי לא חתום)
```

הצביעו על הבינארי ב-config של הלקוח (עם `--data-dir`). **אין צורך ב-Rust, cargo, או בנייה.**

### מסלול ב' — בנייה מהמקור

צריך **Rust** (toolchain יציב, `cargo`) ומהדר C (מגיע עם Xcode CLT / gcc / MSVC — עבור
ה-SQLite המובנה). ואז:

```bash
git clone <repo> && cd otzaria_mcp
cargo build --release      # הבינארי: target/release/otzaria-mcp
```

הקומפילציה הראשונה כוללת את tantivy ולוקחת מספר דקות. מנוע החיפוש כלול בריפו תחת
[`vendor/otzaria_search_engine`](vendor/otzaria_search_engine) — אין submodule ואין repo חיצוני,
`git clone` רגיל מספיק.

## הרצה (stdio)

```bash
./run-stdio.sh --data-dir /path/to/otzaria_latest
```

`run-stdio.sh` בונה release בפעם הראשונה ואז מריץ. הפרוטוקול (JSON-RPC) עובר על
stdin/stdout; לוגים על stderr.

### גילוי נתיבים (סדר עדיפויות)

1. דגלים: `--db <path>` · `--index <path>` · `--lexical-db <path>`
2. משתני סביבה: `OTZARIA_DB` · `OTZARIA_INDEX` · `OTZARIA_LEXICAL_DB`
3. `--data-dir <root>` / `OTZARIA_DATA_DIR` — שתי פריסות מזוהות אוטומטית:
   `<root>/otzaria/seforim.db`+`<root>/index` (שורש הורדה) או `<root>/seforim.db` (ספרייה)
4. ברירות-מחדל פר-OS (ה-DB וה-index נפתרים בנפרד).

`--snapshot` פותח את ה-DB עם `immutable=1` (snapshot סגור; אחרת פתיחה read-only מודעת-WAL).

## הגדרה ב-Claude Desktop

ראו [`claude_desktop_config.example.json`](claude_desktop_config.example.json). ב-macOS
הקובץ נמצא ב-`~/Library/Application Support/Claude/claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "otzaria": {
      "command": "/ABSOLUTE/PATH/TO/otzaria_mcp/run-stdio.sh",
      "args": ["--data-dir", "/ABSOLUTE/PATH/TO/otzaria_latest"]
    }
  }
}
```

## כלים (9)

| כלי | תיאור |
|---|---|
| `search_books` | חיפוש ספרים לפי כותרת/תיאור → id, כותרת, heRef, קטגוריה, מחבר, מס' שורות |
| `full_text_search` | חיפוש טקסט מלא (Tantivy). מצבים: `advanced`/`exact`/`fuzzy`. תוצאות `official` כוללות **book_id + line_index** קנוניים; תוצאות PDF/ספרי משתמש/קטלוג חיצוני מסומנות במקורן ואינן נשלחות ל־`get_text` |
| `resolve_ref` | פענוח הפניה עברית → (book_id, line_index). תומך בכותרות, ראשי-תיבות, תנ״ך, משנה, תלמוד (כולל דף `ב:`) |
| `get_text` | שורות ספר לפי `line_index` (0-based) → heRef+content, `next_line_index`, אופציית `strip_html` |
| `get_book_toc` | תוכן עניינים היררכי + מבנים חלופיים. מעבירים `alt_structure_key` מתוך `alt_structures` כדי לקבל את צומתי הפרשה/דף/סימן עם מצביעי `line_index` |
| `get_links` | קישורים לספר/שורה (15 סוגים), כיוון outgoing/incoming/both, מקור+יעד עם heRef+snippet |
| `get_commentaries` | מפרשים/תרגומים על שורה/טווח + רשימת מפרשים דיפולטיביים מסודרת |
| `get_book_info` | מטא מלא: מחבר, דור, נושאים, מקור, דגלי-קישורים, ניקוד/טעמים, ראשי-תיבות, גרסאות |
| `browse_categories` | ניווט עץ הקטגוריות + ספרים תחת קטגוריה |

**הגשר:** תוצאת `full_text_search`/`resolve_ref` מחזירה `book_id`+`line_index` — הזינו
אותם ישירות ל-`get_text` / `get_links` / `get_commentaries` / `get_book_info`. כל תשובה
כוללת `schema_version` (=15) ו-`data_version` (טביעת-אצבע של ה-snapshot).

## ארכיטקטורה

```
לקוח AI ──MCP (stdio)── otzaria-mcp
                          ├── rusqlite  → seforim.db  (ניווט/מטא/הפניות/קישורים)
                          └── search_engine (Tantivy) → אינדקס  (חיפוש טקסט מלא)
```

- **קריאה-בלבד:** ה-DB נפתח `SQLITE_OPEN_READ_ONLY`; המנוע לקריאה בלבד.
- **מיפוי קנוני:** רק באינדקס `file_path="id:<bookId>"` ו-`segment=lineIndex` יש
  מיפוי חד-משמעי לספר ולשורה ב-`seforim.db`. ספרי משתמש, קטלוגים חיצוניים ו-PDF מסומנים
  במפורש ואינם מועברים לכלי ה-SQLite.
- **עדכון-חם:** משימת-רקע מנטרת טביעת-אצבע (mtime+גודל של ה-DB ושני קבצי meta של
  האינדקס); כשהיא משתנה ומתייצבת, ה-DB והאינדקס מוחלפים **כזוג תקין** מאחורי `RwLock`.
- **גרסת מנוע:** ה-crate המוונדר (0.6.7) חייב להתאים לסכימת האינדקס בדיסק
  (schema_version 2). ראו [`vendor/otzaria_search_engine/VENDORED.md`](vendor/otzaria_search_engine/VENDORED.md).

## פיתוח

```bash
cargo test -p otzaria-mcp                                   # בדיקות-יחידה (תמיד)
OTZARIA_DATA_DIR=/path/to/otzaria_latest cargo test -p otzaria-mcp   # + golden corpus מול נתונים אמיתיים
cargo fmt -p otzaria-mcp --check
cargo clippy -p otzaria-mcp --all-targets --no-deps -- -D warnings   # --no-deps: לא לנתח את ה-crate המוונדר
```

בדיקות היחידה כוללות מסד SQLite בזיכרון לקישורי־טווח ול־TOC חלופי, ולכן ה-CI מכסה את
החוזים האלה בלי להוריד את הספרייה. בדיקות האינטגרציה
([`mcp/tests/real_data.rs`](mcp/tests/real_data.rs)) מדלגות בחן כשאין נתונים, ומאמתות עם
נתונים: corpus golden ל-`resolve_ref` והגשר `full_text_search`→`get_text`.

## מבנה

```
Cargo.toml                      # workspace (members = ["mcp"])
vendor/otzaria_search_engine/   # מנוע Tantivy מוונדר (pub.dev 0.6.7)
mcp/src/
  main.rs      # CLI + serve stdio
  paths.rs     # גילוי נתיבים
  db.rs        # שכבת SQLite (7 כלים)
  search.rs    # עטיפת מנוע Tantivy (full_text_search)
  refs.rs      # פענוח הפניות (resolve_ref)
  reload.rs    # Library/Snapshot + עדכון-חם
  server.rs    # הגדרות כלי rmcp
mcp/tests/real_data.rs
```
