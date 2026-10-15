# otzaria-mcp

שרת **MCP** (Model Context Protocol) שחושף את ספריית **אוצריא** ל-AI — לחיפוש, ניווט,
פענוח הפניות (find-ref), קישורים, מפרשים ומטא-דאטה של ספרים — **אופליין לחלוטין
וקריאה-בלבד**. בהשראת [zayit-mcp](https://github.com/Sivan22/zayit-mcp), אך בנוי מעל שני
עולמות הנתונים של אוצריא:

- **`seforim.db`** (SQLite) — ספרים, שורות, הפניות (heRef), עץ קטגוריות, ראשי-תיבות,
  קישורים (24 סוגים) ומטא-דאטה. נתמכת הספרייה הנוכחית (**db_version 32, סכמה 6**: טקסט
  השורות ב-`line_content` דחוס ב-zstd עם מילון מטבלת `zstd_dict`, מהדורות ב-`version_line`,
  אינדקס הפניות `line_ref`, דיבורי-המתחיל `line_dh`, עוגני-מילים `link_anchor`, קישורי-טווח
  `link_range`/`link_coverage`, צדדים מוסתרים `link_suppressed_side`, תאריכי/מקומות הוצאה,
  תיאורים קצרים/ארוכים) — וגם מסדים ישנים (טקסט גלוי ב-`line.content`) בירידה הדרגתית.
- **אינדקס Tantivy** — חיפוש טקסט מלא עם מורפולוגיה עברית. המנוע המקורי של אוצריא
  (`otzaria_search_engine` **3.1.0**, הגרסה שהאפליקציה מצמידה היום, אינדקס בסכמה 5) עבר
  **vendoring** ומשמש ישירות, כך שהחיפוש נאמן במדויק לזה שבאפליקציה — כולל אפשרויות
  החיפוש המתקדם, ביטויים שחוצים שורה, סינון לפי קטגוריה/תקופה/מחבר/ספרי יסוד, וקריאת
  טקסט השורות בחזרה מ-`seforim.db` (המנוע לא שומר את טקסט הספרים הרשמיים באינדקס).

## התקנה למשתמש הסופי

צריך רק שני דברים חוץ מה-MCP:

1. **אוצריא מותקנת** על אותה מכונה (או עותק של נתוניה) — ה-MCP קורא `seforim.db`,
   את אינדקס החיפוש, ואופציונלית `lexical.db` (חיפוש מקורב) ואת מילוני החיפוש המתקדם.
2. **לקוח MCP** (כמו Claude Desktop) עם רישום ה-MCP ב-config (ראו למטה).

### מסלול א' — בינארי מוכן (מומלץ; **בלי Rust**)

הורידו את הבינארי לפלטפורמה שלכם מעמוד ה-Releases (נבנה אוטומטית ע"י ה-CI: לינוקס
x86_64/aarch64, macOS Intel/Apple-Silicon, Windows x86_64). לאחר ההורדה:

```bash
chmod +x otzaria-mcp-macos-aarch64          # הרשאת הרצה (Linux/macOS)
xattr -d com.apple.quarantine otzaria-mcp-* 2>/dev/null || true   # macOS: הסרת הסגר Gatekeeper (בינארי לא חתום)
```

### מסלול ב' — בנייה מהמקור

צריך **Rust** (toolchain יציב, `cargo`), מהדר C (Xcode CLT / gcc / MSVC — עבור SQLite
ו-zstd המובנים) וגישה ל-GitHub בזמן הבנייה (שתי תלויות git מוצמדות של המנוע:
`Otzaria/tantivy` ו-`Otzaria/otzaria-semantic-search`). ואז:

```bash
git clone https://github.com/Otzaria/otzaria_mcp && cd otzaria_mcp
cargo build --release      # הבינארי: target/release/otzaria-mcp
```

הקומפילציה הראשונה כוללת את tantivy ולוקחת מספר דקות. מנוע החיפוש כלול בריפו תחת
[`vendor/otzaria_search_engine`](vendor/otzaria_search_engine).

## הרצה (stdio)

```bash
./run-stdio.sh --data-dir /path/to/otzaria          # או: otzaria-mcp --db … --index …
```

הפרוטוקול (JSON-RPC) עובר על stdin/stdout; לוגים על stderr. בלי אף דגל, השרת מחפש את
נתוני האפליקציה המותקנת בנתיבי ברירת המחדל (למטה).

### נתיבי הנתונים (סדר עדיפויות)

1. דגלים: `--db <path>` · `--index <dir>` · `--lexical-db <path>` · `--dictionaries-dir <dir>`
2. משתני סביבה: `OTZARIA_DB` · `OTZARIA_INDEX` · `OTZARIA_LEXICAL_DB` · `OTZARIA_DICTIONARIES_DIR`
3. `--data-dir <root>` / `OTZARIA_DATA_DIR` — שתי פריסות מזוהות אוטומטית:
   `<root>/otzaria/seforim.db`+`<root>/index` (שורש הורדה) או `<root>/seforim.db`+`<root>/index`
4. ברירות-מחדל לפי מערכת הפעלה — קודם התקנה למשתמש, אחר כך התקנה למחשב כולו:

| מערכת | התקנה למשתמש | התקנה למחשב (מנהל) |
|---|---|---|
| Windows | `%APPDATA%\otzaria` | `%ProgramData%\otzaria` (בדרך כלל `C:\ProgramData\otzaria`) |
| macOS | `~/Library/Application Support/otzaria` | `/Library/Application Support/otzaria` |
| Linux | `~/.local/share/otzaria` (`$XDG_DATA_HOME`) | `/var/lib/otzaria` |

בתוך השורש: הספרייה ב-`books/seforim.db` (או `books/אוצריא/seforim.db` בחבילה המלאה),
האינדקס ב-`index/` (האפליקציה שמה אותו ליד תיקיית הספרייה, או תחת שורש הנתונים בהתקנות
ישנות — שניהם נבדקים), `lexical.db` ליד `seforim.db`, ומילוני החיפוש המתקדם
(`dictionary.json`, `Acronyms.json`) ב-`dictionaries/` שתחת שורש הנתונים של המשתמש.
אם העברתם את הספרייה לכונן אחר — העבירו את הנתיב ב-`--db`/`--index`.

`--snapshot` פותח את ה-DB עם `immutable=1` (snapshot סגור; אחרת פתיחה read-only מודעת-WAL).

## הגדרה ב-Claude Desktop

ראו [`claude_desktop_config.example.json`](claude_desktop_config.example.json). מיקום הקובץ:
macOS `~/Library/Application Support/Claude/claude_desktop_config.json`,
Windows `%APPDATA%\Claude\claude_desktop_config.json`.

```json
{
  "mcpServers": {
    "otzaria": {
      "command": "C:\\Tools\\otzaria-mcp-windows-x86_64.exe",
      "args": [
        "--db", "C:\\ProgramData\\otzaria\\books\\seforim.db",
        "--index", "C:\\ProgramData\\otzaria\\index"
      ]
    }
  }
}
```

ב-macOS/Linux: `"command": "/ABSOLUTE/PATH/TO/otzaria-mcp"` ו-`"args": ["--data-dir", "/path/to/otzaria"]`
(או בלי args כשהנתונים בנתיבי ברירת המחדל).

## כלים (14)

| כלי | תיאור |
|---|---|
| `library_info` | גרסאות (db_version / סכמה), מוני ספרים ושורות, סוגי קישור, אילו טבלאות קיימות, ומצב אינדקס החיפוש (סכמה, גרסת מנוע, מסמכים, מילונים) — או למה החיפוש לא זמין |
| `search_books` | חיפוש ספרים לפי כותרת/ראשי-תיבות/תיאור, דירוג (כותרת מדויקת → כינוי מדויק → תחילית…), סינון לפי קטגוריה. מחזיר מחבר, תקופה, תיאור קצר |
| `lookup_title` | פענוח שם/ראשי-תיבות/כינוי (`רמב"ם`, `שו"ע או"ח`, `מסכת ברכות`) לספרים — מאותו אינדקס-שמות של find-ref |
| `get_book_info` | פרטי ספר: מסלול קטגוריה, תיאור קצר וארוך, מקור, מחברים, דור ותקופה, תאריכי ומקומות הוצאה, נושאים, סוג תלות וכותרת קיבוצית, ספרי בסיס, מפרשים ותרגומים ברירת-מחדל, מבנים חלופיים, מהדורות |
| `get_text` | שורות ספר לפי `line_index` (0-based), פענוח הטקסט הדחוס, `total_lines`, `next_line_index`, `strip_html`, ו-`version` לקריאת מהדורה אחרת |
| `get_book_toc` | תוכן עניינים (שטוח, pre-order) עם `line_index`; `parent_id` לדפדוף עצל; `alt_structure_key` למבנה חלופי (פרשות, דפים…) |
| `browse_categories` | עץ הקטגוריות — לפי id או נתיב כותרות (`תנ"ך/תורה`), עם תיאורים, מסלול, ספירת ספרים רקורסיבית וספרים ישירים |
| `resolve_ref` | **find-ref**: הפניה עברית → `(book_id, line_index)` כמו באפליקציה — שם/כינוי ספר, מפתח ההפניה הקנוני מול `line_ref` (כולל `פרק/פסוק/סימן`, `ב.`/`ב:`/`ב ע"א`, מפתחות חלקיים), תחילית פרק/דף, כותרות TOC ופרשות, ודיבור המתחיל |
| `full_text_search` | חיפוש טקסט מלא במנוע של האפליקציה: מצבים `advanced`/`exact`/`fuzzy`; מרווח מילים / מרחק עריכה; אפשרויות מילה (קידומות, סיומות, דקדוקיות, כתיב מלא/חסר, חלק ממילה, שגיאות כתיב, ארמית, התעלמות מגרשיים, ראשי תיבות, תרגום ארמי); `exclude_query`; `scope`; `word_match`; ניקוד/טעמים; סינון לפי קטגוריות/ספרים/תקופות/מחברים/ספרי יסוד; סדר `catalogue`/`relevance`/`generation`; דפדוף (`offset` עד 10000); `grouping`. תוצאות רשמיות מחזירות `book_id`+`line_index`, `text_status`, `continues_to_next_line` |
| `search_facet_counts` | איפה הביטוי מופיע: ספירת תוצאות לפי תת-קטגוריה, תקופה, מחבר או ספר |
| `get_links` | קישורים לספר/שורה/טווח במודל של האפליקציה: יוצאים/נכנסים (מקור וירטואלי `SOURCE`), כל 24 סוגי הקישור (כולל `LINKER`; סוג שמתבקש במפורש מוחזר גם מהצד המצוטט), קישורי-טווח, צדדים מוסתרים, עוגני-מילים, סינון לפי סוג וספר, דפדוף |
| `get_commentaries` | טקסט המפרשים/תרגומים/מדרש על שורה או טווח (כולל פירוש שמשתרע על כמה שורות), תקופת המפרש, עוגן המילה בשורת הבסיס, ומפרשי/תרגומי ברירת המחדל |
| `list_commentators` | אילו מפרשים מקושרים לספר או לשורה/טווח, עם ספירה לפי סוג, מקובצים וממוינים לפי תקופה (תורה שבכתב, חז"ל, ראשונים, אחרונים, מחברי זמננו) כמו בחלונית המפרשים |
| `get_line_anchors` | עוגני-המילים בתוך שורה (`link_anchor`): טווח התווים, המילים המעוגנות (או ההקשר סביב אות-מפרש), התווית והיעד |

**הגשר:** תוצאת `full_text_search`/`resolve_ref` מחזירה `book_id`+`line_index` — הזינו
אותם ישירות ל-`get_text` / `get_links` / `get_commentaries` / `get_line_anchors`. כל תשובה
כוללת `schema_version` (= `db_version` של הספרייה, 32 היום) ו-`data_version` (טביעת-אצבע
של ה-snapshot). התשובות הן JSON דחוס.

### שינויי התנהגות מול 0.1
- `full_text_search` במצב `advanced` משתמש כברירת מחדל במרווח 0 בין מילים (כמו
  באפליקציה; ב-0.1 היה 2). העבירו `distance` לשינוי.
- `get_links` מוסיף את נקודת המבט של הספר המבוקש (`at_line_index`, ושדות הצד
  המקושר: `book_id`, `line_index`, `he_ref`, `snippet` …), ואת סוג התצוגה של האפליקציה
  (`display_type`, למשל `SOURCE`). שדות `source_*` ו־`target_*` מ־0.1 נשמרים בכיוון האחסון
  המקורי, גם כשמבקשים קישור נכנס או שורה מכוסה של טווח.
- `get_book_info.versions` נשאר רשימת שמות מהדורות. פרטי המהדורות זמינים בשדה החדש
  `version_details`; מהדורה עם `has_text=true` ניתנת לקריאה באמצעות `get_text.version`.
- `full_text_search` שומר גם את `is_pdf` ואת `page` (כולל `null` לתוצאה שאינה PDF).

### שילוב מסנני חיפוש
קטגוריות וספרים הם בחירות באותו עץ היקף חיפוש, כמו באפליקציה: `categories`,
`category_ids` ו־`book_ids` מצטרפים ב־OR. למשל, בחירה בתורה וגם בבראשית מחפשת בכל התורה.
כדי לחפש רק בבראשית, העבירו `book_ids: [1]` בלי קטגוריות. בין ממדים שונים חל AND:
ההיקף שנבחר מצטלב עם התקופות, המחברים ו־`base_only`; בתוך כל ממד הערכים מצטרפים ב־OR.

## ארכיטקטורה

```
לקוח AI ──MCP (stdio)── otzaria-mcp
                          ├── rusqlite + zstd → seforim.db  (ניווט/מטא/הפניות/קישורים)
                          └── search_engine 3.1.0 (Tantivy) → index/  (+ קריאת שורות מ-seforim.db)
```

- **קריאה-בלבד:** ה-DB נפתח `SQLITE_OPEN_READ_ONLY` + `query_only`; האינדקס נפתח
  ב-`SearchEngine::open_read_only` (בלי `open_or_create`, בלי נעילת כותב, בלי כתיבת
  `otzaria_index_meta.json`; קורא ה-Tantivy נועל לרגע את `.tantivy-meta.lock` בזמן פתיחת
  הסגמנטים, קובץ שקיים בכל אינדקס שהאפליקציה כתבה); מנגנון קריאת השורות של המנוע פותח את
  `seforim.db` לקריאה בלבד.
- **מיפוי קנוני:** רק באינדקס `file_path="id:<bookId>"` ו-`segment`=מיקום השורה (=`lineIndex`)
  יש מיפוי חד-משמעי לספר ולשורה. ספרים אישיים, קטלוגים חיצוניים ו-PDF מסומנים ב-`source`.
- **עדכון-חם:** משימת-רקע מנטרת טביעת-אצבע (mtime+גודל של ה-DB ושני קבצי meta של
  האינדקס); כשהיא משתנה ומתייצבת, נפתחים מחדש ה-DB והאינדקס מאחורי `RwLock`.
  גם חיבור קריאת השורות של מנוע החיפוש מתרענן כשמחליפים את קובץ המסד באותו נתיב.
  תוצאה שאינה תואמת עוד את האינדקס מסומנת `stale`.
- **גרסת מנוע:** ה-crate המוונדר (3.1.0) חייב להתאים לסכימת האינדקס בדיסק (סכמה 5).
  ראו [`vendor/otzaria_search_engine/VENDORED.md`](vendor/otzaria_search_engine/VENDORED.md).
- **חיפוש סמנטי ("חיפוש חכם")** — עדיין לא נחשף; ראו [CHANGELOG](CHANGELOG.md) ("עבודה עתידית").

## פיתוח

```bash
cargo test -p otzaria-mcp                                        # בדיקות-יחידה (תמיד)
OTZARIA_TEST_DB=/path/to/seforim.db cargo test -p otzaria-mcp     # + בדיקות מול ספרייה אמיתית (v32)
cargo fmt -p otzaria-mcp --check
cargo clippy -p otzaria-mcp --all-targets --no-deps -- -D warnings
```

בדיקות היחידה כוללות מסדי SQLite בזיכרון (קישורי-טווח, TOC חלופי, find-ref עם `line_ref`
ו-`line_dh`, פענוח מסגרות zstd עם מילון), כך שה-CI מכסה את החוזים בלי להוריד את הספרייה.
בדיקות האינטגרציה ([`mcp/tests/real_data.rs`](mcp/tests/real_data.rs)) מדלגות בחן בלי
`OTZARIA_TEST_DB`; עם מסד הן מאמתות corpus golden ל-find-ref, פענוח טקסט ומהדורות, קישורים,
מפרשים ועוגנים — ובונות אינדקס זעיר משלושה ספרים (בדיוק כפי שהאפליקציה מאנדקסת ספרים
רשמיים) כדי לבדוק את החיפוש, הסינון, הספירות והגשר חיפוש→`get_text`. לאינדקס מלא קיים
העבירו `OTZARIA_TEST_INDEX`.

## מבנה

```
Cargo.toml                      # workspace + [patch.crates-io] של המנוע
vendor/otzaria_search_engine/   # מנוע Tantivy מוונדר (pub.dev 3.1.0) + open_read_only
mcp/src/
  main.rs      # CLI + serve stdio
  paths.rs     # גילוי נתיבים
  db.rs        # שכבת SQLite: ספרים, טקסט, מהדורות, TOC, קטגוריות, מטא
  links.rs     # קישורים, מפרשים, רשימת מפרשים לפי תקופה, עוגנים
  text.rs      # פענוח zstd, ניקוי HTML, תווים גלויים (קואורדינטות העוגנים)
  refkey.rs    # מפתח ההפניה הקנוני (פורט של ref_key.dart) + FNV ל-line_ref
  refs.rs      # find-ref (resolve_ref)
  search.rs    # עטיפת מנוע החיפוש
  reload.rs    # Library/Snapshot + עדכון-חם
  server.rs    # הגדרות כלי rmcp
mcp/tests/real_data.rs
```
