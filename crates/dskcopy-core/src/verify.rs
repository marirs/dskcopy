use crate::hash::{compute_hash, compute_hash_prefix};
use crate::{ProgressInfo, Stage, VerifyMode};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::time::Instant;

#[derive(Debug, Clone)]
pub enum VerifyError {
    Io(String),
    Mismatch { offset: u64, expected: u8, actual: u8 },
    HashMismatch { source: String, target: String },
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyError::Io(msg) => write!(f, "{msg}"),
            VerifyError::Mismatch { offset, expected, actual } => write!(
                f,
                "Verification failed at byte offset {offset}: expected 0x{expected:02x}, got 0x{actual:02x}."
            ),
            VerifyError::HashMismatch { .. } => {
                write!(f, "Verification failed: source and target hashes differ.")
            }
        }
    }
}

pub fn verify_image(
    image_path: &str,
    device: &str,
    block_size: usize,
    mode: VerifyMode,
    progress: &mut dyn FnMut(ProgressInfo),
) -> Result<(), VerifyError> {
    match mode {
        VerifyMode::ByteByByte => verify_byte_by_byte(image_path, device, block_size, progress),
        VerifyMode::Sha256 => verify_hash(image_path, device, progress),
    }
}

fn verify_byte_by_byte(
    image_path: &str,
    device: &str,
    block_size: usize,
    progress: &mut dyn FnMut(ProgressInfo),
) -> Result<(), VerifyError> {
    let mut src = File::open(image_path)
        .map_err(|_| VerifyError::Io("Could not open source image for verification.".to_string()))?;
    let mut dst = File::open(device).map_err(|_| {
        VerifyError::Io(
            "Could not open target device for verification. Try running with elevated privileges."
                .to_string(),
        )
    })?;
    let total = src
        .metadata()
        .map_err(|_| VerifyError::Io("Could not read source image size.".to_string()))?
        .len();
    dst.seek(SeekFrom::Start(0))
        .map_err(|_| VerifyError::Io("Could not seek target device.".to_string()))?;
    let mut src_buf = vec![0_u8; block_size.max(4096)];
    let mut dst_buf = vec![0_u8; block_size.max(4096)];
    let mut done = 0_u64;
    let start = Instant::now();

    loop {
        let n = src
            .read(&mut src_buf)
            .map_err(|_| VerifyError::Io("Could not read source image during verification.".to_string()))?;
        if n == 0 {
            break;
        }
        dst.read_exact(&mut dst_buf[..n]).map_err(|_| {
            VerifyError::Io("Could not read target device during verification.".to_string())
        })?;
        if src_buf[..n] != dst_buf[..n] {
            for i in 0..n {
                if src_buf[i] != dst_buf[i] {
                    return Err(VerifyError::Mismatch {
                        offset: done + i as u64,
                        expected: src_buf[i],
                        actual: dst_buf[i],
                    });
                }
            }
        }
        done += n as u64;
        let elapsed = start.elapsed().as_secs_f64().max(0.001);
        let percent = if total == 0 {
            100.0
        } else {
            (done as f64 / total as f64) * 100.0
        };
        let mbps = (done as f64 / (1024.0 * 1024.0)) / elapsed;
        let remain = total.saturating_sub(done);
        let eta_secs = if mbps <= 0.0 {
            0
        } else {
            ((remain as f64 / (1024.0 * 1024.0)) / mbps) as u64
        };
        progress(ProgressInfo {
            stage: Stage::Verifying,
            bytes_done: done,
            total_bytes: total,
            percent,
            mbps,
            download_mbps: 0.0,
            eta_secs,
        });
    }
    Ok(())
}

fn verify_hash(
    image_path: &str,
    device: &str,
    progress: &mut dyn FnMut(ProgressInfo),
) -> Result<(), VerifyError> {
    let total = std::fs::metadata(image_path)
        .map_err(|_| VerifyError::Io("Could not read source image metadata.".to_string()))?
        .len();
    progress(ProgressInfo {
        stage: Stage::Verifying,
        bytes_done: 0,
        total_bytes: total,
        percent: 0.0,
        mbps: 0.0,
        download_mbps: 0.0,
        eta_secs: 0,
    });
    let source = compute_hash(image_path)
        .map_err(|_| VerifyError::Io("Could not hash source image.".to_string()))?;
    let target = compute_hash_prefix(device, total)
        .map_err(|_| VerifyError::Io("Could not hash target device.".to_string()))?;
    if source != target {
        return Err(VerifyError::HashMismatch { source, target });
    }
    progress(ProgressInfo {
        stage: Stage::Verifying,
        bytes_done: total,
        total_bytes: total,
        percent: 100.0,
        mbps: 0.0,
        download_mbps: 0.0,
        eta_secs: 0,
    });
    Ok(())
}
