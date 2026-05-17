use crate::detect::{detect_image_type, RawImageType};
use crate::partition::{format_drive, FormatError};
use crate::source::{SourceDescriptor, SourceKind};
use crate::verify::{verify_image, VerifyError};
use crate::{FlashMode, FlashOptions, ProgressInfo, Stage, VerifyMode};
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::process::Command;
use std::time::Instant;

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ImageInfo {
    pub path: String,
    pub size_bytes: u64,
}

pub fn parse_image(path: &str) -> ImageInfo {
    let size_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    ImageInfo {
        path: path.to_string(),
        size_bytes,
    }
}

#[derive(Debug)]
pub enum FlashError {
    User(String),
    Verify(VerifyError),
    Format(FormatError),
}

impl std::fmt::Display for FlashError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FlashError::User(msg) => write!(f, "{msg}"),
            FlashError::Verify(err) => write!(f, "{err}"),
            FlashError::Format(err) => write!(f, "Format error: {err}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FlashResult {
    pub bytes_written: u64,
    /// SHA-256 hex of the uncompressed bytes that were written
    pub sha256: String,
    /// Human-readable detected image type
    pub detected_type: String,
}

// ── Device path helpers ───────────────────────────────────────────────────────

pub fn normalize_target_device(target: &str) -> String {
    if cfg!(target_os = "macos") {
        if let Some(suffix) = target.strip_prefix("/dev/disk") {
            return format!("/dev/rdisk{suffix}");
        }
    }
    target.to_string()
}

// ── Entry points ──────────────────────────────────────────────────────────────

/// High-level flash entry point.  Accepts a URL or local path as `source`.
pub fn flash(
    source_input: &str,
    target: &str,
    opts: FlashOptions,
    progress: Option<&mut dyn FnMut(ProgressInfo)>,
) -> Result<FlashResult, FlashError> {
    let source = SourceDescriptor::detect(source_input)?;
    let image_type = detect_image_type(&source);

    let mut noop = |_: ProgressInfo| {};
    let cb: &mut dyn FnMut(ProgressInfo) = match progress {
        Some(cb) => cb,
        None => &mut noop,
    };

    // Optional: format-before-write
    if opts.format_before_write && opts.flash_mode == FlashMode::RawWrite {
        cb(ProgressInfo {
            stage: Stage::Formatting,
            bytes_done: 0,
            total_bytes: 0,
            percent: 0.0,
            mbps: 0.0,
            download_mbps: 0.0,
            eta_secs: 0,
        });
        format_drive(target, opts.format_scheme, opts.format_fs, None)
            .map_err(FlashError::Format)?;
    }

    let raw_target = normalize_target_device(target);

    // Unmount on macOS
    if cfg!(target_os = "macos") {
        cb(ProgressInfo {
            stage: Stage::Unmounting,
            bytes_done: 0,
            total_bytes: 0,
            percent: 0.0,
            mbps: 0.0,
            download_mbps: 0.0,
            eta_secs: 0,
        });
        unmount_disk(&raw_target)?;
    }

    // Determine effective flash mode
    let effective_mode = resolve_flash_mode(&image_type.raw, &opts, &source);

    // Route by effective mode / image type
    let result = match effective_mode {
        FlashMode::WindowsDualPartition => {
            crate::windows_iso::flash_windows_iso(&source.input, &raw_target, &opts, cb)?
        }
        FlashMode::WindowsNtfs => {
            crate::windows_iso::flash_windows_ntfs(&source.input, &raw_target, &opts, cb)?
        }
        FlashMode::ExtractedFile => {
            crate::linux_iso::flash_linux_extracted(&source.input, &raw_target, &opts, cb)?
        }
        FlashMode::MacosCreateInstallMedia => {
            #[cfg(target_os = "macos")]
            {
                crate::macos_installer::flash_macos_installer(&source.input, &raw_target, cb)?
            }
            #[cfg(not(target_os = "macos"))]
            {
                return Err(FlashError::User(
                    "createinstallmedia is only supported on macOS.".to_string(),
                ));
            }
        }
        FlashMode::RawWrite => match image_type.raw {
            RawImageType::AppleDmg | RawImageType::MacosInstallerDmg
                if source.kind == SourceKind::LocalFile =>
            {
                crate::dmg::restore_dmg(&source, &raw_target, &opts, cb)?
            }
            RawImageType::VhdFixed | RawImageType::VhdDynamic => {
                let reader = crate::vhd::open_vhd_reader(&source.input)
                    .map_err(|e| FlashError::User(format!("VHD error: {e}")))?;
                write_stream_reader(reader, None, &raw_target, &opts, cb)?
            }
            RawImageType::Vhdx => {
                let reader = crate::vhd::open_vhdx_reader(&source.input)
                    .map_err(|e| FlashError::User(format!("VHDX error: {e}")))?;
                write_stream_reader(reader, None, &raw_target, &opts, cb)?
            }
            RawImageType::Ffu => {
                let reader = crate::vhd::open_ffu_reader(&source.input)
                    .map_err(|e| FlashError::User(format!("FFU error: {e}")))?;
                write_stream_reader(reader, None, &raw_target, &opts, cb)?
            }
            _ => write_stream(&source, &raw_target, &opts, cb)?,
        },
    };

    // Verification (only for raw write paths that have a known source)
    if opts.verify && effective_mode == FlashMode::RawWrite {
        let verify_mode = if source.kind == SourceKind::HttpUrl {
            VerifyMode::Sha256
        } else {
            opts.verify_mode
        };

        match verify_mode {
            VerifyMode::ByteByByte => {
                let src_path = &source.input;
                verify_image(
                    src_path,
                    &raw_target,
                    opts.block_size,
                    VerifyMode::ByteByByte,
                    cb,
                )
                .map_err(FlashError::Verify)?;
            }
            VerifyMode::Sha256 => {
                let device_hash =
                    crate::hash::compute_hash_prefix(&raw_target, result.bytes_written).map_err(
                        |e| {
                            FlashError::Verify(VerifyError::Io(format!(
                                "Could not hash target device: {e}"
                            )))
                        },
                    )?;
                if device_hash != result.sha256 {
                    return Err(FlashError::Verify(VerifyError::HashMismatch {
                        source: result.sha256.clone(),
                        target: device_hash,
                    }));
                }
                cb(ProgressInfo {
                    stage: Stage::Verifying,
                    bytes_done: result.bytes_written,
                    total_bytes: result.bytes_written,
                    percent: 100.0,
                    mbps: 0.0,
                    download_mbps: 0.0,
                    eta_secs: 0,
                });
            }
        }
    }

    cb(ProgressInfo {
        stage: Stage::Done,
        bytes_done: result.bytes_written,
        total_bytes: result.bytes_written,
        percent: 100.0,
        mbps: 0.0,
        download_mbps: 0.0,
        eta_secs: 0,
    });

    Ok(FlashResult {
        detected_type: image_type.description(),
        ..result
    })
}

/// Resolve the actual flash mode to use, auto-detecting when mode is RawWrite.
fn resolve_flash_mode(
    raw_type: &RawImageType,
    opts: &FlashOptions,
    source: &SourceDescriptor,
) -> FlashMode {
    // Explicit override wins
    if opts.flash_mode != FlashMode::RawWrite {
        return opts.flash_mode;
    }
    // macOS installer flag
    if opts.macos_installer {
        return FlashMode::MacosCreateInstallMedia;
    }
    // Auto-detect for local files only
    if source.kind != SourceKind::LocalFile {
        return FlashMode::RawWrite;
    }
    match raw_type {
        RawImageType::WindowsIso => FlashMode::WindowsDualPartition,
        RawImageType::LinuxUefiIso => FlashMode::ExtractedFile,
        _ => FlashMode::RawWrite,
    }
}

/// Backward-compatible wrapper for callers that pass a plain file path.
pub fn flash_image(
    source: &str,
    target: &str,
    opts: FlashOptions,
    progress: Option<&mut dyn FnMut(ProgressInfo)>,
) -> Result<(), FlashError> {
    flash(source, target, opts, progress).map(|_| ())
}

// ── Streaming write ───────────────────────────────────────────────────────────

pub(crate) fn write_stream(
    source: &SourceDescriptor,
    raw_target: &str,
    opts: &FlashOptions,
    cb: &mut dyn FnMut(ProgressInfo),
) -> Result<FlashResult, FlashError> {
    let (reader, total_known) = source.open_reader()?;
    write_stream_reader(reader, total_known, raw_target, opts, cb)
}

pub(crate) fn write_stream_reader(
    mut reader: Box<dyn Read + Send + 'static>,
    total_known: Option<u64>,
    raw_target: &str,
    opts: &FlashOptions,
    cb: &mut dyn FnMut(ProgressInfo),
) -> Result<FlashResult, FlashError> {
    let total = total_known.unwrap_or(0);

    let mut dst = OpenOptions::new()
        .write(true)
        .open(raw_target)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                FlashError::User(
                    "Permission denied opening target device. Re-run with sudo.".to_string(),
                )
            } else {
                FlashError::User(format!(
                    "Could not open target device '{}': {}. Ensure it is not in use.",
                    raw_target, e
                ))
            }
        })?;

    let block_size = opts.block_size.max(4096);
    let mut buf = vec![0_u8; block_size];
    let mut written = 0_u64;
    let mut hasher = Sha256::new();
    let start = Instant::now();

    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| FlashError::User(format!("Failed reading source during write: {e}")))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        dst.write_all(&buf[..n]).map_err(|e| {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                FlashError::User(
                    "Permission denied writing target device. Re-run with sudo.".to_string(),
                )
            } else {
                FlashError::User(format!("Write failed: {e}"))
            }
        })?;
        written += n as u64;

        let elapsed = start.elapsed().as_secs_f64().max(0.001);
        let percent = if total > 0 {
            (written as f64 / total as f64 * 100.0).min(100.0)
        } else {
            0.0
        };
        let mbps = (written as f64 / (1024.0 * 1024.0)) / elapsed;
        let eta_secs = if total > 0 && mbps > 0.0 {
            let remain = total.saturating_sub(written);
            ((remain as f64 / (1024.0 * 1024.0)) / mbps) as u64
        } else {
            0
        };
        cb(ProgressInfo {
            stage: Stage::Writing,
            bytes_done: written,
            total_bytes: total,
            percent,
            mbps,
            download_mbps: 0.0,
            eta_secs,
        });
    }

    dst.sync_all()
        .map_err(|e| FlashError::User(format!("Failed to flush writes to target device: {e}")))?;

    let sha256 = format!("{:x}", hasher.finalize());
    Ok(FlashResult {
        bytes_written: written,
        sha256,
        detected_type: String::new(),
    })
}

// ── macOS unmount ─────────────────────────────────────────────────────────────

pub fn unmount_disk(target: &str) -> Result<(), FlashError> {
    let disk = if let Some(suffix) = target.strip_prefix("/dev/rdisk") {
        format!("/dev/disk{suffix}")
    } else {
        target.to_string()
    };
    let status = Command::new("diskutil")
        .args(["unmountDisk", &disk])
        .status()
        .map_err(|_| {
            FlashError::User("Could not run diskutil to unmount target disk.".to_string())
        })?;
    if !status.success() {
        return Err(FlashError::User(
            "Could not unmount target disk. Close apps using it and retry.".to_string(),
        ));
    }
    Ok(())
}
