use crate::iso_reader::iso_has_file;
use crate::source::{Compression, SourceDescriptor, SourceKind};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawImageType {
    // ── ISO variants ───────────────────────────────────────────────────────
    Iso9660,
    WindowsIso,
    LinuxUefiIso,
    // ── Apple ──────────────────────────────────────────────────────────────
    AppleDmg,
    MacosInstallerDmg,
    // ── VHD/VHDX/FFU ──────────────────────────────────────────────────────
    VhdFixed,
    VhdDynamic,
    Vhdx,
    Ffu,
    // ── Generic ────────────────────────────────────────────────────────────
    Raw,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageType {
    pub raw: RawImageType,
    pub compression: Compression,
}

impl ImageType {
    pub fn description(&self) -> String {
        let base = match &self.raw {
            RawImageType::Iso9660 => "ISO 9660",
            RawImageType::WindowsIso => "Windows Installer ISO",
            RawImageType::LinuxUefiIso => "Linux ISO (UEFI-bootable)",
            RawImageType::AppleDmg => "Apple DMG (UDIF)",
            RawImageType::MacosInstallerDmg => "macOS Installer DMG",
            RawImageType::VhdFixed => "VHD (Fixed)",
            RawImageType::VhdDynamic => "VHD (Dynamic)",
            RawImageType::Vhdx => "VHDX",
            RawImageType::Ffu => "FFU (Full Flash Update)",
            RawImageType::Raw => "Raw disk image",
            RawImageType::Unknown => "Unknown",
        };
        match self.compression {
            Compression::None => base.to_string(),
            Compression::Gz => format!("{} (gzip compressed)", base),
            Compression::Xz => format!("{} (xz compressed)", base),
            Compression::Zip => format!("{} (zip compressed)", base),
            Compression::Bzip2 => format!("{} (bzip2 compressed)", base),
        }
    }
}

/// Detect the image type from a SourceDescriptor.
/// For local files: uses magic bytes (inner type for compressed).
/// For URLs: uses extension heuristics only (no pre-download probe).
pub fn detect_image_type(source: &SourceDescriptor) -> ImageType {
    let compression = source.compression;

    let raw = match source.kind {
        SourceKind::LocalFile => detect_local_raw_type(&source.input, compression),
        SourceKind::HttpUrl => {
            let inner_name = strip_compression_ext(&source.input);
            raw_type_from_extension(inner_name)
        }
    };

    ImageType { raw, compression }
}

fn detect_local_raw_type(path: &str, compression: Compression) -> RawImageType {
    match compression {
        Compression::None => probe_raw_magic(path),
        Compression::Gz => probe_compressed_magic(path, compression)
            .unwrap_or_else(|| raw_type_from_extension(path)),
        _ => {
            let inner = strip_compression_ext(path);
            raw_type_from_extension(inner)
        }
    }
}

/// Read magic bytes directly from an uncompressed file.
fn probe_raw_magic(path: &str) -> RawImageType {
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(_) => return RawImageType::Unknown,
    };

    // ── VHDX: first 8 bytes = "vhdxfile" ─────────────────────────────────
    let mut magic16 = [0u8; 16];
    if f.read_exact(&mut magic16).is_ok() {
        if &magic16[..8] == b"vhdxfile" {
            return RawImageType::Vhdx;
        }
        // FFU: offset 4 contains "SignedImage " (12 bytes)
        if magic16.len() >= 16 && &magic16[4..16] == b"SignedImage " {
            return RawImageType::Ffu;
        }
    }

    // ── VHD: "conectix" cookie in last 512 bytes ──────────────────────────
    if let Ok(meta) = f.metadata() {
        let file_len = meta.len();
        if file_len >= 512 {
            if f.seek(SeekFrom::Start(file_len - 512)).is_ok() {
                let mut footer = [0u8; 8];
                if f.read_exact(&mut footer).is_ok() {
                    if &footer == b"conectix" {
                        // Determine fixed vs dynamic from disk_type field at offset +60 in footer
                        let _ = f.seek(SeekFrom::Start(file_len - 512 + 60));
                        let mut dt = [0u8; 4];
                        let disk_type = if f.read_exact(&mut dt).is_ok() {
                            u32::from_be_bytes(dt)
                        } else {
                            2
                        };
                        return if disk_type == 2 {
                            RawImageType::VhdFixed
                        } else {
                            RawImageType::VhdDynamic
                        };
                    }
                }
            }
        }
    }

    // ── DMG koly trailer ─────────────────────────────────────────────────
    if let Ok(meta) = f.metadata() {
        let file_len = meta.len();
        if file_len >= 512 {
            if f.seek(SeekFrom::Start(file_len - 512)).is_ok() {
                let mut trailer = [0u8; 4];
                if f.read_exact(&mut trailer).is_ok() && &trailer == b"koly" {
                    return RawImageType::AppleDmg;
                }
            }
        }
    }

    // ── ISO 9660 ─────────────────────────────────────────────────────────
    if f.seek(SeekFrom::Start(32769)).is_ok() {
        let mut sig = [0u8; 5];
        if f.read_exact(&mut sig).is_ok() && &sig == b"CD001" {
            // Classify ISO further
            return classify_iso(path);
        }
    }

    RawImageType::Raw
}

/// Classify an ISO as Windows, Linux UEFI, or generic.
fn classify_iso(path: &str) -> RawImageType {
    if iso_has_file(path, "sources/install.wim") || iso_has_file(path, "sources/install.esd") {
        return RawImageType::WindowsIso;
    }
    if iso_has_file(path, "EFI/BOOT/BOOTX64.EFI") {
        return RawImageType::LinuxUefiIso;
    }
    RawImageType::Iso9660
}

/// Decompress a small prefix to detect the inner format.
fn probe_compressed_magic(path: &str, compression: Compression) -> Option<RawImageType> {
    use bzip2::read::BzDecoder;
    use flate2::read::GzDecoder;

    let file = File::open(path).ok()?;
    const PROBE_LEN: usize = 40_000;
    let mut buf = vec![0u8; PROBE_LEN];

    let n = match compression {
        Compression::Gz => {
            let mut dec = GzDecoder::new(file);
            read_best_effort(&mut dec, &mut buf)
        }
        Compression::Bzip2 => {
            let mut dec = BzDecoder::new(file);
            read_best_effort(&mut dec, &mut buf)
        }
        _ => return None,
    };

    if n < 4 {
        return None;
    }

    if n >= 32774 && &buf[32769..32774] == b"CD001" {
        // Compressed ISO — can't probe inside without full extract; return generic
        return Some(RawImageType::Iso9660);
    }
    None
}

fn read_best_effort<R: Read>(reader: &mut R, buf: &mut [u8]) -> usize {
    let mut total = 0;
    while total < buf.len() {
        match reader.read(&mut buf[total..]) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(_) => break,
        }
    }
    total
}

/// Strip one level of compression extension to get the inner filename.
fn strip_compression_ext(name: &str) -> &str {
    for ext in &[".gz", ".xz", ".bz2", ".bzip2", ".tgz"] {
        if name.to_lowercase().ends_with(ext) {
            return &name[..name.len() - ext.len()];
        }
    }
    name
}

fn raw_type_from_extension(name: &str) -> RawImageType {
    let lower = name.to_lowercase();
    if lower.ends_with(".iso") {
        // Can't inspect contents from URL; return generic ISO
        RawImageType::Iso9660
    } else if lower.ends_with(".dmg") {
        RawImageType::AppleDmg
    } else if lower.ends_with(".vhd") {
        RawImageType::VhdFixed
    } else if lower.ends_with(".vhdx") {
        RawImageType::Vhdx
    } else if lower.ends_with(".ffu") {
        RawImageType::Ffu
    } else if lower.ends_with(".img") || lower.ends_with(".raw") || lower.ends_with(".bin") {
        RawImageType::Raw
    } else {
        RawImageType::Unknown
    }
}
