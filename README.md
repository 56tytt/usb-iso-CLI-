# usb-iso-CLI-
BurnEngine USB CLI v3.0
# 🔥 BurnEngine USB CLI v3.0

![Rust](https://img.shields.io/badge/language-rust-orange.svg)
![Platform](https://img.shields.io/badge/platform-linux-lightgrey.svg)
![License](https://img.shields.io/badge/license-MIT-blue.svg)

**BurnEngine** הוא כלי שורת פקודה (CLI) מודרני, בטיחותי ומהיר שנכתב ב-Rust, המיועד לצריבת קבצי ISO ישירות לכונני USB במערכות לינוקס. במקום להסתבך עם פקודות `dd` מסוכנות, BurnEngine דואג להגנה על הנתונים שלך ומציג התקדמות בזמן אמת.

## ✨ תכונות עיקריות (Features)

* 🔍 **זיהוי חכם:** מזהה אוטומטית רק כונני USB חיצוניים ומסנן כונני מערכת (NVMe/SATA).
* 🛡️ **בטיחות מעל הכל:** כולל מנגנון אישור כפול (Double Confirmation) לפני כל פעולת כתיבה.
* 📂 **ממשק היברידי:** תמיכה בבחירת קבצים גרפית (File Picker) ישירות מהטרמינל.
* 📊 **חיווי בזמן אמת:** Progress Bar מפורט הכולל מהירות כתיבה (MiB/s) וזמן סיום משוער (ETA).
* 🧹 **ניהול מחיצות:** מבצע Unmount אוטומטי לכונן לפני תחילת הצריבה.

## 🚀 התקנה (Installation)

1. **דרישות מוקדמות:**
   - וודא שמותקן אצלך `Rust` ו-`Cargo`.
   - התוכנה מסתמכת על `dd` ו-`lsblk` (קיימים בכל הפצות הלינוקס).

2. **בנייה מהמקור:**
   ```bash
   git clone [https://github.com/YOUR_USERNAME/burnengine.git](https://github.com/YOUR_USERNAME/burnengine.git)
   cd burnengine
   cargo build --release
