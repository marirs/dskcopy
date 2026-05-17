pub mod badblock;
pub mod bios_boot;
pub mod detect;
pub mod dmg;
pub mod download;
pub mod hash;
pub mod i18n;
pub mod image;
pub mod iso_reader;
pub mod linux_iso;
pub mod macos_installer;
pub mod multi_flash;
pub mod partition;
pub mod persistent;
pub mod source;
pub mod verify;
pub mod vhd;
pub mod windows_iso;

// ── Boot assets (rust-embed) ──────────────────────────────────────────────────
#[derive(rust_embed::RustEmbed)]
#[folder = "assets/"]
pub struct BootAssets;

// ── Stage ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Unmounting,
    Downloading,
    Writing,
    Verifying,
    Formatting,
    Extracting,
    Checking,
    Splitting,
    Installing,
    Done,
}

// ── Progress ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ProgressInfo {
    pub stage: Stage,
    pub bytes_done: u64,
    /// 0 means unknown (e.g. streaming compressed source)
    pub total_bytes: u64,
    pub percent: f64,
    pub mbps: f64,
    /// Download speed when source is URL; 0.0 otherwise
    pub download_mbps: f64,
    pub eta_secs: u64,
}

// ── Verify mode ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyMode {
    ByteByByte,
    Sha256,
}

// ── Partition / filesystem types ──────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionScheme {
    Gpt,
    Mbr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilesystemType {
    Fat32,
    ExFat,
    Ntfs,
    Ext2,
    Ext3,
    Ext4,
    None,
}

// ── ISO content classification ────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsoContent {
    WindowsInstaller,
    LinuxUefi,
    LinuxBiosOnly,
    MacosInstaller,
    Generic,
}

// ── Flash mode ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FlashMode {
    /// Stream raw bytes to device (default)
    #[default]
    RawWrite,
    /// Format drive, extract ISO contents to filesystem
    ExtractedFile,
    /// GPT EFI-SP + data partition; WIM-split if needed
    WindowsDualPartition,
    /// GPT tiny-FAT32 (UEFI:NTFS loader) + large NTFS data partition
    WindowsNtfs,
    /// Delegate to macOS createinstallmedia
    MacosCreateInstallMedia,
}

// ── Flash options ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct FlashOptions {
    pub block_size: usize,
    pub verify: bool,
    pub verify_mode: VerifyMode,
    pub format_before_write: bool,
    pub format_scheme: PartitionScheme,
    pub format_fs: FilesystemType,
    /// Override automatic mode selection
    pub flash_mode: FlashMode,
    /// Create a persistence partition of this many MiB after extracting a Linux ISO
    pub persistent_mb: Option<u64>,
    /// Force macOS createinstallmedia path for DMG sources
    pub macos_installer: bool,
    /// Inject autounattend.xml that bypasses Win 11 TPM/Secure Boot/RAM checks
    pub bypass_requirements: bool,
}

impl Default for FlashOptions {
    fn default() -> Self {
        Self {
            block_size: 1024 * 1024,
            verify: true,
            verify_mode: VerifyMode::ByteByByte,
            format_before_write: false,
            format_scheme: PartitionScheme::Gpt,
            format_fs: FilesystemType::Fat32,
            flash_mode: FlashMode::default(),
            persistent_mb: None,
            macos_installer: false,
            bypass_requirements: false,
        }
    }
}

// ── Drive check result ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DriveCheckResult {
    pub advertised_bytes: u64,
    pub verified_bytes: u64,
    pub bad_sectors: Vec<u64>,
    pub is_fake: bool,
}

// ── Multi-target progress ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct TargetProgress {
    pub device: String,
    pub info: ProgressInfo,
    pub error: Option<String>,
}
