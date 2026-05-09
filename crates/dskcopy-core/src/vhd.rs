//! VHD, VHDX, and FFU image readers.
//!
//! Each function returns a `Box<dyn Read + Send + 'static>` that streams the
//! underlying raw disk data, suitable for passing to `write_stream_reader()`.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};

#[derive(Debug, Clone)]
pub enum VhdError {
    Io(String),
    InvalidFormat(String),
    Unsupported(String),
}

impl std::fmt::Display for VhdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VhdError::Io(s) => write!(f, "VHD I/O error: {s}"),
            VhdError::InvalidFormat(s) => write!(f, "Invalid VHD/VHDX/FFU: {s}"),
            VhdError::Unsupported(s) => write!(f, "Unsupported VHD feature: {s}"),
        }
    }
}

// ── VHD (fixed or dynamic) ────────────────────────────────────────────────────

/// Open a VHD image and return a reader of the underlying raw disk data.
///
/// - Fixed VHD: raw bytes 0..data_size (footer at end is skipped)
/// - Dynamic VHD: BAT-driven sparse layout is stitched into a linear stream
pub fn open_vhd_reader(path: &str) -> Result<Box<dyn Read + Send + 'static>, VhdError> {
    let mut f = File::open(path).map_err(|e| VhdError::Io(e.to_string()))?;
    let file_size = f
        .metadata()
        .map_err(|e| VhdError::Io(e.to_string()))?
        .len();

    if file_size < 512 {
        return Err(VhdError::InvalidFormat("File too small".into()));
    }

    // Read VHD footer (last 512 bytes)
    f.seek(SeekFrom::Start(file_size - 512))
        .map_err(|e| VhdError::Io(e.to_string()))?;
    let mut footer = [0u8; 512];
    f.read_exact(&mut footer)
        .map_err(|e| VhdError::Io(e.to_string()))?;

    if &footer[..8] != b"conectix" {
        return Err(VhdError::InvalidFormat("VHD footer cookie missing".into()));
    }

    let disk_type = u32::from_be_bytes([footer[60], footer[61], footer[62], footer[63]]);
    let disk_size = u64::from_be_bytes([
        footer[40], footer[41], footer[42], footer[43],
        footer[44], footer[45], footer[46], footer[47],
    ]);

    match disk_type {
        2 => {
            // Fixed VHD: raw data from byte 0 to disk_size
            f.seek(SeekFrom::Start(0))
                .map_err(|e| VhdError::Io(e.to_string()))?;
            Ok(Box::new(LimitedReader::new(f, disk_size)))
        }
        3 => {
            // Dynamic VHD
            open_dynamic_vhd(f, &footer, disk_size)
        }
        4 => {
            // Differencing VHD — complex; not supported
            Err(VhdError::Unsupported(
                "Differencing VHD is not supported. Convert to fixed VHD first.".into(),
            ))
        }
        other => Err(VhdError::InvalidFormat(format!(
            "Unknown VHD disk type: {}",
            other
        ))),
    }
}

fn open_dynamic_vhd(
    mut f: File,
    footer: &[u8; 512],
    disk_size: u64,
) -> Result<Box<dyn Read + Send + 'static>, VhdError> {
    // Dynamic VHD has a copy of the footer at byte 0, then the dynamic disk header
    // at byte 512, then the BAT.
    f.seek(SeekFrom::Start(512))
        .map_err(|e| VhdError::Io(e.to_string()))?;
    let mut dyn_header = [0u8; 1024];
    f.read_exact(&mut dyn_header)
        .map_err(|e| VhdError::Io(e.to_string()))?;

    if &dyn_header[..8] != b"cxsparse" {
        return Err(VhdError::InvalidFormat(
            "Dynamic VHD header cookie missing".into(),
        ));
    }

    let bat_offset = u64::from_be_bytes([
        dyn_header[16], dyn_header[17], dyn_header[18], dyn_header[19],
        dyn_header[20], dyn_header[21], dyn_header[22], dyn_header[23],
    ]);
    let max_bat_size = u32::from_be_bytes([
        dyn_header[28], dyn_header[29], dyn_header[30], dyn_header[31],
    ]) as usize;
    let block_size = u32::from_be_bytes([
        dyn_header[32], dyn_header[33], dyn_header[34], dyn_header[35],
    ]) as u64;

    // Read BAT
    f.seek(SeekFrom::Start(bat_offset))
        .map_err(|e| VhdError::Io(e.to_string()))?;
    let mut bat_raw = vec![0u8; max_bat_size * 4];
    f.read_exact(&mut bat_raw)
        .map_err(|e| VhdError::Io(e.to_string()))?;

    let bat: Vec<u32> = bat_raw
        .chunks(4)
        .map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]]))
        .collect();

    let _ = footer; // not needed further

    // Build DynamicVhdReader
    let reader = DynamicVhdReader {
        file: f,
        bat,
        block_size,
        disk_size,
        logical_pos: 0,
    };
    Ok(Box::new(reader))
}

// ── DynamicVhdReader ──────────────────────────────────────────────────────────

struct DynamicVhdReader {
    file: File,
    bat: Vec<u32>,
    block_size: u64,
    disk_size: u64,
    logical_pos: u64,
}

impl Read for DynamicVhdReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.logical_pos >= self.disk_size {
            return Ok(0);
        }
        let remaining = (self.disk_size - self.logical_pos) as usize;
        let to_read = buf.len().min(remaining);
        let buf = &mut buf[..to_read];

        let block_idx = (self.logical_pos / self.block_size) as usize;
        let offset_in_block = self.logical_pos % self.block_size;
        let can_read_in_block = (self.block_size - offset_in_block) as usize;
        let this_read = buf.len().min(can_read_in_block);

        const UNALLOCATED: u32 = 0xFFFF_FFFF;

        if block_idx >= self.bat.len() || self.bat[block_idx] == UNALLOCATED {
            // Unallocated block — return zeros
            buf[..this_read].fill(0);
            self.logical_pos += this_read as u64;
            return Ok(this_read);
        }

        // Calculate absolute offset: BAT entry is in sectors (512 bytes)
        // Add sector bitmap (block_size / 512 / 8 bytes) before data
        let sector_bitmap_size = ((self.block_size / 512) + 7) / 8;
        let block_file_offset =
            self.bat[block_idx] as u64 * 512 + sector_bitmap_size + offset_in_block;

        self.file
            .seek(SeekFrom::Start(block_file_offset))
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        let n = self.file.read(&mut buf[..this_read])?;
        self.logical_pos += n as u64;
        Ok(n)
    }
}

unsafe impl Send for DynamicVhdReader {}

// ── VHDX ─────────────────────────────────────────────────────────────────────

/// Open a VHDX image and return a reader of the raw disk data.
///
/// Supports fixed and dynamic VHDX images.  Differencing (parent) disks are
/// rejected with a clear error message.
pub fn open_vhdx_reader(path: &str) -> Result<Box<dyn Read + Send + 'static>, VhdError> {
    let mut f = File::open(path).map_err(|e| VhdError::Io(e.to_string()))?;

    // ── 1. File identifier at byte 0 ─────────────────────────────────────────
    let mut sig = [0u8; 8];
    f.read_exact(&mut sig).map_err(|e| VhdError::Io(e.to_string()))?;
    if &sig != b"vhdxfile" {
        return Err(VhdError::InvalidFormat("Not a VHDX file (signature missing)".into()));
    }

    // ── 2. Region table (first copy at 0x30000) ───────────────────────────────
    // Layout: file-id(64KB) | padding(64KB) | header1(4KB) | header2(4KB) | …
    // | region-table1(at 0x30000) | region-table2(at 0x31000) |
    let (bat_offset, bat_length, meta_offset, _meta_length) =
        parse_vhdx_region_table(&mut f, 0x30000)?;

    // ── 3. Metadata ───────────────────────────────────────────────────────────
    let meta = parse_vhdx_metadata(&mut f, meta_offset)?;

    if meta.has_parent {
        return Err(VhdError::Unsupported(
            "Differencing VHDX images are not supported. \
             Merge the chain to a single flat image first."
                .into(),
        ));
    }

    // ── 4. BAT ────────────────────────────────────────────────────────────────
    let bat = parse_vhdx_bat(&mut f, bat_offset, bat_length, &meta)?;

    // ── 5. Build reader ───────────────────────────────────────────────────────
    let reader = VhdxReader {
        file: f,
        bat,
        block_size: meta.block_size as u64,
        virtual_size: meta.virtual_disk_size,
        chunk_ratio: meta.chunk_ratio,
        logical_pos: 0,
    };
    Ok(Box::new(reader))
}

// ── VHDX internals ────────────────────────────────────────────────────────────

/// GUIDs are stored mixed-endian in VHDX (LE DWORD, LE WORD, LE WORD, 8 BE bytes).
fn guid_bytes(s: &str) -> [u8; 16] {
    // Parse "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"
    let hex: Vec<u8> = s
        .replace('-', "")
        .as_bytes()
        .chunks(2)
        .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 16).unwrap())
        .collect();
    let mut out = [0u8; 16];
    // Data1 (4 bytes) → swap to LE
    out[0] = hex[3]; out[1] = hex[2]; out[2] = hex[1]; out[3] = hex[0];
    // Data2 (2 bytes) → swap to LE
    out[4] = hex[5]; out[5] = hex[4];
    // Data3 (2 bytes) → swap to LE
    out[6] = hex[7]; out[7] = hex[6];
    // Data4 (8 bytes) → as-is
    out[8..16].copy_from_slice(&hex[8..16]);
    out
}

const GUID_BAT: &str      = "2dc27766-f623-4200-9d64-115e9bfd4a08";
const GUID_METADATA: &str = "8b7ca206-4790-4b9a-b8fe-575f050f886e";
const GUID_FILE_PARAMS:   &str = "caa16737-fa36-4d43-b3b6-33f0aa44e76b";
const GUID_VIRT_DISK_SIZE: &str = "2fa54224-cd1b-4876-b211-5be07a6efa2c";
const GUID_LOGICAL_SECTOR: &str = "8141bf1d-a96f-4709-ba47-f233a8faab5f";

/// Parse the VHDX region table and return (bat_offset, bat_len, meta_offset, meta_len).
fn parse_vhdx_region_table(
    f: &mut File,
    table_offset: u64,
) -> Result<(u64, u64, u64, u64), VhdError> {
    f.seek(SeekFrom::Start(table_offset))
        .map_err(|e| VhdError::Io(e.to_string()))?;

    let mut hdr = [0u8; 16];
    f.read_exact(&mut hdr).map_err(|e| VhdError::Io(e.to_string()))?;

    if &hdr[..4] != b"regi" {
        return Err(VhdError::InvalidFormat("VHDX region table signature missing".into()));
    }
    let entry_count = u32::from_le_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]) as usize;
    if entry_count > 2048 {
        return Err(VhdError::InvalidFormat("VHDX region table: too many entries".into()));
    }

    let bat_guid   = guid_bytes(GUID_BAT);
    let meta_guid  = guid_bytes(GUID_METADATA);

    let mut bat_offset:  u64 = 0;
    let mut bat_len:     u64 = 0;
    let mut meta_offset: u64 = 0;
    let mut meta_len:    u64 = 0;

    for _ in 0..entry_count {
        let mut entry = [0u8; 32];
        f.read_exact(&mut entry).map_err(|e| VhdError::Io(e.to_string()))?;

        let file_offset = u64::from_le_bytes(entry[16..24].try_into().unwrap());
        let length      = u32::from_le_bytes(entry[24..28].try_into().unwrap()) as u64;

        if entry[..16] == bat_guid {
            bat_offset = file_offset;
            bat_len    = length;
        } else if entry[..16] == meta_guid {
            meta_offset = file_offset;
            meta_len    = length;
        }
    }

    if bat_offset == 0 {
        return Err(VhdError::InvalidFormat("VHDX BAT region not found".into()));
    }
    if meta_offset == 0 {
        return Err(VhdError::InvalidFormat("VHDX metadata region not found".into()));
    }

    Ok((bat_offset, bat_len, meta_offset, meta_len))
}

struct VhdxMeta {
    block_size:       u32,
    virtual_disk_size: u64,
    has_parent:       bool,
    chunk_ratio:      u64,
}

fn parse_vhdx_metadata(f: &mut File, meta_region_offset: u64) -> Result<VhdxMeta, VhdError> {
    f.seek(SeekFrom::Start(meta_region_offset))
        .map_err(|e| VhdError::Io(e.to_string()))?;

    let mut hdr = [0u8; 32];
    f.read_exact(&mut hdr).map_err(|e| VhdError::Io(e.to_string()))?;

    if &hdr[..8] != b"metadata" {
        return Err(VhdError::InvalidFormat("VHDX metadata table signature missing".into()));
    }
    let entry_count = u16::from_le_bytes([hdr[10], hdr[11]]) as usize;

    let fp_guid  = guid_bytes(GUID_FILE_PARAMS);
    let vds_guid = guid_bytes(GUID_VIRT_DISK_SIZE);
    let lss_guid = guid_bytes(GUID_LOGICAL_SECTOR);

    let mut block_size:        u32 = 2 * 1024 * 1024; // default 2 MiB
    let mut virtual_disk_size: u64 = 0;
    let mut logical_sector_size: u32 = 512;
    let mut has_parent = false;

    for _ in 0..entry_count {
        let mut entry = [0u8; 32];
        f.read_exact(&mut entry).map_err(|e| VhdError::Io(e.to_string()))?;

        let item_offset = u32::from_le_bytes(entry[16..20].try_into().unwrap()) as u64;
        let item_length = u32::from_le_bytes(entry[20..24].try_into().unwrap()) as usize;
        let saved_pos = f.seek(SeekFrom::Current(0))
            .map_err(|e| VhdError::Io(e.to_string()))?;

        // Read item data
        let read_at = meta_region_offset + item_offset;
        f.seek(SeekFrom::Start(read_at))
            .map_err(|e| VhdError::Io(e.to_string()))?;
        let mut item_data = vec![0u8; item_length.min(32)];
        f.read_exact(&mut item_data).map_err(|e| VhdError::Io(e.to_string()))?;

        if entry[..16] == fp_guid && item_data.len() >= 8 {
            block_size = u32::from_le_bytes(item_data[0..4].try_into().unwrap());
            let bits    = u32::from_le_bytes(item_data[4..8].try_into().unwrap());
            has_parent  = (bits & 2) != 0;
        } else if entry[..16] == vds_guid && item_data.len() >= 8 {
            virtual_disk_size = u64::from_le_bytes(item_data[0..8].try_into().unwrap());
        } else if entry[..16] == lss_guid && item_data.len() >= 4 {
            logical_sector_size = u32::from_le_bytes(item_data[0..4].try_into().unwrap());
        }

        // Restore position in entry table
        f.seek(SeekFrom::Start(saved_pos))
            .map_err(|e| VhdError::Io(e.to_string()))?;
    }

    if virtual_disk_size == 0 {
        return Err(VhdError::InvalidFormat("VHDX virtual disk size is zero".into()));
    }
    if block_size == 0 {
        return Err(VhdError::InvalidFormat("VHDX block size is zero".into()));
    }

    // chunk_ratio = (2^23 * logical_sector_size) / block_size
    let chunk_ratio =
        ((1u64 << 23) * logical_sector_size as u64) / block_size as u64;

    Ok(VhdxMeta {
        block_size,
        virtual_disk_size,
        has_parent,
        chunk_ratio,
    })
}

/// Parse BAT entries into a flat Vec<u64>.
fn parse_vhdx_bat(
    f: &mut File,
    bat_offset: u64,
    bat_length: u64,
    _meta: &VhdxMeta,
) -> Result<Vec<u64>, VhdError> {
    let num_entries = (bat_length / 8) as usize;
    f.seek(SeekFrom::Start(bat_offset))
        .map_err(|e| VhdError::Io(e.to_string()))?;

    let mut buf = vec![0u8; bat_length as usize];
    f.read_exact(&mut buf).map_err(|e| VhdError::Io(e.to_string()))?;

    let bat: Vec<u64> = buf
        .chunks(8)
        .take(num_entries)
        .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
        .collect();

    Ok(bat)
}

// ── VhdxReader ────────────────────────────────────────────────────────────────

/// State for the BAT entry: bits [2:0]
const VHDX_PAYLOAD_FULLY_PRESENT: u64 = 6;

struct VhdxReader {
    file:         File,
    bat:          Vec<u64>,
    block_size:   u64,
    virtual_size: u64,
    chunk_ratio:  u64,
    logical_pos:  u64,
}

impl Read for VhdxReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.logical_pos >= self.virtual_size {
            return Ok(0);
        }
        let remaining = (self.virtual_size - self.logical_pos) as usize;
        let to_read   = buf.len().min(remaining);
        let buf       = &mut buf[..to_read];

        let data_block_idx     = self.logical_pos / self.block_size;
        let offset_in_block    = self.logical_pos % self.block_size;
        let can_read_in_block  = (self.block_size - offset_in_block) as usize;
        let this_read          = buf.len().min(can_read_in_block);

        // BAT index for this data block (interleaved with sector-bitmap entries)
        let bat_idx = if self.chunk_ratio > 0 {
            data_block_idx + data_block_idx / self.chunk_ratio
        } else {
            data_block_idx
        };

        if bat_idx as usize >= self.bat.len() {
            buf[..this_read].fill(0);
            self.logical_pos += this_read as u64;
            return Ok(this_read);
        }

        let bat_entry = self.bat[bat_idx as usize];
        let state     = bat_entry & 0x7;

        if state != VHDX_PAYLOAD_FULLY_PRESENT {
            // PAYLOAD_BLOCK_NOT_PRESENT, ZERO, UNDEFINED, UNMAPPED → emit zeros
            buf[..this_read].fill(0);
            self.logical_pos += this_read as u64;
            return Ok(this_read);
        }

        // FileOffsetMB is stored in bits [63:20]
        let file_offset_mb = bat_entry >> 20;
        let file_offset    = file_offset_mb * (1024 * 1024) + offset_in_block;

        self.file
            .seek(SeekFrom::Start(file_offset))
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

        let n = self.file.read(&mut buf[..this_read])?;
        self.logical_pos += n as u64;
        Ok(n)
    }
}

unsafe impl Send for VhdxReader {}

// ── FFU (Full Flash Update) ───────────────────────────────────────────────────

/// Open an FFU image and return a reader of the raw disk data.
pub fn open_ffu_reader(path: &str) -> Result<Box<dyn Read + Send + 'static>, VhdError> {
    let mut f = File::open(path).map_err(|e| VhdError::Io(e.to_string()))?;

    // FFU header: first 4 bytes = size of signature header (little-endian u32)
    let mut sig_size_buf = [0u8; 4];
    f.read_exact(&mut sig_size_buf)
        .map_err(|e| VhdError::Io(e.to_string()))?;
    let sig_size = u32::from_le_bytes(sig_size_buf) as u64;

    // Skip to signature: "SignedImage " at offset 4
    let mut sig = [0u8; 12];
    f.read_exact(&mut sig)
        .map_err(|e| VhdError::Io(e.to_string()))?;
    if &sig != b"SignedImage " {
        return Err(VhdError::InvalidFormat("FFU signature not found".into()));
    }

    // Skip signature header
    f.seek(SeekFrom::Start(sig_size))
        .map_err(|e| VhdError::Io(e.to_string()))?;

    // Read image header
    let mut img_hdr = [0u8; 256];
    f.read_exact(&mut img_hdr)
        .map_err(|e| VhdError::Io(e.to_string()))?;

    // ImageHeaderSize at offset 0 (u32 LE)
    let img_hdr_size = u32::from_le_bytes([img_hdr[0], img_hdr[1], img_hdr[2], img_hdr[3]]) as u64;
    // ManifestLength at offset 20 (u32 LE)
    let manifest_len = u32::from_le_bytes([img_hdr[20], img_hdr[21], img_hdr[22], img_hdr[23]]) as u64;
    // ChunkSize at offset 24 (u32 LE) — in sectors
    let chunk_size_sectors = u32::from_le_bytes([img_hdr[24], img_hdr[25], img_hdr[26], img_hdr[27]]) as u64;

    let chunk_size = chunk_size_sectors * 512;
    let store_offset = sig_size + img_hdr_size + manifest_len;

    // Skip to store header
    f.seek(SeekFrom::Start(store_offset))
        .map_err(|e| VhdError::Io(e.to_string()))?;

    let mut store_hdr = [0u8; 256];
    f.read_exact(&mut store_hdr)
        .map_err(|e| VhdError::Io(e.to_string()))?;

    // MajorVersion at 0 (u16), StoreHeaderSize at 4 (u32)
    let store_hdr_size = u32::from_le_bytes([store_hdr[4], store_hdr[5], store_hdr[6], store_hdr[7]]) as u64;
    // NumberOfBlockDataEntries at offset 8 (u32)
    let num_entries = u32::from_le_bytes([store_hdr[8], store_hdr[9], store_hdr[10], store_hdr[11]]) as usize;

    // Read block data entries
    let entry_offset = store_offset + store_hdr_size;
    f.seek(SeekFrom::Start(entry_offset))
        .map_err(|e| VhdError::Io(e.to_string()))?;

    let mut entries: Vec<(u64, u32)> = Vec::with_capacity(num_entries); // (lba, chunk_count)
    for _ in 0..num_entries {
        let mut entry = [0u8; 8];
        f.read_exact(&mut entry)
            .map_err(|e| VhdError::Io(e.to_string()))?;
        let lba = u64::from_le_bytes(entry[..8].try_into().unwrap());
        let mut count_buf = [0u8; 4];
        f.read_exact(&mut count_buf)
            .map_err(|e| VhdError::Io(e.to_string()))?;
        let count = u32::from_le_bytes(count_buf);
        entries.push((lba, count));
    }

    // Data starts after entries, aligned to chunk boundary
    let data_start = f
        .seek(SeekFrom::Current(0))
        .map_err(|e| VhdError::Io(e.to_string()))?;
    let aligned_data_start = if data_start % chunk_size == 0 {
        data_start
    } else {
        (data_start / chunk_size + 1) * chunk_size
    };

    let reader = FfuReader {
        file: BufReader::new(f),
        entries,
        chunk_size,
        data_start: aligned_data_start,
        entry_idx: 0,
        chunk_in_entry: 0,
        chunk_pos: 0,
    };

    Ok(Box::new(reader))
}

// ── FfuReader ─────────────────────────────────────────────────────────────────

struct FfuReader {
    file: BufReader<File>,
    entries: Vec<(u64, u32)>,
    chunk_size: u64,
    data_start: u64,
    entry_idx: usize,
    chunk_in_entry: u32,
    chunk_pos: u64, // position within current chunk
}

impl Read for FfuReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.entry_idx < self.entries.len() {
            let (_lba, count) = self.entries[self.entry_idx];
            if self.chunk_in_entry >= count {
                self.entry_idx += 1;
                self.chunk_in_entry = 0;
                self.chunk_pos = 0;
                continue;
            }
            // Calculate file offset for this chunk
            let global_chunk_idx = self.entries[..self.entry_idx]
                .iter()
                .map(|(_, c)| *c as u64)
                .sum::<u64>()
                + self.chunk_in_entry as u64;
            let file_offset = self.data_start + global_chunk_idx * self.chunk_size + self.chunk_pos;

            self.file
                .seek(SeekFrom::Start(file_offset))
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

            let remaining_in_chunk = (self.chunk_size - self.chunk_pos) as usize;
            let to_read = buf.len().min(remaining_in_chunk);
            let n = self.file.read(&mut buf[..to_read])?;
            self.chunk_pos += n as u64;
            if self.chunk_pos >= self.chunk_size {
                self.chunk_in_entry += 1;
                self.chunk_pos = 0;
            }
            if n > 0 {
                return Ok(n);
            }
        }
        Ok(0) // EOF
    }
}

unsafe impl Send for FfuReader {}

// ── LimitedReader ─────────────────────────────────────────────────────────────

struct LimitedReader {
    inner: File,
    remaining: u64,
}

impl LimitedReader {
    fn new(f: File, limit: u64) -> Self {
        LimitedReader { inner: f, remaining: limit }
    }
}

impl Read for LimitedReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            return Ok(0);
        }
        let to_read = buf.len().min(self.remaining as usize);
        let n = self.inner.read(&mut buf[..to_read])?;
        self.remaining -= n as u64;
        Ok(n)
    }
}

unsafe impl Send for LimitedReader {}
