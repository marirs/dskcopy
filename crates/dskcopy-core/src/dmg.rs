/// Apple UDIF (Disk Image Format) block-level restore.
///
/// Supports:
///   - koly trailer parsing (512 bytes, big-endian)
///   - XML resource fork (plist) → blkx resource array
///   - Block types: raw/copy (0x00000002), zlib (0x80000005), bzip2 (0x80000006),
///     ignore/free (0x00000000), comment (0x7FFFFFFE), end (0xFFFFFFFF)
///   - Streams decompressed blocks directly to the target device

use crate::image::{FlashError, FlashResult};
use crate::source::SourceDescriptor;
use crate::{FlashOptions, ProgressInfo, Stage};
use byteorder::{BigEndian, ReadBytesExt};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::time::Instant;

// ── koly block constants ──────────────────────────────────────────────────────

const KOLY_MAGIC: u32 = 0x6B6F_6C79; // "koly"
const KOLY_SIZE: u64 = 512;

const BLOCK_TYPE_ZLIB: u32 = 0x8000_0005;
const BLOCK_TYPE_BZIP2: u32 = 0x8000_0006;
const BLOCK_TYPE_RAW: u32 = 0x0000_0002;
const BLOCK_TYPE_ZERO: u32 = 0x0000_0000;
const BLOCK_TYPE_FREE: u32 = 0x0000_0001;
const BLOCK_TYPE_END: u32 = 0xFFFF_FFFF;
const BLOCK_TYPE_COMMENT: u32 = 0x7FFF_FFFE;

const SECTOR_SIZE: u64 = 512;

// ── Public entry point ────────────────────────────────────────────────────────

pub fn restore_dmg(
    source: &SourceDescriptor,
    raw_target: &str,
    _opts: &FlashOptions,
    cb: &mut dyn FnMut(ProgressInfo),
) -> Result<FlashResult, FlashError> {
    let path = &source.input;

    let mut dmg = File::open(path)
        .map_err(|e| FlashError::User(format!("Cannot open DMG '{}': {}", path, e)))?;

    let koly = read_koly(&mut dmg)?;

    let blkx_entries = parse_resource_fork(&mut dmg, &koly)?;

    let total_sectors: u64 = blkx_entries
        .iter()
        .flat_map(|e| &e.blocks)
        .filter(|b| b.block_type != BLOCK_TYPE_END && b.block_type != BLOCK_TYPE_COMMENT)
        .map(|b| b.sector_count)
        .sum();
    let total_bytes = total_sectors * SECTOR_SIZE;

    let mut dst = OpenOptions::new()
        .write(true)
        .open(raw_target)
        .map_err(|e| FlashError::User(format!("Cannot open target device: {}", e)))?;

    let mut hasher = Sha256::new();
    let mut written_bytes = 0u64;
    let start = Instant::now();

    for entry in &blkx_entries {
        let sector_base = entry.sector_offset;

        for block in &entry.blocks {
            match block.block_type {
                BLOCK_TYPE_END | BLOCK_TYPE_COMMENT | BLOCK_TYPE_FREE | BLOCK_TYPE_ZERO => {
                    // ZERO: write zeroed sectors
                    if block.block_type == BLOCK_TYPE_ZERO && block.sector_count > 0 {
                        let zero_bytes = block.sector_count * SECTOR_SIZE;
                        let zero_buf = vec![0u8; zero_bytes as usize];
                        let write_offset = (sector_base + block.sector_number) * SECTOR_SIZE;
                        dst.seek(SeekFrom::Start(write_offset)).map_err(|e| {
                            FlashError::User(format!("Seek failed: {}", e))
                        })?;
                        dst.write_all(&zero_buf).map_err(|e| {
                            FlashError::User(format!("Write zero block failed: {}", e))
                        })?;
                        hasher.update(&zero_buf);
                        written_bytes += zero_bytes;
                    }
                    continue;
                }
                BLOCK_TYPE_RAW => {
                    let data = read_dmg_bytes(
                        &mut dmg,
                        block.compressed_offset,
                        block.compressed_length,
                    )?;
                    let write_offset = (sector_base + block.sector_number) * SECTOR_SIZE;
                    dst.seek(SeekFrom::Start(write_offset)).map_err(|e| {
                        FlashError::User(format!("Seek failed: {}", e))
                    })?;
                    dst.write_all(&data).map_err(|e| {
                        FlashError::User(format!("Write raw block failed: {}", e))
                    })?;
                    hasher.update(&data);
                    written_bytes += data.len() as u64;
                }
                BLOCK_TYPE_ZLIB => {
                    let compressed =
                        read_dmg_bytes(&mut dmg, block.compressed_offset, block.compressed_length)?;
                    let decompressed = decompress_zlib(&compressed)?;
                    let write_offset = (sector_base + block.sector_number) * SECTOR_SIZE;
                    dst.seek(SeekFrom::Start(write_offset)).map_err(|e| {
                        FlashError::User(format!("Seek failed: {}", e))
                    })?;
                    dst.write_all(&decompressed).map_err(|e| {
                        FlashError::User(format!("Write zlib block failed: {}", e))
                    })?;
                    hasher.update(&decompressed);
                    written_bytes += decompressed.len() as u64;
                }
                BLOCK_TYPE_BZIP2 => {
                    let compressed =
                        read_dmg_bytes(&mut dmg, block.compressed_offset, block.compressed_length)?;
                    let decompressed = decompress_bzip2(&compressed)?;
                    let write_offset = (sector_base + block.sector_number) * SECTOR_SIZE;
                    dst.seek(SeekFrom::Start(write_offset)).map_err(|e| {
                        FlashError::User(format!("Seek failed: {}", e))
                    })?;
                    dst.write_all(&decompressed).map_err(|e| {
                        FlashError::User(format!("Write bzip2 block failed: {}", e))
                    })?;
                    hasher.update(&decompressed);
                    written_bytes += decompressed.len() as u64;
                }
                other => {
                    // Unknown block type — skip silently
                    eprintln!("dskcopy: DMG: skipping unknown block type 0x{:08X}", other);
                }
            }

            let elapsed = start.elapsed().as_secs_f64().max(0.001);
            let percent = if total_bytes > 0 {
                (written_bytes as f64 / total_bytes as f64 * 100.0).min(100.0)
            } else {
                0.0
            };
            let mbps = (written_bytes as f64 / (1024.0 * 1024.0)) / elapsed;
            let eta_secs = if total_bytes > 0 && mbps > 0.0 {
                let remain = total_bytes.saturating_sub(written_bytes);
                ((remain as f64 / (1024.0 * 1024.0)) / mbps) as u64
            } else {
                0
            };
            cb(ProgressInfo {
                stage: Stage::Writing,
                bytes_done: written_bytes,
                total_bytes,
                percent,
                mbps,
                download_mbps: 0.0,
                eta_secs,
            });
        }
    }

    dst.sync_all().map_err(|e| {
        FlashError::User(format!("Failed to flush DMG write: {}", e))
    })?;

    let sha256 = format!("{:x}", hasher.finalize());
    Ok(FlashResult {
        bytes_written: written_bytes,
        sha256,
        detected_type: "Apple DMG (UDIF)".to_string(),
    })
}

// ── koly parsing ──────────────────────────────────────────────────────────────

#[derive(Debug)]
struct KolyBlock {
    _version: u32,
    xml_offset: u64,
    xml_length: u64,
}

fn read_koly(f: &mut File) -> Result<KolyBlock, FlashError> {
    let meta = f.metadata().map_err(|e| {
        FlashError::User(format!("Cannot stat DMG file: {}", e))
    })?;
    let len = meta.len();
    if len < KOLY_SIZE {
        return Err(FlashError::User("File too small to be a DMG.".to_string()));
    }
    f.seek(SeekFrom::Start(len - KOLY_SIZE)).map_err(|e| {
        FlashError::User(format!("Cannot seek to koly trailer: {}", e))
    })?;
    let mut buf = vec![0u8; KOLY_SIZE as usize];
    f.read_exact(&mut buf).map_err(|e| {
        FlashError::User(format!("Cannot read koly trailer: {}", e))
    })?;

    let magic = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if magic != KOLY_MAGIC {
        return Err(FlashError::User(
            "Not a valid DMG file (missing koly signature). Use a raw or ISO image for non-DMG files.".to_string(),
        ));
    }

    let version = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    // xml_offset @ 0xD8 = 216, xml_length @ 0xE0 = 224
    let xml_offset = u64::from_be_bytes([
        buf[216], buf[217], buf[218], buf[219],
        buf[220], buf[221], buf[222], buf[223],
    ]);
    let xml_length = u64::from_be_bytes([
        buf[224], buf[225], buf[226], buf[227],
        buf[228], buf[229], buf[230], buf[231],
    ]);

    if xml_length == 0 || xml_length > 64 * 1024 * 1024 {
        return Err(FlashError::User(
            "DMG XML resource fork size is invalid.".to_string(),
        ));
    }

    Ok(KolyBlock {
        _version: version,
        xml_offset,
        xml_length,
    })
}

// ── XML / blkx parsing ────────────────────────────────────────────────────────

#[derive(Debug)]
struct BlkxEntry {
    sector_offset: u64,
    blocks: Vec<BlockDescriptor>,
}

#[derive(Debug)]
struct BlockDescriptor {
    block_type: u32,
    sector_number: u64,
    sector_count: u64,
    compressed_offset: u64,
    compressed_length: u64,
}

fn parse_resource_fork(f: &mut File, koly: &KolyBlock) -> Result<Vec<BlkxEntry>, FlashError> {
    f.seek(SeekFrom::Start(koly.xml_offset)).map_err(|e| {
        FlashError::User(format!("Cannot seek to XML resource fork: {}", e))
    })?;
    let mut xml_buf = vec![0u8; koly.xml_length as usize];
    f.read_exact(&mut xml_buf).map_err(|e| {
        FlashError::User(format!("Cannot read XML resource fork: {}", e))
    })?;

    let value = plist::Value::from_reader_xml(std::io::Cursor::new(&xml_buf)).map_err(|e| {
        FlashError::User(format!("Cannot parse DMG XML resource fork: {}", e))
    })?;

    let root = value.as_dictionary().ok_or_else(|| {
        FlashError::User("DMG XML resource fork has unexpected structure.".to_string())
    })?;

    let blkx_array = root
        .get("resource-fork")
        .and_then(|v| v.as_dictionary())
        .and_then(|d| d.get("blkx"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            FlashError::User(
                "DMG does not contain a blkx resource fork. Unsupported DMG type.".to_string(),
            )
        })?;

    let mut entries = Vec::new();
    for item in blkx_array {
        let dict = match item.as_dictionary() {
            Some(d) => d,
            None => continue,
        };
        let data = match dict.get("Data").and_then(|v| v.as_data()) {
            Some(d) => d,
            None => continue,
        };
        if let Some(entry) = parse_mish(data) {
            entries.push(entry);
        }
    }

    Ok(entries)
}

/// Parse a mish block descriptor (binary, big-endian).
fn parse_mish(data: &[u8]) -> Option<BlkxEntry> {
    if data.len() < 204 {
        return None;
    }
    let mut c = std::io::Cursor::new(data);

    // signature: "mish" = 0x6D697368
    let sig = c.read_u32::<BigEndian>().ok()?;
    if sig != 0x6D69_7368 {
        return None;
    }
    let _version = c.read_u32::<BigEndian>().ok()?;
    let sector_offset = c.read_u64::<BigEndian>().ok()?;
    let _sector_count = c.read_u64::<BigEndian>().ok()?;
    let _data_offset = c.read_u64::<BigEndian>().ok()?;
    let _buffers_needed = c.read_u32::<BigEndian>().ok()?;
    let _block_descriptors = c.read_u32::<BigEndian>().ok()?;
    // 6 reserved u32s
    for _ in 0..6 {
        c.read_u32::<BigEndian>().ok()?;
    }
    // checksum (136 bytes: type u32, size u32, data [128 bytes])
    let _cksum_type = c.read_u32::<BigEndian>().ok()?;
    let _cksum_size = c.read_u32::<BigEndian>().ok()?;
    let mut _cksum_data = [0u8; 128];
    c.read_exact(&mut _cksum_data).ok()?;

    let num_blocks = c.read_u32::<BigEndian>().ok()? as usize;

    let mut blocks = Vec::with_capacity(num_blocks);
    for _ in 0..num_blocks {
        let block_type = c.read_u32::<BigEndian>().ok()?;
        let _comment = c.read_u32::<BigEndian>().ok()?;
        let sector_number = c.read_u64::<BigEndian>().ok()?;
        let sector_count = c.read_u64::<BigEndian>().ok()?;
        let compressed_offset = c.read_u64::<BigEndian>().ok()?;
        let compressed_length = c.read_u64::<BigEndian>().ok()?;

        blocks.push(BlockDescriptor {
            block_type,
            sector_number,
            sector_count,
            compressed_offset,
            compressed_length,
        });
    }

    Some(BlkxEntry { sector_offset, blocks })
}

// ── I/O helpers ───────────────────────────────────────────────────────────────

fn read_dmg_bytes(f: &mut File, offset: u64, length: u64) -> Result<Vec<u8>, FlashError> {
    if length == 0 {
        return Ok(Vec::new());
    }
    f.seek(SeekFrom::Start(offset)).map_err(|e| {
        FlashError::User(format!("DMG seek failed at offset {}: {}", offset, e))
    })?;
    let mut buf = vec![0u8; length as usize];
    f.read_exact(&mut buf).map_err(|e| {
        FlashError::User(format!("DMG read failed ({} bytes at {}): {}", length, offset, e))
    })?;
    Ok(buf)
}

fn decompress_zlib(input: &[u8]) -> Result<Vec<u8>, FlashError> {
    use flate2::read::ZlibDecoder;
    let mut dec = ZlibDecoder::new(input);
    let mut out = Vec::new();
    dec.read_to_end(&mut out)
        .map_err(|e| FlashError::User(format!("zlib decompression failed: {}", e)))?;
    Ok(out)
}

fn decompress_bzip2(input: &[u8]) -> Result<Vec<u8>, FlashError> {
    use bzip2::read::BzDecoder;
    let mut dec = BzDecoder::new(input);
    let mut out = Vec::new();
    dec.read_to_end(&mut out)
        .map_err(|e| FlashError::User(format!("bzip2 decompression failed: {}", e)))?;
    Ok(out)
}
