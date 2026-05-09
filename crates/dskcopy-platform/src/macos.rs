use crate::{DeviceInfo, ListOptions, PlatformBackend};
use plist::Value;
use std::path::Path;
use std::process::Command;

pub struct MacOsBackend;

impl PlatformBackend for MacOsBackend {
    fn list_devices(&self, opts: &ListOptions) -> Result<Vec<DeviceInfo>, String> {
        let boot_dev = self.get_boot_device();

        // When show_all is false, ask diskutil to only list external drives.
        let args: &[&str] = if opts.show_all {
            &["list", "-plist"]
        } else {
            &["list", "-plist", "external"]
        };

        let output = Command::new("diskutil")
            .args(args)
            .output()
            .map_err(|_| "Could not enumerate drives. Please ensure diskutil is available.".to_string())?;
        if !output.status.success() {
            return Err("Could not enumerate drives. Please check macOS disk access permissions.".to_string());
        }

        let value = Value::from_reader_xml(output.stdout.as_slice())
            .map_err(|_| "Could not parse drive list returned by macOS.".to_string())?;
        let root = value
            .as_dictionary()
            .ok_or_else(|| "Unexpected drive list format from macOS.".to_string())?;

        let mut out = Vec::new();
        let all_disks = root
            .get("AllDisksAndPartitions")
            .and_then(Value::as_array)
            .ok_or_else(|| "No disk metadata found from macOS.".to_string())?;

        for disk in all_disks {
            let Some(d) = disk.as_dictionary() else {
                continue;
            };
            let Some(dev_id) = d.get("DeviceIdentifier").and_then(Value::as_string) else {
                continue;
            };
            let dev_path = format!("/dev/{dev_id}");

            let info = match fetch_disk_info(dev_id) {
                Ok(i) => i,
                Err(_) => continue,
            };

            let removable = info
                .get("RemovableMedia")
                .and_then(Value::as_boolean)
                .unwrap_or(false);
            let removable_or_external = info
                .get("RemovableMediaOrExternalDevice")
                .and_then(Value::as_boolean)
                .unwrap_or(false);
            let ejectable = info
                .get("Ejectable")
                .and_then(Value::as_boolean)
                .unwrap_or(false);
            let protocol = info
                .get("BusProtocol")
                .and_then(Value::as_string)
                .unwrap_or("Unknown");
            let is_usb = protocol.eq_ignore_ascii_case("USB");

            // A drive is "external/targetable" if it's USB AND (removable OR external OR ejectable)
            let is_external_target = is_usb && (removable || removable_or_external || ejectable);

            // Determine if this is a system/internal drive
            let is_internal = !is_external_target || is_internal_drive(&info);
            let is_boot = boot_dev
                .as_deref()
                .map(|b| b == dev_path || b == format!("/dev/{}", dev_id))
                .unwrap_or(false);
            let is_system = is_internal || is_boot;

            // Filter logic: when not showing all, skip non-external or system drives
            if !opts.show_all {
                if !is_external_target {
                    continue;
                }
                if is_system {
                    continue;
                }
            }

            let size_bytes = info
                .get("Size")
                .and_then(Value::as_unsigned_integer)
                .unwrap_or(0);
            let name = info
                .get("MediaName")
                .and_then(Value::as_string)
                .or_else(|| info.get("VolumeName").and_then(Value::as_string))
                .unwrap_or(dev_id)
                .to_string();

            out.push(DeviceInfo {
                id: dev_path.clone(),
                name,
                size_bytes,
                removable: is_external_target,
                bus: protocol.to_string(),
                raw_device: Some(to_raw_device(&dev_path)),
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
        // Ask diskutil for info about /
        let output = Command::new("diskutil")
            .args(["info", "-plist", "/"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let value = Value::from_reader_xml(output.stdout.as_slice()).ok()?;
        let dict = value.into_dictionary()?;
        // The whole-disk device identifier (e.g. "disk0")
        let parent = dict
            .get("ParentWholeDisk")
            .or_else(|| dict.get("DeviceIdentifier"))
            .and_then(Value::as_string)?;
        Some(format!("/dev/{parent}"))
    }
}

/// Returns true if diskutil metadata indicates an internal/APFS system volume.
fn is_internal_drive(info: &plist::Dictionary) -> bool {
    // "Internal" flag
    if info.get("Internal").and_then(Value::as_boolean).unwrap_or(false) {
        return true;
    }
    // APFS container → likely system
    if let Some(t) = info.get("FilesystemType").and_then(Value::as_string) {
        if t.eq_ignore_ascii_case("apfs") {
            return true;
        }
    }
    false
}

fn fetch_disk_info(device_id: &str) -> Result<plist::Dictionary, String> {
    let output = Command::new("diskutil")
        .args(["info", "-plist", device_id])
        .output()
        .map_err(|_| "Failed to inspect disk metadata.".to_string())?;
    if !output.status.success() {
        return Err("Failed to inspect disk metadata.".to_string());
    }
    let value = Value::from_reader_xml(output.stdout.as_slice())
        .map_err(|_| "Failed to parse disk metadata.".to_string())?;
    value
        .into_dictionary()
        .ok_or_else(|| "Unexpected disk metadata format from macOS.".to_string())
}

pub fn to_raw_device(path: &str) -> String {
    if let Some(name) = Path::new(path).file_name().and_then(|s| s.to_str()) {
        if name.starts_with("disk") {
            let suffix = name.trim_start_matches("disk");
            return format!("/dev/rdisk{suffix}");
        }
        if name.starts_with("rdisk") {
            return path.to_string();
        }
    }
    path.to_string()
}
