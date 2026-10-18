# תוכנית: otzaria-mcp — שרת MCP לאוצריא (מתוקנת אחרי ביקורת)

> **עדכון 0.2 (אוקטובר 2026) — ספרייה v32 / סכמה 6, מנוע 3.1.0.** המספרים והסכמה שלמטה
> מתעדים את v1 (db_version 15, מנוע 0.6.7, אינדקס סכמה 2). מאז: טקסט השורות עבר
> ל-`line_content` דחוס ב-zstd עם מילון (`zstd_dict`), נוספו `version_line`, `line_ref`,
> `line_dh`, `link_coverage`, `link_suppressed_side`, נתוני הוצאה ותיאורים; 24 סוגי קישור
> (`baseProvenance` במקום `isDeclaredBase`); האינדקס בסכמה 5 בלי טקסט הספרים הרשמיים
> (המנוע קורא אותו מ-`seforim.db`). המימוש עודכן בהתאם ונוספו 5 כלים (14 בסך הכול):
> `library_info`, `lookup_title`, `list_commentators`, `get_line_anchors`,
> `search_facet_counts`. פירוט: [CHANGELOG.md](CHANGELOG.md), [README.md](README.md).
> חיפוש סמנטי — עבודה עתידית (feature אופציונלי; ראו CHANGELOG).

## Context (למה)
המשתמש רוצה שרת MCP שחושף את ספריית אוצריא ל-AI, במתכונת דומה ל-`zayit-mcp`
(`/Users/david/Documents/others/zayit-mcp` — Kotlin/Lucene מעל `seforim.db` של זית).
שני ממצאים מכריעים:

1. **ה-DB של אוצריא מבוסס על הסכימה של זית + הרחבות רבות.** אומת מול ה-DB **בזמן ריצה**
   (`/Users/david/Downloads/otzaria_latest/otzaria/seforim.db`): **7,332 ספרים · 5,878,826
   שורות · 5,800,142 קישורים**, `schema_meta`: **db_version=15**, db_schema_version=1.
   כולל: `book_acronym` (52,582), `author`/`generation`/`book_generation`, `topic`,
   `default_commentator`/`default_targum`, `category_closure`, alt-TOC, ו-**15 סוגי קישור**
   (`COMMENTARY`=1, `SUPER_COMMENTARY`, `TARGUM`, `REFERENCE`, `SOURCE`, `MIDRASH`,
   `QUOTATION`, `MESORAT_HASHAS`, `EIN_MISHPAT`, `DIBUR_HAMATCHIL`, `PARSHANUT`,
   `MISHNAH_IN_TALMUD`, `RELATED`, `OTHER`, `LINKER`). מודל הקישורים עשיר: `link`
   (+`targetLineIndex`,`isDeclaredBase`), `link_range` (טווחי-שורות), `link_anchor`
   (עוגני-תווים), `link_coverage`. יש גם גרסאות-טקסט (`book_version`/`version_line`).
   *הערה:* מדדתי בתחילה בטעות עותק dev בתוך ריפו אוצריא (7,030/6.06M, db_meta v1,
   5 סוגי-קישור) — הוא **לא** ה-DB של המשתמש. כל המספרים/סוגים לעיל הם מה-DB האמיתי.
2. **חיפוש הטקסט המלא הוא Tantivy (Rust), לא Lucene.** קוד המקור ציבורי:
   `github.com/Otzaria/otzaria_search_engine` (crate `search_engine`). האינדקס בפועל:
   `/Users/david/Downloads/otzaria_latest/index` (schema_version 2, 22 סגמנטים) — קריא ע"י ה-crate.

מכאן: אי-אפשר לשכפל את zayit-mcp; חייבים **reuse ל-crate של Rust** כדי לשחזר את החיפוש
בנאמנות. משתמש הקצה מחזיק רק את נתוני אוצריא (`seforim.db`, `lexical.db`, `index/`, PDFים)
— לא את קוד המקור. לכן ה-MCP עצמאי בזמן ריצה וקורא **קריאה-בלבד** את נתוני אוצריא.

**תוצאה ממומשת (v1):** בינארי עצמאי, תעבורת **stdio בלבד**, 9 כלים, קריאה-בלבד.

---

## ⚑ אבן-הפינה (Milestone 1) — להוכיח מיפוי Search → (bookId, lineIndex)
הסיכון המרכזי (ביקורת #1): תוצאת חיפוש חייבת להחזיר **מזהה קנוני** — `bookId` **ו**-`lineIndex`
— ולא להסתמך על `title → bookId` (שביר). האינדקס שומר: `segment` (=`line.lineIndex`),
`filePath` (לספרי-טקסט = מפתח קטלוגי שמקודד את המזהה, למשל `id:<bookId>`/`uid:<bookId>`;
ל-PDF = נתיב), `title`, `id` (סינתטי `(bookNumber<<32)+ordinal+1`, מספר ספר שהמנוע מקצה; **לא** מפתח DB).

**המשימה הראשונה, לפני 9 הכלים:** לפתוח את האינדקס עם ה-crate, להריץ `search_and_count_advanced`,
ולאמת שכל hit ממופה **חד-משמעית** ל-`(bookId, lineIndex)`:
- לפרסר `filePath` → `bookId` (לוודא את הקידוד בפועל מול האינדקס האמיתי, לא מהתיעוד).
- `segment` → `lineIndex`; ואז `SELECT ... FROM line WHERE bookId=? AND lineIndex=?` חייב
  להחזיר טקסט שתואם ל-`text`/`reference` של ה-hit.
אם המיפוי עובד — שאר הפרויקט ישיר. אם לא — לעצב שכבת-מיפוי (או אינדקס-עזר) **לפני** בניית הכלים.
(ל-PDF אין שורות ב-`line`; שם המזהה הוא `filePath`+`segment`=עמוד, ומטופל בנפרד.)

---

## ארכיטקטורה
- **שפה: Rust.** בינארי **עצמאי לכל פלטפורמה** (לא בהכרח static-linking מלא ב-macOS/Windows).
- **חיפוש טקסט מלא:** תלות ב-crate `search_engine` (git submodule מוצמד ל-commit).
  קריאה ישירה של `search_and_count_{exact,advanced,fuzzy}` + `get_document_by_id`;
  בלי variants של stream (מנעים `StreamSink` של FRB).
- **ספרייה מובנית:** `rusqlite` (feature `bundled`) מעל `seforim.db`, פתיחה **read-only**
  ב-`SQLITE_OPEN_READ_ONLY` כברירת-מחדל (מכבד WAL — אם אוצריא כותבת/טרם checkpoint נקבל state עקבי).
  `immutable=1` **רק** מאחורי דגל מפורש `--snapshot` (מצב snapshot סגור; מתעלם מ-WAL ולכן לא ברירת-מחדל).
- **תעבורת MCP:** ה-SDK הרשמי `rmcp` — **stdio בלבד ב-v1**. HTTP נדחה (ראה למטה).

### גילוי נתיבים (`paths.rs`) — סדר עדיפויות מפורש (ביקורת #2)
בלי קריאת Hive. הסדר:
1. דגלים: `--db <path>` · `--index <path>` · `--lexical-db <path>`
2. משתני סביבה: `OTZARIA_DB` · `OTZARIA_INDEX` · `OTZARIA_LEXICAL_DB`
3. `--data-dir <root>` / `OTZARIA_DATA_DIR` — תומך **בשתי פריסות** (מזהים אוטומטית מי מהן קיים):
   - root של הורדה: `<root>/otzaria/seforim.db` + `<root>/index` (+ `<root>/otzaria/lexical.db`)
   - ספריית אוצריא עצמה: `<root>/seforim.db` + אינדקס שכן/מפורש (+ `<root>/lexical.db`)
4. ברירות-מחדל פר-OS — **לא אחידות** באפליקציה מותקנת: ה-DB עשוי לשבת תחת
   `~/Library/Application Support/otzaria/books/seforim.db` בעוד האינדקס תחת
   `~/Library/Application Support/otzaria/index/`. לכן db ו-index נפתרים בנפרד (לא מאותו שורש).
`lexical.db` אופציונלי — בלעדיו fuzzy עובד בלי מורפולוגיה מילונית.

### עדכון ספרייה חם (ביקורת #6)
אוצריא יכולה להחליף את ה-DB והאינדקס בזמן שה-MCP רץ. השרת:
- **fingerprint של snapshot** = שילוב mtime+גודל של שלושת הקבצים: `seforim.db`,
  `index/meta.json`, `index/otzaria_index_meta.json` (מנטרים את **שניהם** — קיימים בפועל).
  `schema_meta.db_version` לבדו לא מספיק — הוא גרסת-סכימה, לא זהות-תוכן.
- בזיהוי שינוי: **debounce** (להמתין להתייצבות), בדיקת-תאימות של האינדקס, ואז **החלפת
  DB+index כזוג תקין** מאחורי `RwLock` — לעולם לא לחבר DB חדש לאינדקס ישן. בלי להגיש state ישן.

---

## כלים — v1: 9 כלים
| כלי | מקור | פלט/הערות |
|---|---|---|
| `full_text_search` | Tantivy | query, mode(`advanced`/`exact`/`fuzzy`), limit, offset, category(facet אופ'), order. מחזיר לכל hit: **bookId, lineIndex** (קנוני), title, reference, snippet, isPdf, filePath + total. (fuzzy: טוען `lexical.db` אם קיים) |
| `get_text` | SQLite | bookId + **`line_index`** (לא `offset`) + limit; מחזיר שורות (heRef+content, `strip_html`) **ו-`next_line_index`** להמשך. (`line WHERE bookId=? AND lineIndex BETWEEN…`) |
| `resolve_ref` | SQLite | *מורכב* (ביקורת #4). נרמול (ניקוד, `.`/`:`→דף/עמוד, גרשיים) → התאמת תחילית ל-`book_acronym.term`/`book.title` → bookId; שאר הטוקנים מול היררכיית `tocEntry`+`tocText`(+alt_toc) → lineIndex. Fallback: `line.heRef LIKE`. מחזיר מועמדים עם bookId+lineIndex+snippet+ציון-ודאות |
| `search_books` | SQLite | `title LIKE ? OR heShortDesc LIKE ?` + `book_author→author→generation`. מחזיר bookId, title, heRef, מחבר, קטגוריה, דור, totalLines, isBaseBook |
| `get_links` | SQLite | לפי bookId (+lineIndex/connection_type אופ'); מטפל ב-15 סוגי-קישור ובמודל הטווחים (`link`+`link_range`). כותרות+heRef+snippet של מקור ויעד |
| `get_book_info` | SQLite | ל-bookId: heRef, heShortDesc/heDesc, מחבר(ים)+דור, נושאים, מקור, שנת/מקום הוצאה, דגלי-קישורים, hasAltStructures/Teamim/Nekudot, totalLines, גרסאות-טקסט (`book_version`), כמה ראשי-תיבות |
| `get_commentaries` | SQLite | מפרשים ותרגומים על שורה או טווח, כולל קישורי־טווח ורשימת מפרשי ברירת המחדל |
| `get_book_toc` | SQLite | TOC רגיל; `alt_structure_key` מחזיר גם את צומתי המבנה החלופי עם `lineIndex` |
| `browse_categories` | SQLite | עץ הקטגוריות וספרים ישירים תחת קטגוריה |

### חוזה API אחיד (ביקורת #5) — להגדיר לפני מימוש
- `limit`: ברירת-מחדל וסף עליון מפורשים לכל כלי (למשל 50, מקס' 200).
- כתובות שורה **0-based** ומתועדות: `get_text` מקבל `line_index` (לא `offset`) ומחזיר `next_line_index`;
  `full_text_search`/`resolve_ref` מחזירים `line_index` תואם ל-`get_text`.
- HTML: `content` הוא HTML גולמי; דגל `strip_html` (ברירת-מחדל: מנוקה) + snippet מנוקה.
- שגיאות: מבנה אחיד (`code`,`message`) — book/line לא נמצאו, אינדקס לא תואם, query ריק.
- כל תשובה כוללת `data_version` = **fingerprint של ה-snapshot** (hash של mtime+גודל של
  `seforim.db`+`meta.json`+`otzaria_index_meta.json`), בנוסף ל-`schema_version` (=15) בנפרד.

---

## מבנה הריפו
```
otzaria_mcp/
  Cargo.toml            # workspace root; [patch.crates-io]
                        #   tantivy-fst = { path = "vendor/otzaria_search_engine/rust/vendor/tantivy-fst" }
  .gitmodules           # vendor/otzaria_search_engine → github.com/Otzaria/otzaria_search_engine (commit מוצמד)
  vendor/otzaria_search_engine/   # submodule (משתמשים ב-rust/)
  mcp/
    Cargo.toml          # rmcp, tokio, rusqlite(bundled), serde, serde_json, anyhow;
                        #   search_engine = { path = "../vendor/otzaria_search_engine/rust" }
    src/  main.rs · paths.rs · db.rs · refs.rs · search.rs · server.rs · reload.rs
    tests/              # ראה "בדיקות"
  fixtures/             # seforim.db זעיר + index זעיר לבדיקות
  run-stdio.sh · README.md (עברית) · claude_desktop_config.json (דוגמה)
```
ה-`[patch.crates-io]` בשורש הכרחי (patch של תלות מתעלמים ממנו): tantivy-fst הוונדור מעלה
`STATE_LIMIT` של DFA ל-8192, ש-RegexPhraseQuery מסתמך עליו. fallback אם ההצמדה מסתבכת:
vendoring של קוד ה-crate + `vendor/tantivy-fst` פנימה.

---

## בדיקות ואיכות (ביקורת #7)
- fixtures זעירים: `seforim.db` מוקטן (כמה ספרים/שורות/לינקים) + אינדקס Tantivy תואם-סכימה.
- **corpus בדיקות ל-`resolve_ref`** (נקודת-הסיכון הגדולה שנותרה): עשרות הפניות מוגדרות-מראש
  עם התוצאה הצפויה — תנ״ך (`בראשית א א`), תלמוד כולל `דף ב:`/`ב.`, ראשי-תיבות (`שו"ע או"ח`),
  פירושים, ומבני-TOC חלופיים (Parasha/Siman/Daf). מריצים כ-golden tests מול ה-DB האמיתי.
- בדיקות-חוזה לכל כלי (קלט/פלט/גבולות/שגיאות), כולל אימות `full_text_search` → `get_text` (זהות מיקום).
- בדיקת MCP end-to-end (handshake + tools/list + tools/call דרך stdio).
- מטריצת-תאימות מול snapshot אמיתי של DB+index (מסומן ignored כברירת-מחדל אם הנתונים לא זמינים ב-CI).
- שער-איכות: `cargo test` · `cargo fmt --check` · `cargo clippy -- -D warnings`.

## HTTP — נדחה במכוון (ביקורת #3)
רוב לקוחות MCP משתמשים ב-stdio. כשיתווסף: bind ל-`127.0.0.1` כברירת-מחדל, אימות אם חושפים
לרשת, והגבלת CORS/Origin מפורשת.

## בנייה והפצה
`git clone --recursive` → `cargo build --release` → `target/release/otzaria-mcp`
(קומפילציה ראשונה כבדה — tantivy). הפצה: בינארי עצמאי פר-OS; המשתמש לא צריך Rust/את הריפו.
`run-stdio.sh` ו-`claude_desktop_config.json` מצביעים על הבינארי; נתיבי-נתונים דרך דגלים/env/data-dir.

## סדר עבודה (milestones)
0. שלד: workspace + submodule + rmcp stdio (tools/list) + `paths.rs` + פתיחת SQLite read-only,
   `search_books`/`get_text`/`get_book_info` (מוכיח DB + חוזה + `data_version`).
1. **אבן-הפינה:** לפתוח index עם ה-crate, `full_text_search`, ולהוכיח מיפוי חד-משמעי
   `(bookId,lineIndex)` (אימות מול `get_text`).
2. `resolve_ref` + `get_links` (מודל 15-הסוגים והטווחים).
3. hot-reload (`reload.rs`) + חוזה-שגיאות + בדיקות מלאות (fmt/clippy/unit/e2e).
4. הרחבות עתידיות: HTTP עם האבטחה לעיל.

## אימות (end-to-end, מול הנתונים האמיתיים)
1. `search_books "בראשית"` → ספרים עם מחבר/דור/קטגוריה.
2. `full_text_search "ואהבת לרעך"` (advanced) → hits עם **bookId+lineIndex**+reference+snippet; total>0.
3. הגשר: לקחת hit, להריץ `get_text(bookId, offset=lineIndex, limit=1)` → הטקסט תואם ל-hit.
4. `resolve_ref "בראשית א א"` → bookId+lineIndex; `get_text` שם מחזיר את הפסוק.
5. `get_links` לשורת ספר-בסיס (למשל בראשית) → קישורים בסוגים נכונים; `get_book_info 1` → מטא מלא.
6. עדכון-חם: להחליף DB/index תוך כדי ריצה → `data_version` מתעדכן, אין תוצאות ישנות.
