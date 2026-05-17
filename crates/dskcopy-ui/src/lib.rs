slint::include_modules!();

use dskcopy_core::detect::{detect_image_type, RawImageType};
use dskcopy_core::image::{flash, normalize_target_device, parse_image};
use dskcopy_core::source::SourceDescriptor;
use dskcopy_core::{
    FilesystemType, FlashMode, FlashOptions, PartitionScheme, ProgressInfo, Stage, VerifyMode,
};
use dskcopy_platform::macos::MacOsBackend;
use dskcopy_platform::{ListOptions, PlatformBackend};
use i_slint_backend_winit::winit::event::WindowEvent;
use slint::{ModelRc, VecModel};
use std::cell::RefCell;
use std::rc::Rc;
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
            let backend = MacOsBackend;
            let drives = backend
                .list_devices(&ListOptions { show_all: false })
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

            let (source_input, target, macos_installer, bypass_requirements) = {
                let s = state.borrow();
                if s.drives.is_empty() || s.source.is_none() {
                    return;
                }
                let src = s.source.clone().unwrap_or_default();
                let tgt = s.drives[s.selected.min(s.drives.len() - 1)].id.clone();
                (src, tgt, s.macos_installer, s.bypass_requirements)
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
                    format_before_write: false,
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

// ── State ─────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct UiState {
    drives: Vec<dskcopy_platform::DeviceInfo>,
    selected: usize,
    has_selected: bool,
    source: Option<String>,
    source_is_url: bool,
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
