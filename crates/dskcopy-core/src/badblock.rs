//! Bad block and fake-flash detection.
//!
//! Writes deterministic test patterns to the full drive surface, reads them
//! back, and compares. Reports mismatching sectors and detects capacity
//! misreporting (fake flash drives).

use crate::{DriveCheckResult, ProgressInfo, Stage};
use std::io::{Read, Seek, SeekFrom, Write};
use std::time::Instant;

const SECTOR_SIZE: u64 = 4096; // 4 KiB test blocks
const SEED: u64 = 0xDEAD_BEEF_C0FFEE42;

#[derive(Debug, Clone)]
pub enum CheckError {
    Io(String),
    PermissionDenied,
}

impl std::fmt::Display for CheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CheckError::Io(s) => write!(f, "Check I/O error: {s}"),
            CheckError::PermissionDenied => {
                write!(f, "Permission denied. Re-run with sudo.")
            }
        }
    }
}

/// Run a drive integrity check.
///
/// If `quick = true`, only perform a read scan for I/O errors (no write).
/// If `quick = false`, write test patterns, then read back and compare.
pub fn check_drive(
    device: &str,
    quick: bool,
    cb: &mut dyn FnMut(ProgressInfo),
) -> Result<DriveCheckResult, CheckError> {
    if quick {
        check_read_only(device, cb)
    } else {
        check_read_write(device, cb)
    }
}

// ── Read-only quick scan ──────────────────────────────────────────────────────

fn check_read_only(
    device: &str,
    cb: &mut dyn FnMut(ProgressInfo),
) -> Result<DriveCheckResult, CheckError> {
    let mut f = open_device_read(device)?;
    let total = device_size(&mut f)?;

    let mut buf = vec![0u8; SECTOR_SIZE as usize];
    let mut pos: u64 = 0;
    let mut bad_sectors = Vec::new();
    let start = Instant::now();

    while pos < total {
        f.seek(SeekFrom::Start(pos))
            .map_err(|e| CheckError::Io(e.to_string()))?;
        match f.read_exact(&mut buf) {
            Ok(_) => {}
            Err(_) => {
                bad_sectors.push(pos / SECTOR_SIZE);
            }
        }
        pos += SECTOR_SIZE;

        let elapsed = start.elapsed().as_secs_f64().max(0.001);
        let mbps = (pos as f64 / 1_048_576.0) / elapsed;
        let percent = (pos as f64 / total as f64 * 100.0).min(100.0);
        cb(ProgressInfo {
            stage: Stage::Checking,
            bytes_done: pos,
            total_bytes: total,
            percent,
            mbps,
            download_mbps: 0.0,
            eta_secs: if mbps > 0.0 {
                ((total - pos) as f64 / 1_048_576.0 / mbps) as u64
            } else {
                0
            },
        });
    }

    Ok(DriveCheckResult {
        advertised_bytes: total,
        verified_bytes: total.saturating_sub(bad_sectors.len() as u64 * SECTOR_SIZE),
        bad_sectors,
        is_fake: false, // can't detect without write
    })
}

// ── Read-write full check ─────────────────────────────────────────────────────

fn check_read_write(
    device: &str,
    cb: &mut dyn FnMut(ProgressInfo),
) -> Result<DriveCheckResult, CheckError> {
    let total = {
        let mut f = open_device_read(device)?;
        device_size(&mut f)?
    };

    let num_sectors = total / SECTOR_SIZE;
    let mut f = open_device_rw(device)?;
    let start = Instant::now();

    // ── Phase 1: Write ────────────────────────────────────────────────────
    for sector_idx in 0..num_sectors {
        let pos = sector_idx * SECTOR_SIZE;
        let pattern = make_pattern(sector_idx);

        f.seek(SeekFrom::Start(pos))
            .map_err(|e| CheckError::Io(e.to_string()))?;
        f.write_all(&pattern)
            .map_err(|e| CheckError::Io(format!("Write error at sector {}: {}", sector_idx, e)))?;

        let elapsed = start.elapsed().as_secs_f64().max(0.001);
        let bytes_done = pos + SECTOR_SIZE;
        let percent = (bytes_done as f64 / (total * 2) as f64 * 100.0).min(50.0);
        cb(ProgressInfo {
            stage: Stage::Checking,
            bytes_done,
            total_bytes: total * 2, // write + read = 2x total
            percent,
            mbps: (bytes_done as f64 / 1_048_576.0) / elapsed,
            download_mbps: 0.0,
            eta_secs: 0,
        });
    }
    f.sync_all()
        .map_err(|e| CheckError::Io(format!("Sync error: {e}")))?;

    // ── Phase 2: Read and verify ──────────────────────────────────────────
    let mut bad_sectors: Vec<u64> = Vec::new();
    let mut fake_detected = false;
    let mut verified_bytes: u64 = 0;
    let mut buf = vec![0u8; SECTOR_SIZE as usize];

    for sector_idx in 0..num_sectors {
        let pos = sector_idx * SECTOR_SIZE;
        f.seek(SeekFrom::Start(pos))
            .map_err(|e| CheckError::Io(e.to_string()))?;

        match f.read_exact(&mut buf) {
            Err(_) => {
                bad_sectors.push(sector_idx);
                continue;
            }
            Ok(_) => {}
        }

        let expected = make_pattern(sector_idx);
        if buf != expected {
            bad_sectors.push(sector_idx);
            // Check for wrap-around (fake flash pattern: reads a previously
            // written sector instead of the current one)
            if !fake_detected && sector_idx > 0 {
                let early_pattern = make_pattern(sector_idx % (sector_idx / 2).max(1));
                if buf == early_pattern {
                    fake_detected = true;
                }
            }
        } else {
            verified_bytes += SECTOR_SIZE;
        }

        let elapsed = start.elapsed().as_secs_f64().max(0.001);
        let bytes_done = total + pos + SECTOR_SIZE;
        let percent = (bytes_done as f64 / (total * 2) as f64 * 100.0).min(100.0);
        cb(ProgressInfo {
            stage: Stage::Checking,
            bytes_done,
            total_bytes: total * 2,
            percent,
            mbps: (bytes_done as f64 / 1_048_576.0) / elapsed,
            download_mbps: 0.0,
            eta_secs: 0,
        });
    }

    Ok(DriveCheckResult {
        advertised_bytes: total,
        verified_bytes,
        bad_sectors,
        is_fake: fake_detected,
    })
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Generate a 4 KiB deterministic test pattern for sector `idx`.
fn make_pattern(idx: u64) -> Vec<u8> {
    let mut buf = vec![0u8; SECTOR_SIZE as usize];
    // Simple LCG-based pattern: each 8-byte word = idx XOR seed XOR position
    let mut val = idx.wrapping_mul(0x6c62272e07bb0142).wrapping_add(SEED);
    for chunk in buf.chunks_mut(8) {
        let bytes = val.to_le_bytes();
        let len = chunk.len();
        chunk.copy_from_slice(&bytes[..len]);
        val = val
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
    }
    buf
}

fn open_device_read(device: &str) -> Result<std::fs::File, CheckError> {
    std::fs::OpenOptions::new()
        .read(true)
        .open(device)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                CheckError::PermissionDenied
            } else {
                CheckError::Io(e.to_string())
            }
        })
}

fn open_device_rw(device: &str) -> Result<std::fs::File, CheckError> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(device)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                CheckError::PermissionDenied
            } else {
                CheckError::Io(e.to_string())
            }
        })
}

fn device_size(f: &mut std::fs::File) -> Result<u64, CheckError> {
    let meta = f.metadata().map_err(|e| CheckError::Io(e.to_string()))?;
    let len = meta.len();
    if len > 0 {
        return Ok(len);
    }
    // For block devices metadata.len() may return 0; use seek-to-end
    let pos = f
        .seek(SeekFrom::End(0))
        .map_err(|e| CheckError::Io(e.to_string()))?;
    f.seek(SeekFrom::Start(0))
        .map_err(|e| CheckError::Io(e.to_string()))?;
    Ok(pos)
}
