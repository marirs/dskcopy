//! macOS createinstallmedia integration.
//!
//! Detects macOS full-installer DMGs and delegates to the createinstallmedia
//! binary embedded inside the installer app bundle.

use crate::image::{FlashError, FlashResult};
use crate::{ProgressInfo, Stage};
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

/// Returns true if `dmg_path` contains a macOS full-installer app bundle
/// with a createinstallmedia binary.
#[cfg(target_os = "macos")]
pub fn detect_macos_installer_dmg(dmg_path: &str) -> bool {
    find_createinstallmedia(dmg_path).is_some()
}

#[cfg(not(target_os = "macos"))]
pub fn detect_macos_installer_dmg(_dmg_path: &str) -> bool {
    false
}

/// Flash a macOS installer DMG using createinstallmedia.
///
/// Flow:
/// 1. Mount the DMG (read-only, no browse)
/// 2. Find createinstallmedia inside Install macOS*.app
/// 3. Erase target with diskutil
/// 4. Run createinstallmedia --volume /Volumes/Untitled --nointeraction
/// 5. Stream stdout for progress
#[cfg(target_os = "macos")]
pub fn flash_macos_installer(
    dmg_path: &str,
    target: &str,
    cb: &mut dyn FnMut(ProgressInfo),
) -> Result<FlashResult, FlashError> {
    cb(ProgressInfo {
        stage: Stage::Installing,
        bytes_done: 0,
        total_bytes: 0,
        percent: 0.0,
        mbps: 0.0,
        download_mbps: 0.0,
        eta_secs: 0,
    });

    // 1. Mount DMG
    let mount_point = mount_dmg(dmg_path)?;
    let _guard = DmgUnmountGuard(mount_point.clone());

    // 2. Find createinstallmedia
    let cim_path = find_createinstallmedia_in(&mount_point).ok_or_else(|| {
        FlashError::User(
            "No createinstallmedia found in DMG. Not a macOS full installer?".to_string(),
        )
    })?;

    // 3. Erase target with JHFS+
    let disk_id = target
        .trim_start_matches("/dev/rdisk")
        .trim_start_matches("/dev/disk");
    let erase_status = Command::new("diskutil")
        .args([
            "eraseDisk",
            "JHFS+",
            "Untitled",
            "GPT",
            &format!("/dev/disk{}", disk_id),
        ])
        .status()
        .map_err(|e| FlashError::User(format!("diskutil eraseDisk failed: {e}")))?;
    if !erase_status.success() {
        return Err(FlashError::User(
            "diskutil eraseDisk failed. Check target device and permissions.".to_string(),
        ));
    }

    // 4. Run createinstallmedia
    let mut child = Command::new("sudo")
        .args([
            cim_path.as_str(),
            "--volume",
            "/Volumes/Untitled",
            "--nointeraction",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| FlashError::User(format!("Failed to spawn createinstallmedia: {e}")))?;

    // 5. Stream output for progress
    let stdout = child.stdout.take().unwrap();
    let reader = BufReader::new(stdout);
    let mut last_percent = 0.0f64;

    for line in reader.lines() {
        let line = line.unwrap_or_default();
        // createinstallmedia outputs lines like: "Copying installation files…  1%"
        if let Some(pct) = parse_percent(&line) {
            last_percent = pct;
        }
        cb(ProgressInfo {
            stage: Stage::Installing,
            bytes_done: 0,
            total_bytes: 0,
            percent: last_percent,
            mbps: 0.0,
            download_mbps: 0.0,
            eta_secs: 0,
        });
    }

    let status = child
        .wait()
        .map_err(|e| FlashError::User(format!("createinstallmedia wait failed: {e}")))?;
    if !status.success() {
        return Err(FlashError::User(
            "createinstallmedia exited with an error. Check that you have admin privileges.".to_string(),
        ));
    }

    Ok(FlashResult {
        bytes_written: 0, // createinstallmedia handles writes internally
        sha256: String::new(),
        detected_type: "macOS Installer".to_string(),
    })
}

#[cfg(not(target_os = "macos"))]
pub fn flash_macos_installer(
    _dmg_path: &str,
    _target: &str,
    _cb: &mut dyn FnMut(ProgressInfo),
) -> Result<FlashResult, FlashError> {
    Err(FlashError::User(
        "createinstallmedia is only supported on macOS.".to_string(),
    ))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn mount_dmg(dmg_path: &str) -> Result<String, FlashError> {
    let output = Command::new("hdiutil")
        .args(["attach", "-readonly", "-nobrowse", "-plist", dmg_path])
        .output()
        .map_err(|e| FlashError::User(format!("hdiutil attach failed: {e}")))?;

    if !output.status.success() {
        return Err(FlashError::User(
            "hdiutil attach failed. Ensure the DMG is not already mounted.".to_string(),
        ));
    }

    // Parse plist output to find mount point
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_hdiutil_mount_point(&stdout)
        .ok_or_else(|| FlashError::User("Could not determine DMG mount point.".to_string()))
}

#[cfg(target_os = "macos")]
fn parse_hdiutil_mount_point(plist_text: &str) -> Option<String> {
    // Look for <string>/Volumes/...</string> pattern
    for line in plist_text.lines() {
        let trimmed = line.trim();
        if let Some(inner) = trimmed.strip_prefix("<string>/Volumes/") {
            if let Some(vol) = inner.strip_suffix("</string>") {
                return Some(format!("/Volumes/{}", vol));
            }
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn find_createinstallmedia_in(mount_point: &str) -> Option<String> {
    // Look for Install macOS*.app/Contents/Resources/createinstallmedia
    let pattern = format!("{}/Install macOS*", mount_point);
    let paths = glob::glob(&pattern).ok()?;
    for entry in paths.flatten() {
        let cim = entry
            .join("Contents/Resources/createinstallmedia");
        if cim.exists() {
            return Some(cim.to_string_lossy().to_string());
        }
    }
    // Also check for Install OS X
    let pattern2 = format!("{}/Install OS X*", mount_point);
    if let Ok(paths2) = glob::glob(&pattern2) {
        for entry in paths2.flatten() {
            let cim = entry.join("Contents/Resources/createinstallmedia");
            if cim.exists() {
                return Some(cim.to_string_lossy().to_string());
            }
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn find_createinstallmedia(dmg_path: &str) -> Option<String> {
    if let Ok(mount_point) = mount_dmg(dmg_path) {
        let result = find_createinstallmedia_in(&mount_point);
        // Detach
        let _ = Command::new("hdiutil")
            .args(["detach", &mount_point])
            .status();
        return result;
    }
    None
}

/// Parse a percentage from a createinstallmedia output line.
fn parse_percent(line: &str) -> Option<f64> {
    // Lines look like: "Copying installation files…  42%"
    let pct_str = line.split_whitespace().last()?;
    let digits = pct_str.trim_end_matches('%');
    digits.parse::<f64>().ok()
}

/// RAII guard that detaches a DMG on drop.
#[cfg(target_os = "macos")]
struct DmgUnmountGuard(String);

#[cfg(target_os = "macos")]
impl Drop for DmgUnmountGuard {
    fn drop(&mut self) {
        let _ = Command::new("hdiutil")
            .args(["detach", &self.0])
            .status();
    }
}


