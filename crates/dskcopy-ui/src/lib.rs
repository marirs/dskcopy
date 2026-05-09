slint::include_modules!();

use dskcopy_core::detect::{detect_image_type, RawImageType};
use dskcopy_core::download;
use dskcopy_core::image::{flash, normalize_target_device, parse_image};
use dskcopy_core::source::SourceDescriptor;
use dskcopy_core::{
    FilesystemType, FlashMode, FlashOptions, PartitionScheme, ProgressInfo, Stage, VerifyMode,
};
use dskcopy_platform::macos::MacOsBackend;
use dskcopy_platform::{ListOptions, PlatformBackend};
use i_slint_backend_winit::winit::event::WindowEvent;
use slint::{ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

// ── Drag-and-drop handler ─────────────────────────────────────────────────────

struct DeferredDropHandler {
    pending_weak: Arc<Mutex<Option<slint::Weak<AppWindow>>>>,
}

impl i_slint_backend_winit::CustomApplicationHandler for DeferredDropHandler {
    fn window_event(
        &mut self,
        _event_loop: &i_slint_backend_winit::winit::event_loop::ActiveEventLoop,
        _window_id: i_slint_backend_winit::winit::window::WindowId,
        _winit_window: Option<&i_slint_backend_winit::winit::window::Window>,
        _slint_window: Option<&slint::Window>,
        event: &WindowEvent,
    ) -> i_slint_backend_winit::EventResult {
        if let WindowEvent::DroppedFile(path) = event {
            let path_str = path.to_string_lossy().to_string();
            if let Ok(guard) = self.pending_weak.lock() {
                if let Some(app_weak) = guard.clone() {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(app) = app_weak.upgrade() {
                            app.invoke_drop_file(path_str.into());
                        }
                    });
                }
            }
        }
        i_slint_backend_winit::EventResult::Propagate
    }
}

pub fn launch() -> Result<(), slint::PlatformError> {
    let pending_weak: Arc<Mutex<Option<slint::Weak<AppWindow>>>> = Arc::new(Mutex::new(None));
    let pending_weak_clone = pending_weak.clone();

    let backend = i_slint_backend_winit::Backend::builder()
        .with_window_attributes_hook(|attrs| attrs.with_resizable(false))
        .with_custom_application_handler(Box::new(DeferredDropHandler {
            pending_weak: pending_weak_clone,
        }))
        .build()?;
    slint::platform::set_platform(Box::new(backend)).ok();

    let app = AppWindow::new()?;

    *pending_weak.lock().unwrap() = Some(app.as_weak());

    let state = Rc::new(RefCell::new(UiState::default()));
    let download_cancel: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    // ── Populate ISO catalog & languages ───────────────────────────────────
    {
        let versions: Vec<SharedString> = download::list_catalog()
            .iter()
            .map(|e| SharedString::from(e.name))
            .collect();
        app.set_iso_versions(ModelRc::new(VecModel::from(versions)));
        // Initialize languages from first entry
        let langs = languages_for_index(0);
        app.set_iso_language_names(ModelRc::new(VecModel::from(langs)));
        app.set_iso_version_index(0);
        app.set_iso_language_index(0);
    }

    // Initial drive load
    {
        let backend = MacOsBackend;
        let drives = backend.list_devices(&ListOptions { show_all: false }).unwrap_or_default();
        state.borrow_mut().drives = drives;
        refresh_ui(&app, &state.borrow());
    }

    // ── refresh-drives ─────────────────────────────────────────────────────
    {
        let appw = app.as_weak();
        let state = state.clone();
        app.on_refresh_drives(move || {
            let show_all = state.borrow().show_all;
            let backend = MacOsBackend;
            let drives = backend
                .list_devices(&ListOptions { show_all })
                .unwrap_or_default();
            let mut s = state.borrow_mut();
            s.drives = drives;
            s.has_selected = false;
            s.selected = 0;
            if let Some(a) = appw.upgrade() {
                refresh_ui(&a, &s);
            }
        });
    }

    // ── select-drive ────────────────────────────────────────────────────────
    {
        let appw = app.as_weak();
        let state = state.clone();
        app.on_select_drive(move |idx| {
            let mut s = state.borrow_mut();
            let new_sel = idx as usize;
            if new_sel < s.drives.len() {
                s.selected = new_sel;
                s.has_selected = true;
            }
            if let Some(a) = appw.upgrade() {
                refresh_ui(&a, &s);
            }
        });
    }

    // ── toggle-show-all ────────────────────────────────────────────────────
    {
        let appw = app.as_weak();
        let state = state.clone();
        app.on_toggle_show_all(move |show_all| {
            let backend = MacOsBackend;
            let drives = backend
                .list_devices(&ListOptions { show_all })
                .unwrap_or_default();
            let mut s = state.borrow_mut();
            s.show_all = show_all;
            s.drives = drives;
            s.has_selected = false;
            s.selected = 0;
            if let Some(a) = appw.upgrade() {
                refresh_ui(&a, &s);
            }
        });
    }

    // ── pick-source ────────────────────────────────────────────────────────
    {
        let appw = app.as_weak();
        let state = state.clone();
        app.on_pick_source(move || {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter(
                    "Disk Images",
                    &["iso", "img", "dmg", "raw", "gz", "xz", "zip", "bz2", "vhd", "vhdx", "ffu"],
                )
                .add_filter("All Files", &["*"])
                .pick_file()
            {
                let path_str = path.to_string_lossy().to_string();
                let (detected, is_dmg, is_win) = classify_source(&path_str);
                let mut s = state.borrow_mut();
                s.source = Some(path_str.clone());
                s.source_is_url = false;
                if let Some(a) = appw.upgrade() {
                    a.set_detected_type(detected.into());
                    a.set_show_macos_option(is_dmg);
                    a.set_show_win11_option(is_win);
                    refresh_ui(&a, &s);
                }
            }
        });
    }

    // ── drop-file (drag-and-drop) ──────────────────────────────────────────
    {
        let appw = app.as_weak();
        let state = state.clone();
        app.on_drop_file(move |path| {
            let path_str = path.to_string();
            if path_str.is_empty() {
                return;
            }
            let (detected, is_dmg, is_win) = classify_source(&path_str);
            let mut s = state.borrow_mut();
            s.source = Some(path_str.clone());
            s.source_is_url = false;
            if let Some(a) = appw.upgrade() {
                a.set_detected_type(detected.into());
                a.set_show_macos_option(is_dmg);
                a.set_show_win11_option(is_win);
                a.set_error_message("".into());
                refresh_ui(&a, &s);
            }
        });
    }

    // ── dismiss-error ──────────────────────────────────────────────────────
    {
        let appw = app.as_weak();
        app.on_dismiss_error(move || {
            if let Some(a) = appw.upgrade() {
                a.set_error_message("".into());
            }
        });
    }

    // ── show-about-dialog ──────────────────────────────────────────────────
    {
        let appw = app.as_weak();
        app.on_show_about_dialog(move || {
            if let Some(a) = appw.upgrade() {
                a.set_show_about(true);
            }
        });
    }

    // ── close-about ────────────────────────────────────────────────────────
    {
        let appw = app.as_weak();
        app.on_close_about(move || {
            if let Some(a) = appw.upgrade() {
                a.set_show_about(false);
            }
        });
    }

    // ── select-iso-version: refresh language list ──────────────────────────
    {
        let appw = app.as_weak();
        app.on_select_iso_version(move |idx| {
            let langs = languages_for_index(idx as usize);
            if let Some(a) = appw.upgrade() {
                a.set_iso_language_names(ModelRc::new(VecModel::from(langs)));
                a.set_iso_language_index(0);
            }
        });
    }

    // ── cancel-iso-download ────────────────────────────────────────────────
    {
        let cancel = download_cancel.clone();
        app.on_cancel_iso_download(move || {
            cancel.store(true, Ordering::SeqCst);
        });
    }

    // ── start-iso-download ─────────────────────────────────────────────────
    {
        let appw = app.as_weak();
        let cancel = download_cancel.clone();
        app.on_start_iso_download(move |version_idx, lang_idx| {
            let entry = match download::list_catalog().get(version_idx as usize) {
                Some(e) => e.clone(),
                None => return,
            };
            let locale = entry
                .locales
                .get(lang_idx as usize)
                .copied()
                .unwrap_or("en-us")
                .to_string();

            // Compute output path: ~/Downloads/<id>-<locale>.iso, fallback to temp
            let dl_dir = dirs::download_dir().unwrap_or_else(std::env::temp_dir);
            let mut out_path: PathBuf = dl_dir;
            out_path.push(format!("{}-{}.iso", entry.id, locale));
            let out_str = out_path.to_string_lossy().to_string();

            cancel.store(false, Ordering::SeqCst);

            if let Some(a) = appw.upgrade() {
                a.set_error_message("".into());
                a.set_download_active(true);
                a.set_progress_value(0);
                a.set_status_text(format!("Fetching link for {}…", entry.name).into());
            }

            let appw2 = appw.clone();
            let cancel2 = cancel.clone();
            let entry_name = entry.name.to_string();
            std::thread::spawn(move || {
                let url_result = download::get_download_url(&entry, &locale);
                if cancel2.load(Ordering::SeqCst) {
                    finish_download_cancelled(&appw2);
                    return;
                }
                let url = match url_result {
                    Ok(u) => u,
                    Err(e) => {
                        let aw = appw2.clone();
                        let msg = format!("Could not get download link: {}", e);
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(a) = aw.upgrade() {
                                a.set_download_active(false);
                                a.set_progress_value(0);
                                a.set_status_text("".into());
                                a.set_error_message(msg.into());
                            }
                        });
                        return;
                    }
                };

                {
                    let aw = appw2.clone();
                    let name = entry_name.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(a) = aw.upgrade() {
                            a.set_status_text(format!("Downloading {}…", name).into());
                        }
                    });
                }

                let aw_cb = appw2.clone();
                let cancel_cb = cancel2.clone();
                let name_cb = entry_name.clone();
                let result = download::download_iso(&url, &out_str, move |done, total| {
                    if cancel_cb.load(Ordering::SeqCst) {
                        return;
                    }
                    let pct = if total > 0 { (done * 100 / total).min(100) } else { 0 };
                    let mb_done = done as f64 / (1024.0 * 1024.0);
                    let mb_total = total as f64 / (1024.0 * 1024.0);
                    let status = if total > 0 {
                        format!("Downloading {}  ({:.0} / {:.0} MiB)", name_cb, mb_done, mb_total)
                    } else {
                        format!("Downloading {}  ({:.0} MiB)", name_cb, mb_done)
                    };
                    let aw = aw_cb.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(a) = aw.upgrade() {
                            a.set_progress_value(pct as i32);
                            a.set_status_text(status.into());
                        }
                    });
                });

                if cancel2.load(Ordering::SeqCst) {
                    // Best-effort cleanup
                    let _ = std::fs::remove_file(&out_str);
                    finish_download_cancelled(&appw2);
                    return;
                }

                match result {
                    Ok(_bytes) => {
                        let aw = appw2.clone();
                        let path_for_state = out_str.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(a) = aw.upgrade() {
                                a.set_download_active(false);
                                a.set_progress_value(100);
                                a.set_status_text(
                                    format!("Downloaded → {}", path_for_state).into(),
                                );
                                // Reuse drop-file handler to set source state + UI consistently
                                a.invoke_drop_file(path_for_state.into());
                            }
                        });
                    }
                    Err(e) => {
                        let aw = appw2.clone();
                        let msg = format!("Download failed: {}", e);
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(a) = aw.upgrade() {
                                a.set_download_active(false);
                                a.set_progress_value(0);
                                a.set_status_text("".into());
                                a.set_error_message(msg.into());
                            }
                        });
                    }
                }
            });
        });
    }

    // ── flash-now (shows confirmation first) ───────────────────────────────
    {
        let appw = app.as_weak();
        let state = state.clone();
        app.on_flash_now(move || {
            let s = state.borrow();
            if !s.has_selected || s.drives.is_empty() || s.source.is_none() {
                if let Some(a) = appw.upgrade() {
                    a.set_error_message("Pick an image and a target drive first.".into());
                }
                return;
            }
            let tgt = s.drives[s.selected.min(s.drives.len() - 1)].id.clone();
            if let Some(a) = appw.upgrade() {
                a.set_confirm_message(
                    format!("This will ERASE all data on {}. Are you sure?", tgt).into(),
                );
                a.set_show_confirm(true);
            }
        });
    }

    // ── cancel-flash ───────────────────────────────────────────────────────
    {
        let appw = app.as_weak();
        app.on_cancel_flash(move || {
            if let Some(a) = appw.upgrade() {
                a.set_show_confirm(false);
            }
        });
    }

    // ── confirm-flash (actual flash execution) ─────────────────────────────
    {
        let appw = app.as_weak();
        let state = state.clone();
        app.on_confirm_flash(move || {
            if let Some(a) = appw.upgrade() {
                a.set_show_confirm(false);
            }

            let (source_input, target, format_first, macos_installer, bypass_requirements) = {
                let s = state.borrow();
                if s.drives.is_empty() || s.source.is_none() {
                    return;
                }
                let src = s.source.clone().unwrap_or_default();
                let tgt = s.drives[s.selected.min(s.drives.len() - 1)].id.clone();
                (src, tgt, s.format_first, s.macos_installer, s.bypass_requirements)
            };

            if let Some(a) = appw.upgrade() {
                a.set_status_text("Starting…".into());
                a.set_progress_value(0);
                a.set_download_speed("".into());
                a.set_is_flashing(true);
                a.set_error_message("".into());
            }

            let appw2 = appw.clone();
            std::thread::spawn(move || {
                let raw_target = normalize_target_device(&target);

                let opts = FlashOptions {
                    block_size: 1024 * 1024,
                    verify: true,
                    verify_mode: VerifyMode::ByteByByte,
                    format_before_write: format_first,
                    format_scheme: PartitionScheme::Gpt,
                    format_fs: FilesystemType::Fat32,
                    flash_mode: FlashMode::RawWrite,
                    persistent_mb: None,
                    macos_installer,
                    bypass_requirements,
                };

                let aw = appw2.clone();
                let mut cb = move |p: ProgressInfo| {
                    let pct = p.percent.clamp(0.0, 100.0).round() as i32;
                    let stage_str = stage_label(p.stage).to_string();
                    let dl_speed = if p.download_mbps > 0.0 {
                        format!("Download: {:.1} MB/s", p.download_mbps)
                    } else {
                        String::new()
                    };
                    let status = if p.total_bytes > 0 {
                        format!("{}: {}%", stage_str, pct)
                    } else {
                        format!("{}: {} bytes", stage_str, p.bytes_done)
                    };
                    let aw2 = aw.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(a) = aw2.upgrade() {
                            a.set_progress_value(pct);
                            a.set_status_text(status.into());
                            a.set_download_speed(dl_speed.into());
                        }
                    });
                };

                let result = flash(&source_input, &raw_target, opts, Some(&mut cb));

                let aw3 = appw2.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(a) = aw3.upgrade() {
                        a.set_is_flashing(false);
                        match result {
                            Ok(r) => {
                                a.set_status_text(
                                    format!("Done — {} bytes written", r.bytes_written).into(),
                                );
                                a.set_progress_value(100);
                                a.set_download_speed("".into());
                            }
                            Err(e) => {
                                a.set_status_text("Failed.".into());
                                a.set_error_message(format!("Error: {}", e).into());
                            }
                        }
                    }
                });
            });
        });
    }

    app.run()
}

fn finish_download_cancelled(appw: &slint::Weak<AppWindow>) {
    let aw = appw.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(a) = aw.upgrade() {
            a.set_download_active(false);
            a.set_progress_value(0);
            a.set_status_text("Download cancelled.".into());
        }
    });
}

fn languages_for_index(idx: usize) -> Vec<SharedString> {
    download::list_catalog()
        .get(idx)
        .map(|e| {
            e.locales
                .iter()
                .map(|l| SharedString::from(locale_display_name(l)))
                .collect()
        })
        .unwrap_or_else(|| vec![SharedString::from("English (US)")])
}

fn locale_display_name(code: &str) -> &'static str {
    match code {
        "ar-sa" => "Arabic",
        "bg-bg" => "Bulgarian",
        "cs-cz" => "Czech",
        "da-dk" => "Danish",
        "de-de" => "German",
        "el-gr" => "Greek",
        "en-gb" => "English (UK)",
        "en-us" => "English (US)",
        "es-es" => "Spanish",
        "es-mx" => "Spanish (Mexico)",
        "et-ee" => "Estonian",
        "fi-fi" => "Finnish",
        "fr-ca" => "French (Canada)",
        "fr-fr" => "French",
        "he-il" => "Hebrew",
        "hr-hr" => "Croatian",
        "hu-hu" => "Hungarian",
        "it-it" => "Italian",
        "ja-jp" => "Japanese",
        "ko-kr" => "Korean",
        "lt-lt" => "Lithuanian",
        "lv-lv" => "Latvian",
        "nb-no" => "Norwegian",
        "nl-nl" => "Dutch",
        "pl-pl" => "Polish",
        "pt-br" => "Portuguese (Brazil)",
        "pt-pt" => "Portuguese",
        "ro-ro" => "Romanian",
        "ru-ru" => "Russian",
        "sk-sk" => "Slovak",
        "sl-si" => "Slovenian",
        "sr-latn-rs" => "Serbian",
        "sv-se" => "Swedish",
        "th-th" => "Thai",
        "tr-tr" => "Turkish",
        "uk-ua" => "Ukrainian",
        "zh-cn" => "Chinese (Simplified)",
        "zh-tw" => "Chinese (Traditional)",
        other => {
            // Fallback: return the code unchanged. Use a leak to satisfy 'static.
            // Since locale codes come from a static catalog, this branch is dead
            // in practice, but we need a 'static return. Use a small allowlist
            // of known codes only — otherwise return a generic placeholder.
            let _ = other;
            "Other"
        }
    }
}

// ── State ─────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct UiState {
    drives: Vec<dskcopy_platform::DeviceInfo>,
    selected: usize,
    has_selected: bool,
    source: Option<String>,
    source_is_url: bool,
    show_all: bool,
    format_first: bool,
    macos_installer: bool,
    bypass_requirements: bool,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn refresh_ui(app: &AppWindow, state: &UiState) {
    let items: Vec<DriveItem> = state
        .drives
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let gib = d.size_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
            DriveItem {
                device: d.id.clone().into(),
                name: d.name.clone().into(),
                size: format!("{:.2} GiB", gib).into(),
                is_system: d.is_system_drive,
                is_selected: state.has_selected && i == state.selected,
            }
        })
        .collect();
    let model = ModelRc::new(VecModel::from(items));
    app.set_drives(model);

    if let Some(src) = &state.source {
        if state.source_is_url {
            app.set_source_path(src.clone().into());
            app.set_source_info(format!("URL: {}", src).into());
        } else {
            let info = parse_image(src);
            let basename = std::path::Path::new(src)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| src.clone());
            let gib = info.size_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
            app.set_source_path(src.clone().into());
            app.set_source_info(format!("{}  ({:.2} GiB)", basename, gib).into());
        }
    } else {
        app.set_source_info("".into());
    }

    if state.has_selected {
        if let Some(d) = state.drives.get(state.selected) {
            let gib = d.size_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
            app.set_target_info(
                format!("{}  {}  ({:.2} GiB)", d.id, d.name, gib).into(),
            );
        } else {
            app.set_target_info("".into());
        }
    } else {
        app.set_target_info("".into());
    }
}

fn classify_source(input: &str) -> (String, bool, bool) {
    match SourceDescriptor::detect(input) {
        Ok(src) => {
            let typ = detect_image_type(&src);
            let is_dmg = matches!(
                typ.raw,
                RawImageType::AppleDmg | RawImageType::MacosInstallerDmg
            );
            let is_win = matches!(typ.raw, RawImageType::WindowsIso);
            (typ.description(), is_dmg, is_win)
        }
        Err(_) => (String::new(), false, false),
    }
}

fn stage_label(s: Stage) -> &'static str {
    match s {
        Stage::Unmounting => "Unmounting",
        Stage::Downloading => "Downloading",
        Stage::Writing => "Writing",
        Stage::Verifying => "Verifying",
        Stage::Formatting => "Formatting",
        Stage::Extracting => "Extracting",
        Stage::Checking => "Checking",
        Stage::Splitting => "Splitting WIM",
        Stage::Installing => "Installing",
        Stage::Done => "Done",
    }
}
