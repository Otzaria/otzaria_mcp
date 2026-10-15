# Changelog

## 0.2.0 — 2026-10-13 — עדכון לספרייה v32 ולמנוע החיפוש 3.1.0

### תיקוני סקירה — 2026-10-15
- רענון חיבור קריאת השורות לאחר החלפת מסד באותו נתיב, כולל בזמן חיפוש מקביל.
- הסרת כפילויות של קישורי טווח לפני דפדוף, ו־offset משותף לקישורים יוצאים ונכנסים.
- שימור תחביר הכתובת המקורית בפענוח הפניות לטווחים ולפרק:פסוק.
- ניקוי HTML לחוצץ רציף וחיתוך snippets בלי הקצאת מחרוזת לכל תו.
- שימור שדות `source_*`/`target_*`, רשימת שמות המהדורות `versions`, ו־`is_pdf`/`page`
  ללקוחות 0.1. מטא־דאטה של מהדורות נחשף בנפרד ב־`version_details`.
- תיעוד סמנטיקת המסננים: קטגוריות וספרים באיחוד, ממדי תקופה/מחבר/בסיס בחיתוך.

### מה היה שבור מול הספרייה הנוכחית (db_version 32, סכמה 6)
- **טקסט השורות עבר מ-`line.content` לטבלה `line_content`, דחוס ב-zstd עם מילון**
  (`zstd_dict`). כל שאילתה שקראה `line.content` נכשלה ב-`no such column: content`:
  `get_text`, `get_links`, `get_commentaries`, ו-`resolve_ref` (השליפה של ה-snippet ו-fallback).
- **האינדקס נבנה היום ב-`otzaria_search_engine` 3.1.0 (סכמת אינדקס 5)**; המנוע המוונדר
  0.6.7 קורא רק סכמה 2, ולכן `check_index_compatibility` דוחה את האינדקס ו-`full_text_search`
  כבוי. בנוסף, בסכמה 5 טקסט הספרים הרשמיים אינו שמור באינדקס כלל — המנוע קורא אותו
  בחזרה מ-`seforim.db` דרך ה-line source, שצריך להגדיר.
- `link.isDeclaredBase` הוחלף ב-`baseProvenance`; נוספו 9 סוגי קישור (24 במקום 15);
  נוספו `link_suppressed_side` (צדדים שהאפליקציה מסתירה), `link_coverage`, `line_ref`,
  `line_dh`, `version_line`, `book_pub_date`/`pub_date`, `book_pub_place`/`pub_place`,
  `book_base_text`, `category.heShortDesc/heDesc`, `book.dependenceType/collectiveTitle*`.
- ה-golden corpus של `resolve_ref` הוצמד לתוכן ישן (למשל שו"ע או"ח א א הוא היום שורה 4).

### שינויים
- **מנוע החיפוש**: vendoring של `otzaria_search_engine` 3.1.0 (הגרסה שהאפליקציה מצמידה),
  כולל `[patch.crates-io]` של tantivy-fst ושל `Otzaria/tantivy@85b76fe` ב-workspace, ופתיחה
  לקריאה בלבד (`SearchEngine::open_read_only`).
- **פענוח טקסט**: `text.rs` — מסגרות zstd לפי מזהה המילון שבכותרת המסגרת (כמו
  `LineContentCodec` באפליקציה וה-line source של המנוע), TEXT גלוי במסדים ישנים, NULL=שורה ריקה.
- **`get_text`**: פענוח, `total_lines`, ופרמטר `version` לקריאת מהדורה מ-`version_line`
  (שורות מבנה מהשלד, שורה חסרה במהדורה = ריקה — כמו באפליקציה).
- **`get_book_info`**: מסלול קטגוריה, תקופה, תאריכי/מקומות הוצאה, סוג תלות וכותרת
  קיבוצית, ספרי בסיס, תרגומי ברירת מחדל, מבנים חלופיים, פרטי מהדורות, `book_has_links`.
- **`get_links`**: מודל הקישורים של האפליקציה — יוצאים (+`link_coverage` צד 0) ונכנסים
  (+צד 1, סוגים תלויי-טקסט כ-`SOURCE` וירטואלי, סוגי הפניה דו-כיווניים), הסתרת צדדים מדוכאים,
  קצה טווח (`link_range`), עוגנים (`link_anchor`), סינון לפי כמה סוגים ולפי ספרים, דפדוף.
  הרחבה מעבר לאפליקציה: סוג שמתבקש במפורש (למשל `LINKER`, שהאפליקציה מציגה רק מהצד המצטט)
  מוחזר גם ב-`direction=incoming`.
- **`get_commentaries`**: כל הסוגים תלויי-הטקסט (כולל ELUCIDATION/EXPLICATION/FOOTNOTES),
  טקסט פירוש שמשתרע על כמה שורות, תקופה, עוגן בשורת הבסיס, תרגומי ברירת מחדל.
- **`resolve_ref`** (find-ref): פורט של `ref_key.dart` (מפתח קנוני + FNV-1a-64) וחיפוש
  ב-`line_ref` (מפתח מלא וחלקי), `ע"א/ע"ב`, מילות מיקום, כותרות TOC ופרשות, דיבור המתחיל
  (`line_dh`); ה-snippet מפוענח.
- **`full_text_search`**: מצבים exact/advanced/fuzzy במנוע 3.1.0; ברירות המחדל של האפליקציה
  (מרווח 0, מרחק fuzzy 2); כל אפשרויות המילה של החיפוש המתקדם; `exclude_query`, `scope`,
  `word_match`, ניקוד/טעמים, `grouping`, סדר `generation`; סינון לפי קטגוריות (כותרות/מזהים),
  ספרים, תקופות, מחברים וספרי יסוד; `text_status`, `continues_to_next_line`, `truncated`.
  `offset` מוגבל ל-10000: המנוע שומר offset+limit תוצאות לכל סגמנט לפני הדילוג, והיסט
  ענק (למשל 4e9) גרם להקצאת מאות GB ולקריסת השרת כולו.
- **`get_book_toc`**: `parent_id` לדפדוף עצל. **`browse_categories`**: נתיב כותרות, תיאורים,
  ספירת ספרים רקורסיבית. **`search_books`**: כינויים, דירוג, סינון לפי קטגוריה, תקופה.
- **כלים חדשים**: `library_info`, `lookup_title`, `list_commentators`, `get_line_anchors`,
  `search_facet_counts`.
- **נתיבים**: ברירות מחדל להתקנה מערכתית (`%ProgramData%\otzaria`,
  `/Library/Application Support/otzaria`, `/var/lib/otzaria`), `books/אוצריא/seforim.db`,
  ו-`--dictionaries-dir` (מילוני תרגום ארמי וראשי תיבות של החיפוש המתקדם).
- תשובות JSON דחוסות (במקום pretty) כדי לחסוך טוקנים.

### עבודה עתידית
- **חיפוש סמנטי ("חיפוש חכם")**: המנוע חושף `open_semantic_artifact` + `search_semantic`,
  אך הם דורשים בנייה עם feature `semantic` (backend ONNX), ספריית ONNX Runtime, חבילת המודל
  (`meivin-round2-onnx/` ליד `seforim.db`) וסט הוקטורים (`<root>/vectors`). לא ניתן היה לאמת
  זאת בלי נתונים אמיתיים, ולכן הוא לא נחשף בגרסה זו. מועמד טבעי: feature אופציונלי
  `semantic` עם `--vectors-dir`/`--semantic-model`.
- ספרים אישיים (`user_books.db`) ומסדים מצורפים — תוצאות חיפוש שלהם מסומנות אך לא נקראות.
- HTTP transport (נדחה במכוון; ראו PLAN.md).
