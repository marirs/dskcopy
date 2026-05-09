//! Legacy BIOS boot code installer.
//!
//! Writes 440-byte MBR boot code to a device, preserving the existing
//! partition table (bytes 446-511). Supports Syslinux and GRUB boot code
//! from embedded assets.

use crate::image::FlashError;
use crate::BootAssets;
use std::io::{Read, Seek, SeekFrom, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MbrBootType {
    Syslinux,
    Grub,
    Generic,
}

#[derive(Debug, Clone)]
pub enum BootError {
    Io(String),
    Unsupported(String),
    AssetMissing(String),
}

impl std::fmt::Display for BootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BootError::Io(s) => write!(f, "Boot install I/O error: {s}"),
            BootError::Unsupported(s) => write!(f, "Unsupported boot mode: {s}"),
            BootError::AssetMissing(s) => write!(f, "Boot asset missing: {s}"),
        }
    }
}

impl From<BootError> for FlashError {
    fn from(e: BootError) -> Self {
        FlashError::User(e.to_string())
    }
}

/// Install MBR boot code onto a device.
///
/// Only the first 440 bytes (boot code area) are written.
/// The partition table (bytes 446-509) and the MBR signature (510-511)
/// are preserved from the existing MBR.
pub fn install_mbr(device: &str, boot_type: MbrBootType) -> Result<(), BootError> {
    let asset_name = match boot_type {
        MbrBootType::Syslinux => "syslinux_mbr.bin",
        MbrBootType::Grub => "grub_mbr.bin",
        MbrBootType::Generic => "syslinux_mbr.bin",
    };

    let asset = BootAssets::get(asset_name)
        .ok_or_else(|| BootError::AssetMissing(asset_name.to_string()))?;

    let boot_code = asset.data.as_ref();
    if boot_code.len() < 440 {
        return Err(BootError::AssetMissing(format!(
            "{asset_name} is too small ({} bytes, need 440)",
            boot_code.len()
        )));
    }

    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(device)
        .map_err(|e| BootError::Io(format!("Cannot open device '{device}': {e}")))?;

    // Read existing MBR to preserve partition table + signature
    let mut mbr = [0u8; 512];
    f.read_exact(&mut mbr)
        .map_err(|e| BootError::Io(format!("Cannot read MBR: {e}")))?;

    // Overwrite boot code (first 440 bytes) only
    mbr[..440].copy_from_slice(&boot_code[..440]);

    // Make sure signature is set
    mbr[510] = 0x55;
    mbr[511] = 0xAA;

    f.seek(SeekFrom::Start(0))
        .map_err(|e| BootError::Io(e.to_string()))?;
    f.write_all(&mbr)
        .map_err(|e| BootError::Io(format!("Cannot write MBR: {e}")))?;
    f.sync_all()
        .map_err(|e| BootError::Io(format!("Cannot sync MBR: {e}")))?;

    Ok(())
}

/// Install a hybrid MBR that works on both UEFI and BIOS systems.
///
/// On a GPT disk: writes GRUB/Syslinux boot code into the protective MBR
/// without disturbing the GPT structures.
pub fn install_hybrid_mbr(device: &str) -> Result<(), BootError> {
    install_mbr(device, MbrBootType::Grub)
}
