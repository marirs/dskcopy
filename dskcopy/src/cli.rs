use clap::{Parser, Subcommand, ValueEnum};
use dskcopy_core::badblock::check_drive;
use dskcopy_core::detect::detect_image_type;
use dskcopy_core::download;
use dskcopy_core::image::{flash, normalize_target_device};
use dskcopy_core::multi_flash::flash_multi;
use dskcopy_core::partition::{format_drive, FormatError};
use dskcopy_core::source::SourceDescriptor;
use dskcopy_core::{
    FilesystemType, FlashMode, FlashOptions, PartitionScheme, ProgressInfo, Stage, VerifyMode,
};
use dskcopy_platform::macos::MacOsBackend;
use dskcopy_platform::{ListOptions, PlatformBackend};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(name = "dskcopy", version, about = "Disk image writer — Phase 3")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// List detected drives (USB removable by default)
    ListDrives {
        /// Show all drives including internal and system volumes
        #[arg(long, default_value_t = false)]
        show_all: bool,
    },

    /// Flash an image (file path or HTTPS URL) to one or more target devices
    Flash {
        /// Source image: local file path or https:// URL
        source: String,
        /// Target device(s) e.g. /dev/disk2 /dev/disk3
        #[arg(required = true)]
        targets: Vec<String>,
        #[arg(long, default_value_t = false)]
        no_verify: bool,
        #[arg(long, default_value_t = 1024 * 1024)]
        block_size: usize,
        #[arg(long, value_enum, default_value_t = VerifyArg::Byte)]
        verify_mode: VerifyArg,
        /// Format target with GPT+FAT32 before writing
        #[arg(long, default_value_t = false)]
        format_first: bool,
        /// Override flash mode
        #[arg(long, value_enum)]
        mode: Option<FlashModeArg>,
        /// Force macOS createinstallmedia path for DMG sources
        #[arg(long, default_value_t = false)]
        macos_installer: bool,
        /// Create a persistence partition of this size (MiB) after Linux extraction
        #[arg(long)]
        persistent: Option<u64>,
        /// Filesystem to use for explicit format operations
        #[arg(long, value_enum, default_value_t = FsArg::Fat32)]
        fs: FsArg,
        /// Bypass Windows 11 TPM/Secure Boot/RAM/CPU requirements (injects autounattend.xml)
        #[arg(long, default_value_t = false)]
        bypass_requirements: bool,
    },

    /// Format a drive with a partition table and filesystem
    Format {
        /// Target device (e.g. /dev/disk2)
        target: String,
        #[arg(long, value_enum, default_value_t = SchemeArg::Gpt)]
        scheme: SchemeArg,
        #[arg(long, value_enum, default_value_t = FsArg::Fat32)]
        fs: FsArg,
    },

    /// Check drive for bad blocks / fake flash capacity
    Check {
        /// Target device (e.g. /dev/disk2)
        target: String,
        /// Quick read-only scan (no destructive write)
        #[arg(long, default_value_t = false)]
        quick: bool,
    },

    /// Download a Windows ISO directly from Microsoft
    Download {
        /// List available Windows versions
        #[arg(long, default_value_t = false)]
        list: bool,
        /// Windows version slug (e.g. win11-24h2, win11-23h2, win10-22h2)
        #[arg(long)]
        version: Option<String>,
        /// Locale / language (e.g. en-us, de-de, fr-fr)
        #[arg(long, default_value = "en-us")]
        lang: String,
        /// Output directory (default: current directory)
        #[arg(long, default_value = ".")]
        output: String,
    },
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum VerifyArg {
    Byte,
    Hash,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum SchemeArg {
    Gpt,
    Mbr,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum FsArg {
    Fat32,
    ExFat,
    Ntfs,
    Ext2,
    Ext3,
    Ext4,
    None,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum FlashModeArg {
    Raw,
    Extracted,
    Windows,
    WindowsNtfs,
    MacosInstaller,
}

fn format_size(size: u64) -> String {
    let gb = size as f64 / (1024.0 * 1024.0 * 1024.0);
    format!("{gb:.2} GiB")
}

fn map_verify_mode(v: VerifyArg) -> VerifyMode {
    match v {
        VerifyArg::Byte => VerifyMode::ByteByByte,
        VerifyArg::Hash => VerifyMode::Sha256,
    }
}

fn map_scheme(s: SchemeArg) -> PartitionScheme {
    match s {
        SchemeArg::Gpt => PartitionScheme::Gpt,
        SchemeArg::Mbr => PartitionScheme::Mbr,
    }
}

fn map_fs(f: FsArg) -> FilesystemType {
    match f {
        FsArg::Fat32 => FilesystemType::Fat32,
        FsArg::ExFat => FilesystemType::ExFat,
        FsArg::Ntfs => FilesystemType::Ntfs,
        FsArg::Ext2 => FilesystemType::Ext2,
        FsArg::Ext3 => FilesystemType::Ext3,
        FsArg::Ext4 => FilesystemType::Ext4,
        FsArg::None => FilesystemType::None,
    }
}

fn map_flash_mode(m: Option<FlashModeArg>) -> FlashMode {
    match m {
        None => FlashMode::RawWrite,
        Some(FlashModeArg::Raw) => FlashMode::RawWrite,
        Some(FlashModeArg::Extracted) => FlashMode::ExtractedFile,
        Some(FlashModeArg::Windows) => FlashMode::WindowsDualPartition,
        Some(FlashModeArg::WindowsNtfs) => FlashMode::WindowsNtfs,
        Some(FlashModeArg::MacosInstaller) => FlashMode::MacosCreateInstallMedia,
    }
}

fn stage_label(s: Stage) -> &'static str {
    match s {
        Stage::Unmounting => "Unmounting",
        Stage::Downloading => "Downloading",
        Stage::Writing => "Writing",
        Stage::Verifying => "Verifying",
        Stage::Formatting => "Formatting",
        Stage::Extracting => "Extracting",
        Stage::Checking => "Checking",
        Stage::Splitting => "Splitting WIM",
        Stage::Installing => "Installing",
        Stage::Done => "Done",
    }
}

// ── list-drives ───────────────────────────────────────────────────────────────

fn list_drives(show_all: bool) -> Result<(), String> {
    if show_all {
        eprintln!("WARNING: Showing all drives including system and internal volumes.");
    }
    let backend = MacOsBackend;
    let opts = ListOptions { show_all };
    let drives = backend.list_devices(&opts)?;
    if drives.is_empty() {
        println!("No removable USB drives detected.");
        return Ok(());
    }
    for d in drives {
        let system_tag = if d.is_system_drive { " [SYSTEM]" } else { "" };
        println!(
            "{}{} | {} | {} | removable={} | bus={} | raw={}",
            d.id,
            system_tag,
            d.name,
            format_size(d.size_bytes),
            d.removable,
            d.bus,
            d.raw_device.unwrap_or_else(|| d.id.clone())
        );
    }
    Ok(())
}

// ── flash ─────────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn do_flash(
    source_input: &str,
    targets: Vec<String>,
    no_verify: bool,
    block_size: usize,
    verify_mode: VerifyArg,
    format_first: bool,
    mode: Option<FlashModeArg>,
    macos_installer: bool,
    persistent: Option<u64>,
    fs: FsArg,
    bypass_requirements: bool,
) -> Result<(), String> {
    // Detect source type and print info
    let source = SourceDescriptor::detect(source_input).map_err(|e| e.to_string())?;
    let image_type = detect_image_type(&source);
    println!("Source: {}", source_input);
    println!("Detected image type: {}", image_type.description());
    println!("Targets: {}", targets.join(", "));

    if bypass_requirements {
        println!(
            "NOTE: Windows 11 hardware requirement bypass enabled (autounattend.xml injection)."
        );
    }

    let opts = FlashOptions {
        block_size,
        verify: !no_verify,
        verify_mode: map_verify_mode(verify_mode),
        format_before_write: format_first,
        format_scheme: PartitionScheme::Gpt,
        format_fs: map_fs(fs),
        flash_mode: map_flash_mode(mode),
        persistent_mb: persistent,
        macos_installer,
        bypass_requirements,
    };

    if targets.len() == 1 {
        // Single target: simple inline progress
        let raw_target = normalize_target_device(&targets[0]);
        let pb = ProgressBar::new(100);
        pb.set_style(
            ProgressStyle::with_template(
                "{msg:20} [{bar:40.cyan/blue}] {pos:>3}% {bytes}/{total_bytes} {binary_bytes_per_sec} ETA {eta}",
            )
            .unwrap()
            .progress_chars("=>-"),
        );

        let pb2 = pb.clone();
        let mut progress = move |p: ProgressInfo| {
            let pos = p.percent.clamp(0.0, 100.0).round() as u64;
            pb2.set_length(100);
            pb2.set_position(pos);
            pb2.set_message(stage_label(p.stage));
        };

        let result = flash(source_input, &raw_target, opts, Some(&mut progress))
            .map_err(|e| e.to_string())?;

        pb.finish_with_message("Done");
        println!(
            "Flashed {} -> {} ({} bytes written, SHA256: {}...)",
            source_input,
            raw_target,
            result.bytes_written,
            &result.sha256[..16.min(result.sha256.len())]
        );
    } else {
        // Multi-target: parallel flash with per-drive progress bars
        let mp = MultiProgress::new();
        let bars: Vec<ProgressBar> = targets
            .iter()
            .map(|t| {
                let pb = mp.add(ProgressBar::new(100));
                pb.set_style(
                    ProgressStyle::with_template("{msg:30} [{bar:30.cyan/blue}] {pos:>3}%")
                        .unwrap()
                        .progress_chars("=>-"),
                );
                pb.set_message(format!("{}: waiting", t));
                pb
            })
            .collect();

        let bars = Arc::new(bars);
        let target_list = targets.clone();
        let bars2 = Arc::clone(&bars);

        let results = flash_multi(source_input, &targets, opts, move |progresses| {
            for tp in &progresses {
                if let Some(idx) = target_list
                    .iter()
                    .position(|t| *t == tp.device || normalize_target_device(t) == tp.device)
                {
                    let pos = tp.info.percent.clamp(0.0, 100.0).round() as u64;
                    bars2[idx].set_position(pos);
                    if let Some(ref err) = tp.error {
                        bars2[idx].set_message(format!("{}: ERROR {}", tp.device, err));
                    } else {
                        bars2[idx].set_message(format!(
                            "{}: {} {:.0}%",
                            tp.device,
                            stage_label(tp.info.stage),
                            tp.info.percent
                        ));
                    }
                }
            }
        });

        for (i, result) in results.iter().enumerate() {
            match result {
                Ok(r) => {
                    bars[i].finish_with_message(format!("{}: Done", targets[i]));
                    println!("[OK] {} -> {} bytes written", targets[i], r.bytes_written);
                }
                Err(e) => {
                    bars[i].abandon_with_message(format!("{}: FAILED", targets[i]));
                    eprintln!("[FAIL] {}: {}", targets[i], e);
                }
            }
        }
    }
    Ok(())
}

// ── format ────────────────────────────────────────────────────────────────────

fn do_format(target: &str, scheme: SchemeArg, fs: FsArg) -> Result<(), String> {
    let raw_target = normalize_target_device(target);
    println!(
        "Formatting {} with {:?} partition table and {:?} filesystem...",
        raw_target, scheme, fs
    );

    let pb = ProgressBar::new_spinner();
    pb.set_message("Formatting...");
    pb.enable_steady_tick(std::time::Duration::from_millis(120));

    format_drive(&raw_target, map_scheme(scheme), map_fs(fs), None)
        .map_err(|e: FormatError| e.to_string())?;

    pb.finish_with_message("Format complete");
    println!("Formatted {} successfully.", raw_target);
    Ok(())
}

// ── check ─────────────────────────────────────────────────────────────────────

fn do_check(target: &str, quick: bool) -> Result<(), String> {
    let raw_target = normalize_target_device(target);

    if !quick {
        println!(
            "WARNING: Full check will write test patterns to '{}'. All data will be destroyed.",
            raw_target
        );
        print!("Continue? (yes/no): ");
        use std::io::Write;
        std::io::stdout().flush().unwrap();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).unwrap();
        if line.trim().to_lowercase() != "yes" {
            println!("Aborted.");
            return Ok(());
        }
    }

    let mode_str = if quick {
        "quick read-only"
    } else {
        "full read-write"
    };
    println!("Starting {} check on {}...", mode_str, raw_target);

    let pb = ProgressBar::new(100);
    pb.set_style(
        ProgressStyle::with_template(
            "Checking  [{bar:40.yellow/white}] {pos:>3}% {binary_bytes_per_sec} ETA {eta}",
        )
        .unwrap()
        .progress_chars("=>-"),
    );

    let mut progress = |p: ProgressInfo| {
        let pos = p.percent.clamp(0.0, 100.0).round() as u64;
        pb.set_length(100);
        pb.set_position(pos);
    };

    let result = check_drive(&raw_target, quick, &mut progress).map_err(|e| e.to_string())?;

    pb.finish_with_message("Check complete");
    println!();
    println!("Drive check results:");
    println!(
        "  Advertised capacity : {}",
        format_size(result.advertised_bytes)
    );
    println!(
        "  Verified good bytes : {}",
        format_size(result.verified_bytes)
    );
    println!("  Bad sectors         : {}", result.bad_sectors.len());
    if !result.bad_sectors.is_empty() {
        let show: Vec<_> = result.bad_sectors.iter().take(10).collect();
        println!(
            "  First bad sectors   : {:?}{}",
            show,
            if result.bad_sectors.len() > 10 {
                " ..."
            } else {
                ""
            }
        );
    }
    if result.is_fake {
        println!("  FAKE FLASH DETECTED: drive appears to misreport its capacity!");
    } else if result.bad_sectors.is_empty() {
        println!("  All sectors OK.");
    }
    Ok(())
}

// ── download ──────────────────────────────────────────────────────────────────

fn do_download(
    list: bool,
    version: Option<String>,
    _lang: String,
    _output: String,
) -> Result<(), String> {
    if list {
        println!("{:<20} {}", "ID", "Name");
        println!("{}", "-".repeat(60));
        for entry in download::list_catalog() {
            println!("{:<20} {}", entry.id, entry.name);
        }
        return Ok(());
    }

    let version_slug = version.ok_or_else(|| {
        "Specify --version <slug> or use --list to see available versions.".to_string()
    })?;

    let entry = download::find_entry(&version_slug).ok_or_else(|| {
        format!(
            "Unknown version '{}'. Use --list to see available versions.",
            version_slug
        )
    })?;

    let url = download::get_windows_download_page(entry);
    println!("Microsoft no longer supports automated ISO downloads.");
    println!(
        "Open the following page in a browser to download {}:",
        entry.name
    );
    println!();
    println!("  {}", url);
    Ok(())
}

// ── main ──────────────────────────────────────────────────────────────────────

fn main() {
    let args = Cli::parse();
    let result = match args.command {
        Some(Commands::ListDrives { show_all }) => list_drives(show_all),
        Some(Commands::Flash {
            source,
            targets,
            no_verify,
            block_size,
            verify_mode,
            format_first,
            mode,
            macos_installer,
            persistent,
            fs,
            bypass_requirements,
        }) => do_flash(
            &source,
            targets,
            no_verify,
            block_size,
            verify_mode,
            format_first,
            mode,
            macos_installer,
            persistent,
            fs,
            bypass_requirements,
        ),
        Some(Commands::Format { target, scheme, fs }) => do_format(&target, scheme, fs),
        Some(Commands::Check { target, quick }) => do_check(&target, quick),
        Some(Commands::Download {
            list,
            version,
            lang,
            output,
        }) => do_download(list, version, lang, output),
        None => {
            use clap::CommandFactory;
            Cli::command().print_help().ok();
            println!();
            Ok(())
        }
    };
    if let Err(err) = result {
        eprintln!("Error: {err}");
        std::process::exit(1);
    }
}
