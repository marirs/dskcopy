//! Linux ISO extracted-file mode with UEFI boot support.
//!
//! For Linux ISOs that include EFI/BOOT/BOOTX64.EFI:
//!   1. Format target as GPT + FAT32
//!   2. Extract all ISO contents to FAT32
//!   3. (EFI boot files land in the right place automatically)
//!   4. Optionally add a persistence partition (delegated to persistent.rs)
//!
//! Falls back to raw write if the ISO has no EFI boot directory.

use crate::image::{FlashError, FlashResult};
use crate::iso_reader::{iso_has_file, list_iso_entries};
use crate::partition::{format_drive, PartitionSlice};
use crate::{FlashOptions, ProgressInfo, Stage};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};

/// Returns true if the ISO supports UEFI extraction mode.
pub fn detect_linux_uefi_iso(iso_path: &str) -> bool {
    iso_has_file(iso_path, "EFI/BOOT/BOOTX64.EFI")
        || iso_has_file(iso_path, "efi/boot/bootx64.efi")
}

/// Flash a Linux ISO by extracting its contents to a FAT32 partition.
pub fn flash_linux_extracted(
    iso_path: &str,
    target: &str,
    opts: &FlashOptions,
    cb: &mut dyn FnMut(ProgressInfo),
) -> Result<FlashResult, FlashError> {
    // 1. Format GPT + FAT32
    format_drive(target, crate::PartitionScheme::Gpt, crate::FilesystemType::Fat32, Some(cb))
        .map_err(FlashError::Format)?;

    // 2. Collect ISO entries
    let entries = list_iso_entries(iso_path)
        .map_err(|e| FlashError::User(format!("Cannot read ISO: {e}")))?;

    let total_bytes: u64 = entries.iter().filter(|e| !e.is_dir).map(|e| e.size).sum();
    let mut bytes_done: u64 = 0;

    // 3. Open device and the FAT32 partition region
    let sector_size: u64 = 512;
    let start_lba: u64 = 2048;

    let mut dev = OpenOptions::new()
        .read(true)
        .write(true)
        .open(target)
        .map_err(|e| FlashError::User(format!("Cannot open device: {e}")))?;

    let disk_size = dev
        .metadata()
        .map_err(|e| FlashError::User(format!("Cannot stat device: {e}")))?
        .len();
    let end_lba = (disk_size / sector_size).saturating_sub(34);
    let part_offset = start_lba * sector_size;
    let part_size = (end_lba - start_lba) * sector_size;

    {
        let mut part_slice = PartitionSlice::new(&mut dev, part_offset, part_size);

        // Open FAT32 on the partition
        let fs = fatfs::FileSystem::new(&mut part_slice, fatfs::FsOptions::new())
            .map_err(|e| FlashError::User(format!("Cannot open FAT32 on device: {e}")))?;
        let root = fs.root_dir();

        let mut iso_file = File::open(iso_path)
            .map_err(|e| FlashError::User(format!("Cannot open ISO: {e}")))?;

        let start = std::time::Instant::now();

        for entry in &entries {
            if entry.is_dir {
                continue;
            }
            let rel = entry.path.trim_start_matches('/');
            bytes_done += extract_to_fat(
                &mut iso_file,
                entry.offset,
                entry.size,
                &root,
                rel,
                bytes_done,
                total_bytes,
                &start,
                cb,
            )?;
        }
    }

    // 4. Optionally add persistence partition
    if let Some(persist_mb) = opts.persistent_mb {
        crate::persistent::add_persistence_partition(target, persist_mb, cb)
            .map_err(|e| FlashError::User(format!("Persistence partition failed: {e}")))?;
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
        detected_type: "Linux ISO (extracted)".to_string(),
    })
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn ensure_dirs<T: fatfs::ReadWriteSeek>(root: &fatfs::Dir<T>, rel_path: &str) -> Result<(), FlashError> {
    let parts: Vec<&str> = rel_path.split('/').collect();
    if parts.len() <= 1 {
        return Ok(());
    }
    let mut cur = root.clone();
    for dir_name in &parts[..parts.len() - 1] {
        if dir_name.is_empty() {
            continue;
        }
        cur = cur
            .create_dir(dir_name)
            .map_err(|e| FlashError::User(format!("Cannot create dir '{}': {e}", dir_name)))?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn extract_to_fat<T: fatfs::ReadWriteSeek>(
    iso_file: &mut File,
    offset: u64,
    size: u64,
    root: &fatfs::Dir<T>,
    rel_path: &str,
    bytes_so_far: u64,
    total_bytes: u64,
    start: &std::time::Instant,
    cb: &mut dyn FnMut(ProgressInfo),
) -> Result<u64, FlashError> {
    ensure_dirs(root, rel_path)?;

    let file_name = rel_path.split('/').last().unwrap_or(rel_path);
    let mut fat_file = root
        .create_file(file_name)
        .map_err(|e| FlashError::User(format!("Cannot create FAT file '{}': {e}", file_name)))?;

    iso_file
        .seek(SeekFrom::Start(offset))
        .map_err(|e| FlashError::User(e.to_string()))?;

    let mut remaining = size;
    let mut written = 0u64;
    let mut buf = vec![0u8; 256 * 1024];

    while remaining > 0 {
        let to_read = buf.len().min(remaining as usize);
        iso_file
            .read_exact(&mut buf[..to_read])
            .map_err(|e| FlashError::User(format!("ISO read error: {e}")))?;
        fat_file
            .write_all(&buf[..to_read])
            .map_err(|e| FlashError::User(format!("FAT write error for '{}': {e}", rel_path)))?;
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


