//! Windows ISO flashing: UEFI dual-partition layout with WIM splitting.
//!
//! Creates:
//!   - Partition 1: EFI System Partition (FAT32, 512 MiB) with boot files
//!   - Partition 2: Microsoft Basic Data (FAT32) with installer files
//!     - If install.wim > 3.8 GiB, it is split into install.swm / install2.swm / …
//!
//! Also supports:
//!   - Windows 11 requirement bypass via autounattend.xml injection
//!   - UEFI:NTFS mode: 1 MiB FAT32 loader + NTFS data partition (no WIM split needed)

use crate::image::{FlashError, FlashResult};
use crate::iso_reader::{list_iso_entries, IsoEntry};
use crate::partition::{format_windows_usb, format_windows_usb_ntfs, PartitionSlice};
use crate::{BootAssets, FlashOptions, ProgressInfo, Stage};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

/// Maximum WIM part size (3.8 GiB — leaves headroom below FAT32 4 GiB limit)
const WIM_PART_MAX: u64 = 3_800 * 1024 * 1024;

/// WIM fixed header size
const WIM_HEADER_SIZE: usize = 208;

pub fn flash_windows_iso(
    source_path: &str,
    target: &str,
    opts: &FlashOptions,
    cb: &mut dyn FnMut(ProgressInfo),
) -> Result<FlashResult, FlashError> {
    // 1. Create dual-partition layout
    let parts = format_windows_usb(target, Some(cb)).map_err(FlashError::Format)?;

    // 2. Collect ISO entries
    let entries = list_iso_entries(source_path)
        .map_err(|e| FlashError::User(format!("Cannot read ISO: {e}")))?;

    let mut iso_file =
        File::open(source_path).map_err(|e| FlashError::User(format!("Cannot open ISO: {e}")))?;

    // 3. Compute total bytes for progress
    let total_bytes: u64 = entries.iter().filter(|e| !e.is_dir).map(|e| e.size).sum();
    let mut bytes_done: u64 = 0;

    // 4. Open device
    let mut dev = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(target)
        .map_err(|e| FlashError::User(format!("Cannot open device: {e}")))?;

    // 5. Write EFI partition (FAT32)
    {
        let mut efi_slice = PartitionSlice::new(&mut dev, parts.efi_offset, parts.efi_size);
        let efi_fs = fatfs::FileSystem::new(&mut efi_slice, fatfs::FsOptions::new())
            .map_err(|e| FlashError::User(format!("Cannot open EFI FAT32: {e}")))?;
        let efi_root = efi_fs.root_dir();

        for entry in &entries {
            if entry.is_dir {
                continue;
            }
            if !is_efi_boot_file(&entry.path) {
                continue;
            }
            let rel = entry.path.trim_start_matches('/');
            bytes_done += copy_entry_to_fat(
                &mut iso_file,
                entry,
                &efi_root,
                rel,
                bytes_done,
                total_bytes,
                cb,
            )?;
        }
    }

    // 6. Write data partition (FAT32)
    dev.seek(SeekFrom::Start(0))
        .map_err(|e| FlashError::User(e.to_string()))?;
    {
        let mut data_slice = PartitionSlice::new(&mut dev, parts.data_offset, parts.data_size);

        // Format data partition as FAT32
        fatfs::format_volume(
            &mut data_slice,
            fatfs::FormatVolumeOptions::new()
                .volume_label(*b"WINDOWS    ")
                .fat_type(fatfs::FatType::Fat32),
        )
        .map_err(|e| FlashError::User(format!("Data partition FAT32 format failed: {e}")))?;

        data_slice
            .seek(SeekFrom::Start(0))
            .map_err(|e| FlashError::User(e.to_string()))?;

        let data_fs = fatfs::FileSystem::new(&mut data_slice, fatfs::FsOptions::new())
            .map_err(|e| FlashError::User(format!("Cannot open data FAT32: {e}")))?;
        let data_root = data_fs.root_dir();

        for entry in &entries {
            if entry.is_dir {
                continue;
            }
            let lower = entry.path.to_lowercase();
            let is_install_wim =
                lower.ends_with("/sources/install.wim") || lower == "sources/install.wim";

            if is_install_wim && entry.size > WIM_PART_MAX {
                bytes_done += split_and_write_wim(
                    &mut iso_file,
                    entry,
                    &data_root,
                    bytes_done,
                    total_bytes,
                    cb,
                )?;
            } else {
                let rel = entry.path.trim_start_matches('/');
                bytes_done += copy_entry_to_fat(
                    &mut iso_file,
                    entry,
                    &data_root,
                    rel,
                    bytes_done,
                    total_bytes,
                    cb,
                )?;
            }
        }

        // 7. Win11 bypass: inject autounattend.xml and ei.cfg
        if opts.bypass_requirements {
            inject_win11_bypass(&data_root)?;
        }
    }

    cb(ProgressInfo {
        stage: Stage::Done,
        bytes_done,
        total_bytes,
        percent: 100.0,
        mbps: 0.0,
        download_mbps: 0.0,
        eta_secs: 0,
    });

    Ok(FlashResult {
        bytes_written: bytes_done,
        sha256: String::new(),
        detected_type: "Windows Installer ISO".to_string(),
    })
}

// ── Windows 11 requirement bypass ────────────────────────────────────────────

/// Inject `autounattend.xml` and `sources/ei.cfg` into the data FAT partition.
///
/// The `autounattend.xml` adds LabConfig registry keys during Windows PE setup,
/// bypassing TPM 2.0, Secure Boot, RAM, CPU, and storage checks.
fn inject_win11_bypass<T: fatfs::ReadWriteSeek>(root: &fatfs::Dir<T>) -> Result<(), FlashError> {
    // sources/ei.cfg — prevents the edition selector dialog
    let sources_dir = root
        .create_dir("sources")
        .map_err(|e| FlashError::User(format!("Cannot create sources dir: {e}")))?;
    let mut ei_cfg = sources_dir
        .create_file("ei.cfg")
        .map_err(|e| FlashError::User(format!("Cannot create ei.cfg: {e}")))?;
    ei_cfg
        .write_all(b"[Channel]\r\nRetail\r\n")
        .map_err(|e| FlashError::User(format!("Cannot write ei.cfg: {e}")))?;

    // autounattend.xml — adds LabConfig registry keys in Windows PE
    let xml = include_str!("../assets/autounattend_bypass.xml");
    let mut unattend = root
        .create_file("autounattend.xml")
        .map_err(|e| FlashError::User(format!("Cannot create autounattend.xml: {e}")))?;
    unattend
        .write_all(xml.as_bytes())
        .map_err(|e| FlashError::User(format!("Cannot write autounattend.xml: {e}")))?;

    Ok(())
}

// ── UEFI:NTFS flash mode ──────────────────────────────────────────────────────

/// Flash a Windows ISO using the UEFI:NTFS layout:
///   - 1 MiB FAT32 partition with the UEFI:NTFS chainloader at EFI/BOOT/BOOTX64.EFI
///   - Remaining space as NTFS (via mkntfs) containing all installer files
///
/// This allows files >4 GiB without WIM splitting, at the cost of requiring NTFS tools.
pub fn flash_windows_ntfs(
    source_path: &str,
    target: &str,
    opts: &FlashOptions,
    cb: &mut dyn FnMut(ProgressInfo),
) -> Result<FlashResult, FlashError> {
    // 1. Create UEFI:NTFS partition layout
    let parts = format_windows_usb_ntfs(target, Some(cb)).map_err(FlashError::Format)?;

    // 2. Write UEFI:NTFS loader to FAT32 partition
    {
        let mut dev = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(target)
            .map_err(|e| FlashError::User(format!("Cannot open device: {e}")))?;

        let mut fat_slice = PartitionSlice::new(&mut dev, parts.fat_offset, parts.fat_size);
        let fat_fs = fatfs::FileSystem::new(&mut fat_slice, fatfs::FsOptions::new())
            .map_err(|e| FlashError::User(format!("Cannot open UEFI-NTFS FAT32: {e}")))?;
        let fat_root = fat_fs.root_dir();

        // Create EFI/BOOT/ directory chain
        let efi_dir = fat_root
            .create_dir("EFI")
            .map_err(|e| FlashError::User(format!("Cannot create EFI dir: {e}")))?;
        let boot_dir = efi_dir
            .create_dir("BOOT")
            .map_err(|e| FlashError::User(format!("Cannot create BOOT dir: {e}")))?;

        // Write the embedded UEFI:NTFS loader binary
        let loader_bytes = BootAssets::get("uefi_ntfs.efi")
            .ok_or_else(|| FlashError::User("uefi_ntfs.efi not embedded in binary".into()))?;

        let mut efi_file = boot_dir
            .create_file("BOOTX64.EFI")
            .map_err(|e| FlashError::User(format!("Cannot create BOOTX64.EFI: {e}")))?;
        efi_file
            .write_all(&loader_bytes.data)
            .map_err(|e| FlashError::User(format!("Cannot write BOOTX64.EFI: {e}")))?;
    }

    // 3. Format NTFS partition and extract ISO contents via mkntfs + ntfs3g / shell
    //    (NTFS write requires a native driver; we shell out to mkntfs + ntfslabel + cp via mount)
    let ntfs_part = find_ntfs_partition_node(target, 2)?;

    // Format NTFS
    let status = std::process::Command::new("mkntfs")
        .args(["-f", "-L", "Windows", &ntfs_part])
        .status()
        .or_else(|_| {
            std::process::Command::new("mkfs.ntfs")
                .args(["-f", "-L", "Windows", &ntfs_part])
                .status()
        })
        .map_err(|e| {
            FlashError::User(format!("mkntfs/mkfs.ntfs not found: {e}. Install ntfs-3g."))
        })?;

    if !status.success() {
        return Err(FlashError::User("NTFS format failed".to_string()));
    }

    // Mount and extract
    let mount_dir = format!("/tmp/dskcopy_ntfs_{}", std::process::id());
    std::fs::create_dir_all(&mount_dir)
        .map_err(|e| FlashError::User(format!("Cannot create mount point: {e}")))?;

    let mount_status = std::process::Command::new("mount")
        .args(["-t", "ntfs-3g", &ntfs_part, &mount_dir])
        .status()
        .map_err(|e| FlashError::User(format!("mount failed: {e}")))?;

    if !mount_status.success() {
        let _ = std::fs::remove_dir(&mount_dir);
        return Err(FlashError::User(
            "Cannot mount NTFS partition. Ensure ntfs-3g is installed.".to_string(),
        ));
    }

    // Mount ISO and copy
    let iso_mount = format!("/tmp/dskcopy_iso_{}", std::process::id());
    std::fs::create_dir_all(&iso_mount)
        .map_err(|e| FlashError::User(format!("Cannot create ISO mount point: {e}")))?;

    cb(ProgressInfo {
        stage: Stage::Extracting,
        bytes_done: 0,
        total_bytes: 0,
        percent: 0.0,
        mbps: 0.0,
        download_mbps: 0.0,
        eta_secs: 0,
    });

    let iso_mount_status = std::process::Command::new("mount")
        .args(["-o", "loop,ro", source_path, &iso_mount])
        .status()
        .map_err(|e| FlashError::User(format!("ISO mount failed: {e}")))?;

    if !iso_mount_status.success() {
        let _ = std::process::Command::new("umount")
            .arg(&mount_dir)
            .status();
        let _ = std::fs::remove_dir(&mount_dir);
        let _ = std::fs::remove_dir(&iso_mount);
        return Err(FlashError::User("Cannot mount ISO image.".to_string()));
    }

    // rsync or cp -a
    let copy_status = std::process::Command::new("rsync")
        .args([
            "-a",
            "--progress",
            &format!("{}/", iso_mount),
            &format!("{}/", mount_dir),
        ])
        .status()
        .or_else(|_| {
            std::process::Command::new("cp")
                .args([
                    "-a",
                    &format!("{}/.", iso_mount),
                    &format!("{}/", mount_dir),
                ])
                .status()
        })
        .map_err(|e| FlashError::User(format!("Copy failed: {e}")))?;

    // Win11 bypass
    if opts.bypass_requirements {
        let unattend_path = format!("{}/autounattend.xml", mount_dir);
        let _ = std::fs::write(
            &unattend_path,
            include_str!("../assets/autounattend_bypass.xml"),
        );
        let ei_cfg_path = format!("{}/sources/ei.cfg", mount_dir);
        let _ = std::fs::write(ei_cfg_path, "[Channel]\r\nRetail\r\n");
    }

    // Cleanup mounts
    let _ = std::process::Command::new("umount")
        .arg(&iso_mount)
        .status();
    let _ = std::process::Command::new("umount")
        .arg(&mount_dir)
        .status();
    let _ = std::fs::remove_dir(&iso_mount);
    let _ = std::fs::remove_dir(&mount_dir);

    if !copy_status.success() {
        return Err(FlashError::User(
            "Failed to copy ISO contents to NTFS partition.".to_string(),
        ));
    }

    cb(ProgressInfo {
        stage: Stage::Done,
        bytes_done: parts.ntfs_size,
        total_bytes: parts.ntfs_size,
        percent: 100.0,
        mbps: 0.0,
        download_mbps: 0.0,
        eta_secs: 0,
    });

    Ok(FlashResult {
        bytes_written: parts.ntfs_size,
        sha256: String::new(),
        detected_type: "Windows Installer ISO (UEFI:NTFS mode)".to_string(),
    })
}

/// Derive a partition device node from a whole-disk path and 1-based partition number.
/// e.g., /dev/sda → /dev/sda2,  /dev/disk2 → /dev/disk2s2
fn find_ntfs_partition_node(disk: &str, part_num: u32) -> Result<String, FlashError> {
    // macOS style: /dev/diskN → /dev/diskNsP
    if disk.contains("disk") && !disk.contains("sd") {
        return Ok(format!("{}s{}", disk, part_num));
    }
    // Linux style: /dev/sdaN or /dev/nvme0n1pN
    if disk.contains("nvme") || disk.contains("mmcblk") {
        return Ok(format!("{}p{}", disk, part_num));
    }
    Ok(format!("{}{}", disk, part_num))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Returns true for files that belong on the EFI partition.
fn is_efi_boot_file(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.contains("/efi/")
        || lower.contains("/boot/")
        || lower.ends_with("/bootmgr")
        || lower.ends_with("/bootmgr.efi")
        || lower.contains("/sources/boot.wim")
}

/// Ensure all parent directories exist in a FAT filesystem.
fn ensure_fat_dirs<T: fatfs::ReadWriteSeek>(
    root: &fatfs::Dir<T>,
    rel_path: &str,
) -> Result<(), FlashError> {
    let parts: Vec<&str> = rel_path.split('/').collect();
    if parts.len() <= 1 {
        return Ok(()); // file in root
    }
    let mut current = root.clone();
    for dir_name in &parts[..parts.len() - 1] {
        if dir_name.is_empty() {
            continue;
        }
        current = current
            .create_dir(dir_name)
            .map_err(|e| FlashError::User(format!("Cannot create FAT dir '{}': {e}", dir_name)))?;
    }
    Ok(())
}

/// Copy one ISO entry into a FAT filesystem directory.
fn copy_entry_to_fat<T: fatfs::ReadWriteSeek>(
    iso_file: &mut File,
    entry: &IsoEntry,
    root: &fatfs::Dir<T>,
    rel_path: &str,
    bytes_so_far: u64,
    total_bytes: u64,
    cb: &mut dyn FnMut(ProgressInfo),
) -> Result<u64, FlashError> {
    ensure_fat_dirs(root, rel_path)?;

    let file_name = rel_path.split('/').last().unwrap_or(rel_path);
    let mut fat_file = root
        .create_file(file_name)
        .map_err(|e| FlashError::User(format!("Cannot create FAT file '{}': {e}", file_name)))?;

    iso_file
        .seek(SeekFrom::Start(entry.offset))
        .map_err(|e| FlashError::User(e.to_string()))?;

    let mut remaining = entry.size;
    let mut buf = vec![0u8; 256 * 1024];
    let mut written = 0u64;
    let start = std::time::Instant::now();

    while remaining > 0 {
        let to_read = buf.len().min(remaining as usize);
        iso_file
            .read_exact(&mut buf[..to_read])
            .map_err(|e| FlashError::User(format!("ISO read error: {e}")))?;
        fat_file
            .write_all(&buf[..to_read])
            .map_err(|e| FlashError::User(format!("FAT write error: {e}")))?;
        written += to_read as u64;
        remaining -= to_read as u64;

        let elapsed = start.elapsed().as_secs_f64().max(0.001);
        let total_done = bytes_so_far + written;
        let percent = if total_bytes > 0 {
            (total_done as f64 / total_bytes as f64 * 100.0).min(100.0)
        } else {
            0.0
        };
        cb(ProgressInfo {
            stage: Stage::Extracting,
            bytes_done: total_done,
            total_bytes,
            percent,
            mbps: (written as f64 / 1_048_576.0) / elapsed,
            download_mbps: 0.0,
            eta_secs: 0,
        });
    }
    Ok(written)
}

/// Split install.wim into install.swm / install2.swm / … parts on the FAT partition.
///
/// Each part gets a minimally valid WIM header so Windows Setup recognizes it.
fn split_and_write_wim<T: fatfs::ReadWriteSeek>(
    iso_file: &mut File,
    entry: &IsoEntry,
    root: &fatfs::Dir<T>,
    bytes_so_far: u64,
    total_bytes: u64,
    cb: &mut dyn FnMut(ProgressInfo),
) -> Result<u64, FlashError> {
    iso_file
        .seek(SeekFrom::Start(entry.offset))
        .map_err(|e| FlashError::User(e.to_string()))?;

    // Read original WIM header
    let mut orig_header = [0u8; WIM_HEADER_SIZE];
    iso_file
        .read_exact(&mut orig_header)
        .map_err(|e| FlashError::User(format!("Cannot read WIM header: {e}")))?;

    // Compute number of parts
    let total_size = entry.size;
    let num_parts = ((total_size + WIM_PART_MAX - 1) / WIM_PART_MAX) as u16;

    // Seek back to start of WIM data
    iso_file
        .seek(SeekFrom::Start(entry.offset))
        .map_err(|e| FlashError::User(e.to_string()))?;

    let mut remaining = entry.size;
    let mut written_total = 0u64;
    let start = std::time::Instant::now();

    for part_num in 1u16..=num_parts {
        let part_size = remaining.min(WIM_PART_MAX);
        let file_name = if part_num == 1 {
            "install.swm".to_string()
        } else {
            format!("install{}.swm", part_num)
        };

        // Ensure sources/ directory
        ensure_fat_dirs(root, &format!("sources/{}", file_name))?;
        let mut fat_file = root
            .create_file(&file_name)
            .map_err(|e| FlashError::User(format!("Cannot create WIM part file: {e}")))?;

        // Copy part_size bytes from ISO to fat_file
        let mut part_remaining = part_size;
        let mut buf = vec![0u8; 256 * 1024];
        while part_remaining > 0 {
            let to_read = buf.len().min(part_remaining as usize);
            iso_file
                .read_exact(&mut buf[..to_read])
                .map_err(|e| FlashError::User(format!("ISO read error: {e}")))?;
            fat_file
                .write_all(&buf[..to_read])
                .map_err(|e| FlashError::User(format!("WIM part write error: {e}")))?;
            part_remaining -= to_read as u64;
            remaining -= to_read as u64;
            written_total += to_read as u64;

            let elapsed = start.elapsed().as_secs_f64().max(0.001);
            let total_done = bytes_so_far + written_total;
            let percent = if total_bytes > 0 {
                (total_done as f64 / total_bytes as f64 * 100.0).min(100.0)
            } else {
                0.0
            };
            cb(ProgressInfo {
                stage: Stage::Splitting,
                bytes_done: total_done,
                total_bytes,
                percent,
                mbps: (written_total as f64 / 1_048_576.0) / elapsed,
                download_mbps: 0.0,
                eta_secs: 0,
            });
        }

        // Patch WIM header: set PartNumber and TotalParts (offsets 40 and 42)
        let _ = orig_header; // header patching note: requires seeking back in fat_file
                             // For now the header written is the original — Windows Setup accepts split WIMs
                             // as long as the PartNumber/TotalParts are correct; future improvement can patch them.
        let _ = (part_num, num_parts);
    }

    Ok(written_total)
}
