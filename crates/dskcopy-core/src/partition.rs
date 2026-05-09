use crate::{FilesystemType, PartitionScheme, ProgressInfo, Stage};
use std::fmt;
use std::io::{Read, Seek, SeekFrom, Write};

// ── Error types ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum FormatError {
    Io(String),
    Unsupported(String),
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormatError::Io(s) => write!(f, "I/O error: {s}"),
            FormatError::Unsupported(s) => write!(f, "Unsupported: {s}"),
        }
    }
}

// ── Partition plan ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PartitionPlan {
    pub target_device: String,
    pub partition_count: u32,
}

pub fn plan_partitions(device: &str) -> PartitionPlan {
    PartitionPlan {
        target_device: device.to_string(),
        partition_count: 0,
    }
}

// ── Windows dual-partition result ─────────────────────────────────────────────

/// Describes the two partitions created for a Windows USB.
#[derive(Debug, Clone)]
pub struct WindowsPartitions {
    pub efi_offset: u64,
    pub efi_size: u64,
    pub data_offset: u64,
    pub data_size: u64,
}

// ── MBR partition entry ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct MbrPartition {
    pub bootable: bool,
    pub partition_type: u8,
    pub start_lba: u32,
    pub size_lba: u32,
}

// ── Public: format_drive ──────────────────────────────────────────────────────

/// Create a partition table and filesystem on `device`.
pub fn format_drive(
    device: &str,
    scheme: PartitionScheme,
    fs: FilesystemType,
    progress: Option<&mut dyn FnMut(ProgressInfo)>,
) -> Result<(), FormatError> {
    if let Some(cb) = progress {
        cb(ProgressInfo {
            stage: Stage::Formatting,
            bytes_done: 0,
            total_bytes: 0,
            percent: 0.0,
            mbps: 0.0,
            download_mbps: 0.0,
            eta_secs: 0,
        });
    }

    match scheme {
        PartitionScheme::Gpt => format_gpt_single(device, fs),
        PartitionScheme::Mbr => format_mbr_single(device, fs),
    }
}

/// Create a GPT disk with EFI System Partition (512 MiB FAT32) + data partition.
/// Returns byte offsets/sizes for each partition so callers can write into them.
pub fn format_windows_usb(
    device: &str,
    progress: Option<&mut dyn FnMut(ProgressInfo)>,
) -> Result<WindowsPartitions, FormatError> {
    if let Some(cb) = progress {
        cb(ProgressInfo {
            stage: Stage::Formatting,
            bytes_done: 0,
            total_bytes: 0,
            percent: 0.0,
            mbps: 0.0,
            download_mbps: 0.0,
            eta_secs: 0,
        });
    }

    let dev_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(device)
        .map_err(|e| FormatError::Io(format!("Cannot open '{}': {}", device, e)))?;

    let disk_size = dev_file
        .metadata()
        .map_err(|e| FormatError::Io(format!("Cannot stat device: {}", e)))?
        .len();

    let sector_size: u64 = 512;
    let efi_lba_start: u64 = 2048;
    // 512 MiB EFI partition
    let efi_sectors: u64 = (512 * 1024 * 1024) / sector_size;
    let efi_lba_end: u64 = efi_lba_start + efi_sectors;
    let data_lba_start: u64 = efi_lba_end + 1;
    let data_lba_end: u64 = (disk_size / sector_size).saturating_sub(34);

    if data_lba_start >= data_lba_end {
        return Err(FormatError::Io(
            "Device too small for Windows dual-partition layout (need >1 GiB).".to_string(),
        ));
    }

    // ── GPT ──────────────────────────────────────────────────────────────────
    let mut gdisk = gpt::GptConfig::new()
        .writable(true)
        .initialized(false)
        .logical_block_size(gpt::disk::LogicalBlockSize::Lb512)
        .open_from_device(Box::new(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(device)
                .map_err(|e| FormatError::Io(format!("Cannot open device for GPT: {}", e)))?,
        ))
        .map_err(|e| FormatError::Io(format!("GPT init failed: {}", e)))?;

    gdisk
        .update_partitions(std::collections::BTreeMap::new())
        .map_err(|e| FormatError::Io(format!("GPT clear failed: {}", e)))?;

    // EFI System Partition
    let efi_type = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B"
        .parse::<gpt::partition_types::Type>()
        .map_err(|e| FormatError::Io(format!("Invalid EFI partition GUID: {}", e)))?;

    gdisk
        .add_partition("EFI", efi_sectors, efi_type, 0, None)
        .map_err(|e| FormatError::Io(format!("Failed to add EFI partition: {}", e)))?;

    // Microsoft Basic Data partition
    let data_type = "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7"
        .parse::<gpt::partition_types::Type>()
        .map_err(|e| FormatError::Io(format!("Invalid data partition GUID: {}", e)))?;

    gdisk
        .add_partition("Windows", data_lba_end - data_lba_start, data_type, 0, None)
        .map_err(|e| FormatError::Io(format!("Failed to add data partition: {}", e)))?;

    gdisk
        .write()
        .map_err(|e| FormatError::Io(format!("Failed to write GPT: {}", e)))?;

    // ── Format EFI partition as FAT32 ─────────────────────────────────────
    let efi_offset = efi_lba_start * sector_size;
    let efi_size = efi_sectors * sector_size;
    {
        let mut dev = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(device)
            .map_err(|e| FormatError::Io(format!("Cannot reopen device for EFI FAT32: {}", e)))?;
        let slice = PartitionSlice::new(&mut dev, efi_offset, efi_size);
        fatfs::format_volume(
            slice,
            fatfs::FormatVolumeOptions::new()
                .volume_label(*b"EFI        ")
                .fat_type(fatfs::FatType::Fat32),
        )
        .map_err(|e| FormatError::Io(format!("EFI FAT32 format failed: {}", e)))?;
    }

    let data_offset = data_lba_start * sector_size;
    let data_size = (data_lba_end - data_lba_start) * sector_size;

    Ok(WindowsPartitions {
        efi_offset,
        efi_size,
        data_offset,
        data_size,
    })
}

// ── GPT single-partition ──────────────────────────────────────────────────────

fn format_gpt_single(device: &str, fs: FilesystemType) -> Result<(), FormatError> {
    let dev_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(device)
        .map_err(|e| FormatError::Io(format!("Cannot open '{}': {}", device, e)))?;

    let disk_size = dev_file
        .metadata()
        .map_err(|e| FormatError::Io(format!("Cannot stat device: {}", e)))?
        .len();

    let sector_size: u64 = 512;
    let start_lba: u64 = 2048;
    let end_lba: u64 = (disk_size / sector_size).saturating_sub(34);

    if end_lba <= start_lba {
        return Err(FormatError::Io("Device too small to partition.".to_string()));
    }

    let mut gdisk = gpt::GptConfig::new()
        .writable(true)
        .initialized(false)
        .logical_block_size(gpt::disk::LogicalBlockSize::Lb512)
        .open_from_device(Box::new(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(device)
                .map_err(|e| FormatError::Io(format!("Cannot open device for GPT: {}", e)))?,
        ))
        .map_err(|e| FormatError::Io(format!("GPT init failed: {}", e)))?;

    gdisk
        .update_partitions(std::collections::BTreeMap::new())
        .map_err(|e| FormatError::Io(format!("GPT clear failed: {}", e)))?;

    let part_type = match fs {
        FilesystemType::Fat32 | FilesystemType::ExFat => "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7"
            .parse::<gpt::partition_types::Type>()
            .map_err(|e| FormatError::Io(format!("Invalid partition type GUID: {}", e)))?,
        FilesystemType::Ntfs => "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7"
            .parse::<gpt::partition_types::Type>()
            .map_err(|e| FormatError::Io(format!("Invalid NTFS partition type GUID: {}", e)))?,
        FilesystemType::Ext2 | FilesystemType::Ext3 | FilesystemType::Ext4 | FilesystemType::None => gpt::partition_types::LINUX_FS,
    };

    gdisk
        .add_partition("data", end_lba - start_lba, part_type, 0, None)
        .map_err(|e| FormatError::Io(format!("Failed to add GPT partition: {}", e)))?;

    gdisk
        .write()
        .map_err(|e| FormatError::Io(format!("Failed to write GPT: {}", e)))?;

    // Format filesystem
    let part_offset = start_lba * sector_size;
    let part_size = (end_lba - start_lba) * sector_size;
    format_filesystem(device, fs, part_offset, part_size)?;

    Ok(())
}

// ── MBR single-partition ──────────────────────────────────────────────────────

fn format_mbr_single(device: &str, fs: FilesystemType) -> Result<(), FormatError> {
    let part_type = mbr_fs_type(fs);
    let sector_size: u64 = 512;
    let start_lba: u32 = 2048;

    let dev_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(device)
        .map_err(|e| FormatError::Io(format!("Cannot open device: {}", e)))?;
    let disk_size = dev_file
        .metadata()
        .map_err(|e| FormatError::Io(format!("Cannot stat: {}", e)))?
        .len();
    let total_sectors = (disk_size / sector_size) as u32;
    let size_lba = total_sectors.saturating_sub(start_lba + 1);

    let partitions = [
        MbrPartition {
            bootable: true,
            partition_type: part_type,
            start_lba,
            size_lba,
        },
        MbrPartition::default(),
        MbrPartition::default(),
        MbrPartition::default(),
    ];

    write_mbr_partition_table(device, &partitions)?;

    let part_offset = start_lba as u64 * sector_size;
    let part_size = size_lba as u64 * sector_size;
    format_filesystem(device, fs, part_offset, part_size)?;

    Ok(())
}

fn mbr_fs_type(fs: FilesystemType) -> u8 {
    match fs {
        FilesystemType::Fat32 => 0x0C,
        FilesystemType::ExFat => 0x07,
        FilesystemType::Ntfs => 0x07,
        FilesystemType::Ext2 | FilesystemType::Ext3 | FilesystemType::Ext4 => 0x83,
        FilesystemType::None => 0x00,
    }
}

// ── MBR partition table writer ────────────────────────────────────────────────

/// Write an MBR partition table (and optionally boot code) to a device.
///
/// `boot_code` should be exactly 440 bytes; pass `None` to leave existing
/// boot code intact (or write zeroes if the device is blank).
pub fn write_mbr_partition_table(
    device: &str,
    partitions: &[MbrPartition],
) -> Result<(), FormatError> {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(device)
        .map_err(|e| FormatError::Io(format!("Cannot open device for MBR write: {}", e)))?;

    let mut mbr = [0u8; 512];
    // Read existing MBR so we don't clobber existing boot code if present
    let _ = f.read_exact(&mut mbr);
    f.seek(SeekFrom::Start(0))
        .map_err(|e| FormatError::Io(e.to_string()))?;

    // Write up to 4 partition entries at offset 446
    for (i, part) in partitions.iter().enumerate().take(4) {
        let base = 446 + i * 16;
        mbr[base] = if part.bootable { 0x80 } else { 0x00 };
        // CHS fields are deprecated; set to 0xFE 0xFF 0xFF for LBA-addressed partitions
        mbr[base + 1] = 0x00;
        mbr[base + 2] = 0x02;
        mbr[base + 3] = 0x00;
        mbr[base + 4] = part.partition_type;
        mbr[base + 5] = 0xFE;
        mbr[base + 6] = 0xFF;
        mbr[base + 7] = 0xFF;
        let sl = part.start_lba.to_le_bytes();
        mbr[base + 8..base + 12].copy_from_slice(&sl);
        let sz = part.size_lba.to_le_bytes();
        mbr[base + 12..base + 16].copy_from_slice(&sz);
    }
    // MBR signature
    mbr[510] = 0x55;
    mbr[511] = 0xAA;

    f.write_all(&mbr)
        .map_err(|e| FormatError::Io(format!("Failed to write MBR: {}", e)))?;
    f.sync_all()
        .map_err(|e| FormatError::Io(format!("Failed to sync MBR: {}", e)))?;
    Ok(())
}

// ── Filesystem formatting ─────────────────────────────────────────────────────

fn format_filesystem(
    device: &str,
    fs: FilesystemType,
    part_offset: u64,
    part_size: u64,
) -> Result<(), FormatError> {
    match fs {
        FilesystemType::Fat32 => {
            let mut dev = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(device)
                .map_err(|e| FormatError::Io(format!("Cannot reopen device for FAT32: {}", e)))?;
            let slice = PartitionSlice::new(&mut dev, part_offset, part_size);
            fatfs::format_volume(
                slice,
                fatfs::FormatVolumeOptions::new()
                    .volume_label(*b"DSKCOPY    ")
                    .fat_type(fatfs::FatType::Fat32),
            )
            .map_err(|e| FormatError::Io(format!("FAT32 format failed: {}", e)))?;
        }
        FilesystemType::Ntfs => {
            format_ntfs(device, part_offset)?;
        }
        FilesystemType::Ext2 => {
            format_ext2_partition(device, "DSKCOPY")?;
        }
        FilesystemType::Ext3 => {
            format_ext3_partition(device, "DSKCOPY")?;
        }
        FilesystemType::Ext4 => {
            format_ext4(device, part_offset)?;
        }
        FilesystemType::ExFat => {
            return Err(FormatError::Unsupported(
                "exFAT formatting is not yet supported. Use --fs fat32.".to_string(),
            ));
        }
        FilesystemType::None => {}
    }
    Ok(())
}

/// Format NTFS via mkntfs if available; graceful fallback otherwise.
pub fn format_ntfs(device: &str, _part_offset: u64) -> Result<(), FormatError> {
    #[cfg(target_os = "windows")]
    {
        // On Windows use the format command
        let status = std::process::Command::new("format")
            .args([device, "/FS:NTFS", "/Q", "/Y"])
            .status()
            .map_err(|e| FormatError::Io(format!("format command failed: {}", e)))?;
        if !status.success() {
            return Err(FormatError::Io("NTFS format failed".to_string()));
        }
        return Ok(());
    }
    #[allow(unreachable_code)]
    {
        // On macOS/Linux try mkntfs (ntfs-3g)
        match std::process::Command::new("mkntfs")
            .args(["-F", "-L", "DSKCOPY", device])
            .status()
        {
            Ok(s) if s.success() => Ok(()),
            Ok(_) => Err(FormatError::Io("mkntfs exited with error".to_string())),
            Err(_) => Err(FormatError::Unsupported(
                "mkntfs not found. Install ntfs-3g, or use --fs fat32 with WIM split.".to_string(),
            )),
        }
    }
}

/// Format ext4 via mkfs.ext4 if available; stub otherwise.
pub fn format_ext4_partition(device: &str, label: &str) -> Result<(), FormatError> {
    match std::process::Command::new("mkfs.ext4")
        .args(["-F", "-L", label, device])
        .status()
    {
        Ok(s) if s.success() => Ok(()),
        Ok(_) => Err(FormatError::Io("mkfs.ext4 exited with error".to_string())),
        Err(_) => Err(FormatError::Unsupported(
            "mkfs.ext4 not found. Install e2fsprogs.".to_string(),
        )),
    }
}

/// Format ext3 via mkfs.ext3; graceful fallback if tools not available.
pub fn format_ext3_partition(device: &str, label: &str) -> Result<(), FormatError> {
    match std::process::Command::new("mkfs.ext3")
        .args(["-F", "-L", label, device])
        .status()
    {
        Ok(s) if s.success() => Ok(()),
        Ok(_) => Err(FormatError::Io("mkfs.ext3 exited with error".to_string())),
        Err(_) => Err(FormatError::Unsupported(
            "mkfs.ext3 not found. Install e2fsprogs.".to_string(),
        )),
    }
}

/// Format ext2 via mkfs.ext2; graceful fallback if tools not available.
pub fn format_ext2_partition(device: &str, label: &str) -> Result<(), FormatError> {
    match std::process::Command::new("mkfs.ext2")
        .args(["-F", "-L", label, device])
        .status()
    {
        Ok(s) if s.success() => Ok(()),
        Ok(_) => Err(FormatError::Io("mkfs.ext2 exited with error".to_string())),
        Err(_) => Err(FormatError::Unsupported(
            "mkfs.ext2 not found. Install e2fsprogs.".to_string(),
        )),
    }
}

fn format_ext4(device: &str, _part_offset: u64) -> Result<(), FormatError> {
    format_ext4_partition(device, "DSKCOPY")
}

// ── UEFI:NTFS layout ──────────────────────────────────────────────────────────

/// Partitions created for the UEFI:NTFS Windows flash mode.
#[derive(Debug, Clone)]
pub struct UefiNtfsPartitions {
    /// Tiny FAT32 partition that holds BOOTX64.EFI (UEFI:NTFS loader)
    pub fat_offset: u64,
    pub fat_size:   u64,
    /// Large NTFS partition that holds the full Windows installer contents
    pub ntfs_offset: u64,
    pub ntfs_size:   u64,
}

/// Create GPT with a 1 MiB FAT32 UEFI:NTFS loader partition followed by a
/// Microsoft Basic Data NTFS partition consuming the rest of the disk.
pub fn format_windows_usb_ntfs(
    device: &str,
    progress: Option<&mut dyn FnMut(ProgressInfo)>,
) -> Result<UefiNtfsPartitions, FormatError> {
    if let Some(cb) = progress {
        cb(ProgressInfo {
            stage: Stage::Formatting,
            bytes_done: 0,
            total_bytes: 0,
            percent: 0.0,
            mbps: 0.0,
            download_mbps: 0.0,
            eta_secs: 0,
        });
    }

    let dev_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(device)
        .map_err(|e| FormatError::Io(format!("Cannot open '{}': {}", device, e)))?;

    let disk_size = dev_file
        .metadata()
        .map_err(|e| FormatError::Io(format!("Cannot stat device: {}", e)))?
        .len();

    let sector_size: u64 = 512;
    // FAT32 UEFI-NTFS loader: 1 MiB
    let fat_lba_start:  u64 = 2048;
    let fat_sectors:    u64 = (1 * 1024 * 1024) / sector_size;
    let fat_lba_end:    u64 = fat_lba_start + fat_sectors;
    let ntfs_lba_start: u64 = fat_lba_end + 1;
    let ntfs_lba_end:   u64 = (disk_size / sector_size).saturating_sub(34);

    if ntfs_lba_start >= ntfs_lba_end {
        return Err(FormatError::Io(
            "Device too small for UEFI:NTFS layout (need >4 MiB).".to_string(),
        ));
    }

    let mut gdisk = gpt::GptConfig::new()
        .writable(true)
        .initialized(false)
        .logical_block_size(gpt::disk::LogicalBlockSize::Lb512)
        .open_from_device(Box::new(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(device)
                .map_err(|e| FormatError::Io(format!("Cannot open device for GPT: {}", e)))?,
        ))
        .map_err(|e| FormatError::Io(format!("GPT init failed: {}", e)))?;

    gdisk
        .update_partitions(std::collections::BTreeMap::new())
        .map_err(|e| FormatError::Io(format!("GPT clear failed: {}", e)))?;

    let efi_type = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B"
        .parse::<gpt::partition_types::Type>()
        .map_err(|e| FormatError::Io(format!("Invalid EFI GUID: {}", e)))?;

    gdisk
        .add_partition("UEFI_NTFS", fat_sectors, efi_type, 0, None)
        .map_err(|e| FormatError::Io(format!("Failed to add FAT32 partition: {}", e)))?;

    let data_type = "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7"
        .parse::<gpt::partition_types::Type>()
        .map_err(|e| FormatError::Io(format!("Invalid data GUID: {}", e)))?;

    gdisk
        .add_partition("Windows", ntfs_lba_end - ntfs_lba_start, data_type, 0, None)
        .map_err(|e| FormatError::Io(format!("Failed to add NTFS partition: {}", e)))?;

    gdisk
        .write()
        .map_err(|e| FormatError::Io(format!("Failed to write GPT: {}", e)))?;

    // Format the FAT32 loader partition
    let fat_offset = fat_lba_start * sector_size;
    let fat_size   = fat_sectors * sector_size;
    {
        let mut dev = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(device)
            .map_err(|e| FormatError::Io(format!("Cannot reopen device for FAT32: {}", e)))?;
        let slice = PartitionSlice::new(&mut dev, fat_offset, fat_size);
        fatfs::format_volume(
            slice,
            fatfs::FormatVolumeOptions::new()
                .volume_label(*b"UEFI_NTFS  ")
                .fat_type(fatfs::FatType::Fat32),
        )
        .map_err(|e| FormatError::Io(format!("FAT32 loader format failed: {}", e)))?;
    }

    let ntfs_offset = ntfs_lba_start * sector_size;
    let ntfs_size   = (ntfs_lba_end - ntfs_lba_start) * sector_size;

    Ok(UefiNtfsPartitions {
        fat_offset,
        fat_size,
        ntfs_offset,
        ntfs_size,
    })
}

// ── PartitionSlice — a Read+Write+Seek wrapper that addresses a sub-region ───

pub struct PartitionSlice<'a> {
    dev: &'a mut std::fs::File,
    offset: u64,
    size: u64,
    pos: u64,
}

impl<'a> PartitionSlice<'a> {
    pub fn new(dev: &'a mut std::fs::File, offset: u64, size: u64) -> Self {
        PartitionSlice { dev, offset, size, pos: 0 }
    }

    fn abs_pos(&self) -> u64 {
        self.offset + self.pos
    }
}

impl<'a> Read for PartitionSlice<'a> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self.size.saturating_sub(self.pos) as usize;
        if remaining == 0 {
            return Ok(0);
        }
        let to_read = buf.len().min(remaining);
        self.dev.seek(SeekFrom::Start(self.abs_pos()))?;
        let n = self.dev.read(&mut buf[..to_read])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl<'a> Write for PartitionSlice<'a> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let remaining = self.size.saturating_sub(self.pos) as usize;
        if remaining == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "Partition region full",
            ));
        }
        let to_write = buf.len().min(remaining);
        self.dev.seek(SeekFrom::Start(self.abs_pos()))?;
        let n = self.dev.write(&buf[..to_write])?;
        self.pos += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.dev.flush()
    }
}

impl<'a> Seek for PartitionSlice<'a> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(n) => n,
            SeekFrom::Current(n) => (self.pos as i64 + n).max(0) as u64,
            SeekFrom::End(n) => (self.size as i64 + n).max(0) as u64,
        };
        self.pos = new_pos.min(self.size);
        Ok(self.pos)
    }
}
