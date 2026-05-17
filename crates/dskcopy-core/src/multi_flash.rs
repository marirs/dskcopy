//! Multi-target parallel flashing.
//!
//! Reads the source image once into memory (or re-opens the file per thread
//! for large images) and writes to multiple target devices simultaneously.

use crate::image::{FlashError, FlashResult};
use crate::{FlashOptions, ProgressInfo, Stage, TargetProgress};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Flash the same source image to multiple target devices in parallel.
///
/// Each target gets its own thread. Progress is reported via `cb` which
/// receives the current state of all targets on every update.
/// Partial failures are tolerated: if one target fails, others continue.
pub fn flash_multi(
    source: &str,
    targets: &[String],
    opts: FlashOptions,
    cb: impl Fn(Vec<TargetProgress>) + Send + Sync + 'static,
) -> Vec<Result<FlashResult, String>> {
    if targets.is_empty() {
        return Vec::new();
    }

    // Pre-read source into memory for small-to-medium images (<= 8 GiB).
    // For larger images, each thread re-opens the file.
    let source_data = load_source_if_fits(source);

    let cb = Arc::new(cb);
    type FlashSlot = Option<Result<FlashResult, String>>;
    let results: Arc<Mutex<Vec<FlashSlot>>> = Arc::new(Mutex::new(vec![None; targets.len()]));

    let progress_state: Arc<Mutex<Vec<TargetProgress>>> = Arc::new(Mutex::new(
        targets
            .iter()
            .map(|t| TargetProgress {
                device: t.clone(),
                info: ProgressInfo {
                    stage: Stage::Writing,
                    bytes_done: 0,
                    total_bytes: 0,
                    percent: 0.0,
                    mbps: 0.0,
                    download_mbps: 0.0,
                    eta_secs: 0,
                },
                error: None,
            })
            .collect(),
    ));

    std::thread::scope(|s| {
        for (idx, target) in targets.iter().enumerate() {
            let target = target.clone();
            let source = source.to_string();
            let opts = opts.clone();
            let source_data = source_data.clone();
            let results = Arc::clone(&results);
            let progress_state = Arc::clone(&progress_state);
            let cb = Arc::clone(&cb);

            s.spawn(move || {
                let result = flash_single_target(
                    &source,
                    &target,
                    &opts,
                    source_data.as_deref().map(|v| v.as_slice()),
                    idx,
                    &progress_state,
                    &*cb,
                );

                let result_str = result.map_err(|e| e.to_string());
                results.lock().unwrap()[idx] = Some(result_str);
            });
        }
    });

    let results = Arc::try_unwrap(results).unwrap().into_inner().unwrap();
    results
        .into_iter()
        .map(|r| r.unwrap_or(Err("Thread did not complete".to_string())))
        .collect()
}

fn flash_single_target(
    source: &str,
    target: &str,
    opts: &FlashOptions,
    source_data: Option<&[u8]>,
    idx: usize,
    progress_state: &Mutex<Vec<TargetProgress>>,
    cb: &dyn Fn(Vec<TargetProgress>),
) -> Result<FlashResult, FlashError> {
    let total = source_data.map(|d| d.len() as u64).unwrap_or(0);

    let raw_target = crate::image::normalize_target_device(target);

    // Unmount on macOS
    if cfg!(target_os = "macos") {
        update_progress(
            progress_state,
            idx,
            &raw_target,
            Stage::Unmounting,
            0,
            total,
            0.0,
            cb,
        );
        crate::image::unmount_disk(&raw_target)?;
    }

    let mut dst = std::fs::OpenOptions::new()
        .write(true)
        .open(&raw_target)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                FlashError::User("Permission denied. Re-run with sudo.".to_string())
            } else {
                FlashError::User(format!("Cannot open target '{}': {}", raw_target, e))
            }
        })?;

    let block_size = opts.block_size.max(4096);
    let mut written = 0u64;
    let mut hasher = Sha256::new();
    let start = Instant::now();

    match source_data {
        Some(data) => {
            // Write from in-memory buffer
            let mut reader = std::io::Cursor::new(data);
            let mut buf = vec![0u8; block_size];
            loop {
                let n = reader
                    .read(&mut buf)
                    .map_err(|e| FlashError::User(e.to_string()))?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                dst.write_all(&buf[..n])
                    .map_err(|e| FlashError::User(format!("Write error: {e}")))?;
                written += n as u64;

                let elapsed = start.elapsed().as_secs_f64().max(0.001);
                let percent = if total > 0 {
                    (written as f64 / total as f64 * 100.0).min(100.0)
                } else {
                    0.0
                };
                let _mbps = (written as f64 / 1_048_576.0) / elapsed;
                update_progress(
                    progress_state,
                    idx,
                    &raw_target,
                    Stage::Writing,
                    written,
                    total,
                    percent,
                    cb,
                );
            }
        }
        None => {
            // Re-open source file for this thread
            let src_desc = crate::source::SourceDescriptor::detect(source)?;
            let (mut reader, size_hint) = src_desc.open_reader()?;
            let total2 = size_hint.unwrap_or(0);
            let mut buf = vec![0u8; block_size];
            loop {
                let n = reader
                    .read(&mut buf)
                    .map_err(|e| FlashError::User(format!("Read error: {e}")))?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                dst.write_all(&buf[..n])
                    .map_err(|e| FlashError::User(format!("Write error: {e}")))?;
                written += n as u64;

                let _elapsed = start.elapsed().as_secs_f64().max(0.001);
                let percent = if total2 > 0 {
                    (written as f64 / total2 as f64 * 100.0).min(100.0)
                } else {
                    0.0
                };
                update_progress(
                    progress_state,
                    idx,
                    &raw_target,
                    Stage::Writing,
                    written,
                    total2,
                    percent,
                    cb,
                );
            }
        }
    }

    dst.sync_all()
        .map_err(|e| FlashError::User(format!("Sync error: {e}")))?;
    let sha256 = format!("{:x}", hasher.finalize());

    update_progress(
        progress_state,
        idx,
        &raw_target,
        Stage::Done,
        written,
        written,
        100.0,
        cb,
    );

    Ok(FlashResult {
        bytes_written: written,
        sha256,
        detected_type: String::new(),
    })
}

#[allow(clippy::too_many_arguments)]
fn update_progress(
    state: &Mutex<Vec<TargetProgress>>,
    idx: usize,
    device: &str,
    stage: Stage,
    bytes_done: u64,
    total_bytes: u64,
    percent: f64,
    cb: &dyn Fn(Vec<TargetProgress>),
) {
    let snapshot = {
        let mut guard = state.lock().unwrap();
        if idx < guard.len() {
            guard[idx].info = ProgressInfo {
                stage,
                bytes_done,
                total_bytes,
                percent,
                mbps: 0.0,
                download_mbps: 0.0,
                eta_secs: 0,
            };
            guard[idx].device = device.to_string();
        }
        guard.clone()
    };
    cb(snapshot);
}

/// Load source into memory if it fits within 8 GiB.
fn load_source_if_fits(source: &str) -> Option<Arc<Vec<u8>>> {
    const MAX_IN_MEMORY: u64 = 8 * 1024 * 1024 * 1024;

    let desc = crate::source::SourceDescriptor::detect(source).ok()?;
    let (mut reader, size_hint) = desc.open_reader().ok()?;

    let size = size_hint.unwrap_or(0);
    if size > MAX_IN_MEMORY {
        return None;
    }

    let cap = size as usize;
    let mut buf = Vec::with_capacity(cap);
    reader.read_to_end(&mut buf).ok()?;

    if buf.is_empty() {
        None
    } else {
        Some(Arc::new(buf))
    }
}
