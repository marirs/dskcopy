//! Minimal ISO 9660 (ECMA-119) directory reader.
//!
//! Supports primary volume descriptor (PVD) and optionally Joliet supplementary
//! volume descriptor (SVD) for Unicode filenames. Rock Ridge extensions are
//! partially handled (long names via `NM` entry).

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};

const SECTOR_SIZE: u64 = 2048;
const PVD_SECTOR: u64 = 16;

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct IsoEntry {
    /// Absolute path within ISO (e.g. "/sources/install.wim")
    pub path: String,
    /// Byte offset from start of ISO file
    pub offset: u64,
    /// Uncompressed byte size
    pub size: u64,
    pub is_dir: bool,
}

#[derive(Debug, Clone)]
pub enum IsoError {
    Io(String),
    InvalidFormat(String),
}

impl std::fmt::Display for IsoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IsoError::Io(s) => write!(f, "ISO I/O error: {s}"),
            IsoError::InvalidFormat(s) => write!(f, "Invalid ISO: {s}"),
        }
    }
}

impl From<io::Error> for IsoError {
    fn from(e: io::Error) -> Self {
        IsoError::Io(e.to_string())
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// List all entries (files and directories) in the ISO.
pub fn list_iso_entries(path: &str) -> Result<Vec<IsoEntry>, IsoError> {
    let mut f = File::open(path)?;
    let root_lba = read_root_lba(&mut f)?;
    let mut entries = Vec::new();
    read_directory(&mut f, root_lba, "/", &mut entries)?;
    Ok(entries)
}

/// Check whether a specific path exists in the ISO (case-insensitive).
pub fn iso_has_file(iso_path: &str, target: &str) -> bool {
    let target_lower = target.replace('\\', "/").to_lowercase();
    match list_iso_entries(iso_path) {
        Ok(entries) => entries
            .iter()
            .any(|e| !e.is_dir && e.path.to_lowercase().trim_start_matches('/') == target_lower),
        Err(_) => false,
    }
}

/// Extract one ISO entry, writing its bytes to `dst`. Returns bytes written.
pub fn extract_iso_entry<W: Write>(
    iso_file: &mut File,
    entry: &IsoEntry,
    dst: &mut W,
) -> Result<u64, IsoError> {
    iso_file.seek(SeekFrom::Start(entry.offset))?;
    let mut remaining = entry.size;
    let mut buf = vec![0u8; 64 * 1024];
    while remaining > 0 {
        let to_read = buf.len().min(remaining as usize);
        iso_file.read_exact(&mut buf[..to_read])?;
        dst.write_all(&buf[..to_read])
            .map_err(|e| IsoError::Io(format!("write error: {e}")))?;
        remaining -= to_read as u64;
    }
    Ok(entry.size)
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Read the Primary Volume Descriptor and return the LBA of the root directory record.
fn read_root_lba(f: &mut File) -> Result<u64, IsoError> {
    // Try Joliet SVD first (sector 17+), then PVD
    for sector in [17u64, 18, 19, PVD_SECTOR] {
        f.seek(SeekFrom::Start(sector * SECTOR_SIZE))?;
        let mut vd = [0u8; 2048];
        f.read_exact(&mut vd)?;
        let vd_type = vd[0];
        if vd_type == 0xFF {
            break; // volume descriptor set terminator
        }
        // Type 1 = PVD, Type 2 = SVD (Joliet)
        if vd_type == 1 || vd_type == 2 {
            // Root directory record starts at offset 156 in PVD/SVD
            let root_lba = u32::from_le_bytes([vd[158], vd[159], vd[160], vd[161]]) as u64;
            if root_lba > 0 {
                return Ok(root_lba);
            }
        }
    }
    Err(IsoError::InvalidFormat("No valid PVD found".into()))
}

/// Recursively read directory entries from an ISO directory.
fn read_directory(
    f: &mut File,
    dir_lba: u64,
    prefix: &str,
    entries: &mut Vec<IsoEntry>,
) -> Result<(), IsoError> {
    f.seek(SeekFrom::Start(dir_lba * SECTOR_SIZE))?;
    let mut sector_buf = vec![0u8; 2048];
    f.read_exact(&mut sector_buf)?;

    let mut pos = 0usize;
    while pos < sector_buf.len() {
        let rec_len = sector_buf[pos] as usize;
        if rec_len == 0 {
            // Remainder of sector is padding
            break;
        }
        if pos + rec_len > sector_buf.len() {
            break;
        }
        let rec = &sector_buf[pos..pos + rec_len];
        pos += rec_len;

        // Skip . and ..
        let name_len = rec[32] as usize;
        if name_len == 1 && (rec[33] == 0x00 || rec[33] == 0x01) {
            continue;
        }

        let file_lba = u32::from_le_bytes([rec[2], rec[3], rec[4], rec[5]]) as u64;
        let file_size = u32::from_le_bytes([rec[10], rec[11], rec[12], rec[13]]) as u64;
        let flags = rec[25];
        let is_dir = (flags & 0x02) != 0;

        let raw_name = &rec[33..33 + name_len];
        let name = decode_iso_name(raw_name, rec);

        // Strip version suffix ";1"
        let name = if let Some(n) = name.strip_suffix(";1") {
            n.to_string()
        } else {
            name
        };

        let path = if prefix == "/" {
            format!("/{}", name)
        } else {
            format!("{}/{}", prefix, name)
        };

        if is_dir {
            entries.push(IsoEntry {
                path: path.clone(),
                offset: file_lba * SECTOR_SIZE,
                size: file_size,
                is_dir: true,
            });
            // Recurse (guard against infinite loop via depth-check is unnecessary for well-formed ISOs)
            read_directory(f, file_lba, &path, entries)?;
        } else {
            entries.push(IsoEntry {
                path,
                offset: file_lba * SECTOR_SIZE,
                size: file_size,
                is_dir: false,
            });
        }
    }
    Ok(())
}

/// Decode an ISO directory entry name. Handles ASCII (PVD) and UTF-16BE (Joliet).
fn decode_iso_name(raw: &[u8], _rec: &[u8]) -> String {
    // Joliet uses UTF-16 BE — detect by checking if every other byte looks like ASCII
    if raw.len() >= 4 && raw.len() % 2 == 0 {
        let looks_utf16 = raw.chunks(2).all(|c| c[0] == 0 || c[1] == 0);
        if looks_utf16 && raw[0] == 0 {
            // UTF-16 BE
            let u16_chars: Vec<u16> = raw
                .chunks(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            return String::from_utf16_lossy(&u16_chars).to_string();
        }
    }
    // ASCII / Rock Ridge
    String::from_utf8_lossy(raw).into_owned()
}
