use crate::{DeviceInfo, ListOptions, PlatformBackend};
use std::fs;
use std::path::Path;
use std::process::Command;

pub struct LinuxBackend;

impl PlatformBackend for LinuxBackend {
    fn list_devices(&self, opts: &ListOptions) -> Result<Vec<DeviceInfo>, String> {
        let boot_dev = self.get_boot_device();
        let mut out = Vec::new();

        let sys_block = Path::new("/sys/block");
        if !sys_block.exists() {
            return Err(
                "Cannot enumerate drives: /sys/block not found. Are you running on Linux?"
                    .to_string(),
            );
        }

        let entries =
            fs::read_dir(sys_block).map_err(|e| format!("Cannot read /sys/block: {}", e))?;

        for entry in entries.flatten() {
            let dev_name = entry.file_name().to_string_lossy().to_string();
            let dev_path = format!("/dev/{}", dev_name);
            let sys_path = entry.path();

            // Skip loop, ram, zram, sr devices
            if dev_name.starts_with("loop")
                || dev_name.starts_with("ram")
                || dev_name.starts_with("zram")
                || dev_name.starts_with("sr")
            {
                continue;
            }

            let removable = read_sys_u8(&sys_path.join("removable")).unwrap_or(0) == 1;
            let is_usb = detect_usb(&sys_path);

            // Determine if system / boot drive
            let is_boot = boot_dev.as_deref().map(|b| b == dev_path).unwrap_or(false);
            let is_system = !removable || !is_usb || is_boot;

            if !opts.show_all {
                if !removable || !is_usb {
                    continue;
                }
                if is_system {
                    continue;
                }
            }

            let size_blocks = read_sys_u64(&sys_path.join("size")).unwrap_or(0);
            let size_bytes = size_blocks * 512;

            let name =
                read_sys_str(&sys_path.join("device/model")).unwrap_or_else(|| dev_name.clone());

            out.push(DeviceInfo {
                id: dev_path.clone(),
                name: name.trim().to_string(),
                size_bytes,
                removable,
                bus: if is_usb {
                    "USB".to_string()
                } else {
                    "Unknown".to_string()
                },
                raw_device: Some(dev_path.clone()),
                is_system_drive: is_system,
            });
        }

        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    fn write_image(&self, _image_path: &str, _device_id: &str) -> Result<(), String> {
        Ok(())
    }

    fn get_boot_device(&self) -> Option<String> {
        // Parse /proc/mounts or use findmnt to find the device backing /
        let output = Command::new("findmnt")
            .args(["-n", "-o", "SOURCE", "/"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let source = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if source.is_empty() {
            return None;
        }
        // Strip partition suffix: /dev/sda1 → /dev/sda, /dev/nvme0n1p1 → /dev/nvme0n1
        let dev = strip_partition_suffix(&source);
        Some(dev)
    }
}

/// Unmount all partitions of a block device via udisksctl or umount.
pub fn unmount_device(dev: &str) -> Result<(), String> {
    // Try udisksctl first
    let output = Command::new("udisksctl")
        .args(["unmount", "--block-device", dev])
        .output();

    if let Ok(o) = output {
        if o.status.success() {
            return Ok(());
        }
    }

    // Fall back to umount
    let status = Command::new("umount")
        .arg(dev)
        .status()
        .map_err(|e| format!("Cannot run umount: {}", e))?;
    if !status.success() {
        return Err(format!(
            "Could not unmount '{}'. Close any apps using it and try again.",
            dev
        ));
    }
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn read_sys_u8(path: &Path) -> Option<u8> {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

fn read_sys_u64(path: &Path) -> Option<u64> {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

fn read_sys_str(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// Detect whether a /sys/block/<dev> entry is connected via USB.
fn detect_usb(sys_path: &Path) -> bool {
    // Walk up the symlink target looking for a USB device node
    let real = match sys_path.read_link().ok().or_else(|| {
        // /sys/block entries are sometimes symlinks; resolve them
        fs::canonicalize(sys_path).ok()
    }) {
        Some(p) => p,
        None => return false,
    };

    // Check for "usb" anywhere in the resolved path
    real.to_string_lossy().contains("/usb")
}

/// /dev/sda1 → /dev/sda, /dev/nvme0n1p2 → /dev/nvme0n1
fn strip_partition_suffix(dev: &str) -> String {
    // nvme pattern: nvme0n1p1 → nvme0n1
    if let Some(base) = dev.strip_prefix("/dev/nvme") {
        if let Some(pos) = base.rfind('p') {
            if base[pos + 1..].chars().all(|c| c.is_ascii_digit()) {
                return format!("/dev/nvme{}", &base[..pos]);
            }
        }
    }
    // sd pattern: sda1 → sda
    if dev.len() > 5 {
        let last = &dev[dev.len() - 1..];
        if last.chars().all(|c| c.is_ascii_digit()) {
            return dev[..dev.len() - 1].to_string();
        }
    }
    dev.to_string()
}
