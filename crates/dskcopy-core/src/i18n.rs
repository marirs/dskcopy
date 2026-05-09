//! Minimal localization framework for dskcopy.
//!
//! English is the default. Additional languages can be added by implementing
//! the `Messages` trait and registering via `set_locale()`.
//!
//! Usage:
//!   ```
//!   dskcopy_core::i18n::set_locale("en");
//!   let msg = dskcopy_core::i18n::t("flash_in_progress");
//!   ```

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

static CURRENT_LOCALE: OnceLock<Mutex<String>> = OnceLock::new();
static MESSAGES: OnceLock<Mutex<HashMap<&'static str, HashMap<&'static str, &'static str>>>> =
    OnceLock::new();

fn locale() -> String {
    CURRENT_LOCALE
        .get_or_init(|| Mutex::new("en".to_string()))
        .lock()
        .unwrap()
        .clone()
}

fn message_table() -> &'static Mutex<HashMap<&'static str, HashMap<&'static str, &'static str>>> {
    MESSAGES.get_or_init(|| {
        let mut table: HashMap<&str, HashMap<&str, &str>> = HashMap::new();

        // ── English ────────────────────────────────────────────────────────
        let mut en = HashMap::new();
        en.insert("app_title", "dskcopy");
        en.insert("tab_devices", "Devices");
        en.insert("tab_image", "Image");
        en.insert("tab_flash", "Write & Verify");
        en.insert("btn_refresh", "Refresh");
        en.insert("btn_next_drive", "Next Drive");
        en.insert("btn_pick_file", "Pick File");
        en.insert("btn_flash_now", "Flash Now");
        en.insert("label_no_drive", "No removable USB drives detected");
        en.insert("label_no_image", "No image selected");
        en.insert("label_status_idle", "Idle");
        en.insert("label_type_prefix", "Type: ");
        en.insert("warning_show_all", "WARNING: System and internal drives are visible. Do NOT flash your boot drive.");
        en.insert("confirm_flash_title", "Confirm Flash");
        en.insert("confirm_flash_body", "This will ERASE all data on the target device. Are you sure?");
        en.insert("confirm_yes", "Yes, flash");
        en.insert("confirm_no", "Cancel");
        en.insert("stage_unmounting", "Unmounting");
        en.insert("stage_downloading", "Downloading");
        en.insert("stage_writing", "Writing");
        en.insert("stage_verifying", "Verifying");
        en.insert("stage_formatting", "Formatting");
        en.insert("stage_extracting", "Extracting");
        en.insert("stage_checking", "Checking");
        en.insert("stage_splitting", "Splitting WIM");
        en.insert("stage_installing", "Installing");
        en.insert("stage_done", "Done");
        en.insert("error_no_source", "Select source and target first");
        en.insert("error_title", "Error");
        en.insert("about_title", "About dskcopy");
        en.insert("about_body", "dskcopy — fast, safe disk image writer\nVersion 0.3.0\n\nBuilt with Rust + Slint\nhttps://github.com/dskcopy/dskcopy");
        table.insert("en", en);

        // ── Placeholder for future languages ──────────────────────────────
        // Add entries like:
        //   let mut de = en.clone();  // copy English
        //   de.insert("btn_flash_now", "Jetzt flashen");
        //   table.insert("de", de);

        Mutex::new(table)
    })
}

/// Set the active locale. Defaults to "en" if the locale is not found.
pub fn set_locale(locale: &str) {
    let mut guard = CURRENT_LOCALE
        .get_or_init(|| Mutex::new("en".to_string()))
        .lock()
        .unwrap();
    *guard = locale.to_string();
}

/// Translate a message key. Returns the English fallback if key or locale is missing.
pub fn t(key: &'static str) -> &'static str {
    let loc = locale();
    let table = message_table().lock().unwrap();

    if let Some(messages) = table.get(loc.as_str()) {
        if let Some(val) = messages.get(key) {
            return val;
        }
    }
    // English fallback
    if let Some(en) = table.get("en") {
        if let Some(val) = en.get(key) {
            return val;
        }
    }
    key // Return the key itself as last resort
}
