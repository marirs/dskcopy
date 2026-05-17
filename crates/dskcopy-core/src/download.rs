//! ISO/installer download support.
//!
//! Windows ISOs are *not* downloaded automatically: Microsoft's
//! `contentinclude/html` API blocks scripted clients and changes too often to
//! be a reliable target. Instead we expose a curated catalog of versions and
//! the canonical Microsoft download page URL the user should open in a
//! browser.
//!
//! Linux ISOs are downloaded directly via HTTPS using a curated mirror list.
//!
//! macOS full installers are obtained on the user's Mac via
//! `softwareupdate(8)`; we just enumerate what's available and shell out to it.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ── Fido (Microsoft ISO download) constants ──────────────────────────────────

const ORG_ID: &str = "y6jn8c31";
const PROFILE_ID: &str = "606624d44113";
const INSTANCE_ID: &str = "560dc9f3-1aa5-4a2f-b63c-9e18f8d0e175";

// ── Catalog ───────────────────────────────────────────────────────────────────

/// A single entry in the Windows download catalog.
#[derive(Debug, Clone)]
pub struct DownloadEntry {
    /// Short slug used on the CLI (e.g. "win11-24h2")
    pub id: &'static str,
    /// Human-readable name
    pub name: &'static str,
    /// Microsoft product edition ID (kept for reference / future use)
    pub product_edition_id: u32,
    /// Supported locale codes (e.g. "en-us", "de-de")
    pub locales: &'static [&'static str],
}

/// Curated catalog of Windows ISO editions available from Microsoft.
pub static CATALOG: &[DownloadEntry] = &[
    DownloadEntry {
        id:                  "win11-25h2",
        name:                "Windows 11 25H2",
        product_edition_id:  3321,
        locales: &[
            "ar-sa", "bg-bg", "cs-cz", "da-dk", "de-de", "el-gr", "en-gb",
            "en-us", "es-es", "es-mx", "et-ee", "fi-fi", "fr-ca", "fr-fr",
            "he-il", "hr-hr", "hu-hu", "it-it", "ja-jp", "ko-kr", "lt-lt",
            "lv-lv", "nb-no", "nl-nl", "pl-pl", "pt-br", "pt-pt", "ro-ro",
            "ru-ru", "sk-sk", "sl-si", "sr-latn-rs", "sv-se", "th-th",
            "tr-tr", "uk-ua", "zh-cn", "zh-tw",
        ],
    },
    DownloadEntry {
        id:                  "win10-22h2",
        name:                "Windows 10 22H2",
        product_edition_id:  2618,
        locales: &[
            "ar-sa", "bg-bg", "cs-cz", "da-dk", "de-de", "el-gr", "en-gb",
            "en-us", "es-es", "es-mx", "et-ee", "fi-fi", "fr-ca", "fr-fr",
            "he-il", "hr-hr", "hu-hu", "it-it", "ja-jp", "ko-kr", "lt-lt",
            "lv-lv", "nb-no", "nl-nl", "pl-pl", "pt-br", "pt-pt", "ro-ro",
            "ru-ru", "sk-sk", "sl-si", "sr-latn-rs", "sv-se", "th-th",
            "tr-tr", "uk-ua", "zh-cn", "zh-tw",
        ],
    },
];

// ── Errors ────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum DownloadError {
    NotFound(String),
    Network(String),
    Io(String),
    /// Microsoft's Sentinel anti-automation system blocked this request. The
    /// caller should surface a friendly message and direct the user to the
    /// official download page.
    Blocked(String),
    Cancelled,
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DownloadError::NotFound(s) => write!(f, "Not found: {s}"),
            DownloadError::Network(s)  => write!(f, "Network error: {s}"),
            DownloadError::Io(s)       => write!(f, "I/O error: {s}"),
            DownloadError::Blocked(s)  => write!(f, "{s}"),
            DownloadError::Cancelled   => write!(f, "Download cancelled"),
        }
    }
}

// ── Catalog helpers ───────────────────────────────────────────────────────────

/// Return all Windows catalog entries.
pub fn list_catalog() -> &'static [DownloadEntry] {
    CATALOG
}

/// Look up a Windows catalog entry by slug (case-insensitive).
pub fn find_entry(id: &str) -> Option<&'static DownloadEntry> {
    let lower = id.to_lowercase();
    CATALOG.iter().find(|e| e.id.to_lowercase() == lower)
}

/// Return the Microsoft download-page URL the user can open in a browser to
/// obtain an ISO for the given catalog entry.
pub fn get_windows_download_page(entry: &DownloadEntry) -> &'static str {
    match entry.id {
        "win11-25h2" => {
            "https://www.microsoft.com/en-us/software-download/windows11"
        }
        "win10-22h2" => {
            "https://www.microsoft.com/en-us/software-download/windows10ISO"
        }
        _ => "https://www.microsoft.com/en-us/software-download/windows11",
    }
}

// ── Linux catalog ─────────────────────────────────────────────────────────────

/// A single Linux distribution entry available for direct ISO download.
#[derive(Debug, Clone)]
pub struct LinuxEntry {
    pub id: &'static str,
    pub name: &'static str,
    pub url: &'static str,
    pub arch: &'static str,
}

/// Curated list of common Linux distros available via direct HTTPS download.
pub static LINUX_CATALOG: &[LinuxEntry] = &[
    LinuxEntry { id: "ubuntu-24.04",     name: "Ubuntu 24.04 LTS",         url: "https://releases.ubuntu.com/24.04.4/ubuntu-24.04.4-desktop-amd64.iso",                                                     arch: "x86_64"  },
    LinuxEntry { id: "ubuntu-24.04-arm", name: "Ubuntu 24.04 LTS (ARM)",   url: "https://cdimage.ubuntu.com/releases/24.04.4/release/ubuntu-24.04.4-desktop-arm64.iso",                                   arch: "aarch64" },
    LinuxEntry { id: "fedora-41",        name: "Fedora 41 Workstation",    url: "https://download.fedoraproject.org/pub/fedora/linux/releases/41/Workstation/x86_64/iso/Fedora-Workstation-Live-x86_64-41-1.4.iso", arch: "x86_64" },
    LinuxEntry { id: "debian-13",        name: "Debian 13 (Trixie)",       url: "https://cdimage.debian.org/debian-cd/current/amd64/iso-cd/debian-13.4.0-amd64-netinst.iso",                              arch: "x86_64"  },
    LinuxEntry { id: "arch-latest",      name: "Arch Linux (latest)",      url: "https://geo.mirror.pkgbuild.com/iso/latest/archlinux-x86_64.iso",                                                        arch: "x86_64"  },
    LinuxEntry { id: "mint-22",          name: "Linux Mint 22",            url: "https://mirrors.edge.kernel.org/linuxmint/stable/22/linuxmint-22-cinnamon-64bit.iso",                                    arch: "x86_64"  },
];

/// Return all Linux catalog entries.
pub fn list_linux_catalog() -> &'static [LinuxEntry] {
    LINUX_CATALOG
}

// ── macOS catalog ─────────────────────────────────────────────────────────────

/// A single macOS installer entry.
///
/// Two acquisition modes are represented:
///
/// * On macOS, `list_macos_installers()` parses
///   `softwareupdate --list-full-installers` and returns entries with `url =
///   None`. Downloading is delegated back to `softwareupdate(8)`.
/// * On non-macOS hosts, `list_macos_from_catalog()` parses Apple's public
///   software-update catalog and returns entries with `url = Some(...)`
///   pointing at the `InstallAssistant.pkg`. Downloading is a plain HTTPS
///   GET via `download_iso()`.
#[derive(Debug, Clone)]
pub struct MacOsInstaller {
    /// Display name, e.g. "macOS Tahoe 26.4.1" or "macOS Sonoma 14.7".
    pub name: String,
    /// `softwareupdate(8)` `--full-installer-version` value (e.g. "26.4.1").
    pub version: String,
    /// Direct `InstallAssistant.pkg` URL, when known. `None` means use
    /// `softwareupdate --fetch-full-installer` instead.
    pub url: Option<String>,
    /// Approximate download size in bytes, when known.
    pub size_bytes: u64,
}

/// Run `softwareupdate --list-full-installers` and return the parsed list.
///
/// Returns an empty vector on non-macOS hosts or if the command fails.
pub fn list_macos_installers() -> Vec<MacOsInstaller> {
    let output = std::process::Command::new("softwareupdate")
        .arg("--list-full-installers")
        .output();

    let out = match output {
        Ok(o) if o.status.success() || !o.stdout.is_empty() => o,
        _ => return Vec::new(),
    };

    let stdout = String::from_utf8_lossy(&out.stdout);
    // Lines look like:
    //   * Title: macOS Tahoe, Version: 26.4.1, Size: 17814740KiB, Build: 25E253, Deferred: NO
    stdout
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            if !l.starts_with('*') || !l.contains("Title:") {
                return None;
            }
            let title = l.split("Title:").nth(1)?.split(',').next()?.trim().to_string();
            let version = l.split("Version:").nth(1)?.split(',').next()?.trim().to_string();
            let size_bytes = l
                .split("Size:")
                .nth(1)
                .and_then(|s| s.split(',').next())
                .and_then(|s| s.trim().strip_suffix("KiB"))
                .and_then(|n| n.parse::<u64>().ok())
                .map(|kib| kib * 1024)
                .unwrap_or(0);
            if title.is_empty() || version.is_empty() {
                return None;
            }
            Some(MacOsInstaller {
                name: format!("{} {}", title, version),
                version,
                url: None,
                size_bytes,
            })
        })
        .collect()
}

// ── macOS catalog (cross-platform, via Apple's sucatalog) ─────────────────────

/// Apple's combined software-update catalog. Same URL gibMacOS uses by default.
const SUCATALOG_URL: &str = "https://swscan.apple.com/content/catalogs/others/\
    index-15-14-13-12-10.16-10.15-10.14-10.13-10.12-10.11-10.10-10.9-mountainlion-\
    lion-snowleopard-leopard.merged-1.sucatalog.gz";

/// Fetch and parse Apple's sucatalog and return all available macOS full
/// installers.
///
/// This works on any platform — it's pure HTTP + plist parsing — and is the
/// fallback path on Linux/Windows where `softwareupdate(8)` is unavailable.
///
/// The returned entries have `url = Some(InstallAssistant.pkg URL)`.
pub fn list_macos_from_catalog() -> Result<Vec<MacOsInstaller>, DownloadError> {
    use plist::Value;
    use std::io::Read as _;

    let client = build_client()?;
    let resp = client
        .get(SUCATALOG_URL)
        .send()
        .map_err(|e| DownloadError::Network(format!("Cannot fetch sucatalog: {e}")))?;
    if !resp.status().is_success() {
        return Err(DownloadError::Network(format!(
            "HTTP {} fetching sucatalog",
            resp.status()
        )));
    }
    let gz_bytes = resp
        .bytes()
        .map_err(|e| DownloadError::Network(format!("Catalog read error: {e}")))?;

    // Decompress gzip → XML plist.
    let mut decoder = flate2::read::GzDecoder::new(&gz_bytes[..]);
    let mut xml = Vec::with_capacity(gz_bytes.len() * 4);
    decoder
        .read_to_end(&mut xml)
        .map_err(|e| DownloadError::Io(format!("Catalog decompress error: {e}")))?;

    let root: Value = plist::from_bytes(&xml)
        .map_err(|e| DownloadError::Io(format!("Catalog parse error: {e}")))?;

    let products = root
        .as_dictionary()
        .and_then(|d| d.get("Products"))
        .and_then(|v| v.as_dictionary())
        .ok_or_else(|| DownloadError::Io("Catalog missing Products dict".into()))?;

    let mut out: Vec<MacOsInstaller> = Vec::new();
    for (_pid, product) in products.iter() {
        let pdict = match product.as_dictionary() {
            Some(d) => d,
            None => continue,
        };
        // Find the InstallAssistant.pkg URL & size in this product's Packages.
        let packages = match pdict.get("Packages").and_then(|v| v.as_array()) {
            Some(a) => a,
            None => continue,
        };
        let mut pkg_url: Option<String> = None;
        let mut pkg_size: u64 = 0;
        for pkg in packages.iter() {
            let pdict = match pkg.as_dictionary() {
                Some(d) => d,
                None => continue,
            };
            let url = pdict.get("URL").and_then(|v| v.as_string()).unwrap_or("");
            if url.ends_with("/InstallAssistant.pkg") {
                pkg_url = Some(url.to_string());
                pkg_size = pdict
                    .get("Size")
                    .and_then(|v| v.as_unsigned_integer())
                    .or_else(|| pdict.get("Size").and_then(|v| v.as_signed_integer()).map(|i| i as u64))
                    .unwrap_or(0);
                break;
            }
        }
        let pkg_url = match pkg_url {
            Some(u) => u,
            None => continue,
        };
        // English distribution URL gives us the title + version.
        let dist_url = pdict
            .get("Distributions")
            .and_then(|v| v.as_dictionary())
            .and_then(|d| d.get("English"))
            .and_then(|v| v.as_string())
            .map(|s| s.to_string());

        let (name, version) = match dist_url.as_deref() {
            Some(u) => fetch_dist_title_version(&client, u)
                .unwrap_or_else(|| ("macOS".to_string(), String::new())),
            None => ("macOS".to_string(), String::new()),
        };

        let display = if version.is_empty() {
            name.clone()
        } else if name.contains(&version) {
            name.clone()
        } else {
            format!("{} {}", name, version)
        };

        out.push(MacOsInstaller {
            name: display,
            version,
            url: Some(pkg_url),
            size_bytes: pkg_size,
        });
    }

    // Sort newest first by version (lexicographic on dotted numbers is
    // good-enough; ties broken by name).
    out.sort_by(|a, b| {
        version_key(&b.version)
            .cmp(&version_key(&a.version))
            .then(a.name.cmp(&b.name))
    });
    // De-duplicate (Apple often ships the same version under multiple
    // product IDs — keep the first, which is the largest InstallAssistant.pkg).
    out.dedup_by(|a, b| a.name == b.name);

    Ok(out)
}

/// Fetch a `*.English.dist` URL and extract `(title, version)`.
///
/// The dist file is a small XML document. We only need two fields, so a
/// quick string scan beats pulling in a full XML parser.
fn fetch_dist_title_version(
    client: &reqwest::blocking::Client,
    url: &str,
) -> Option<(String, String)> {
    let body = client.get(url).send().ok()?.text().ok()?;
    let title = scan_between(&body, "<title>", "</title>")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "macOS".to_string());
    let version = scan_after_key(&body, "VERSION")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    Some((title, version))
}

/// Return the substring between the first `start` and the next `end`.
fn scan_between<'a>(haystack: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let s = haystack.find(start)? + start.len();
    let rest = &haystack[s..];
    let e = rest.find(end)?;
    Some(&rest[..e])
}

/// Find a plist `<key>NAME</key>` and return the `<string>...</string>` that
/// follows it.
fn scan_after_key(haystack: &str, key: &str) -> Option<String> {
    let needle = format!("<key>{key}</key>");
    let idx = haystack.find(&needle)? + needle.len();
    let rest = &haystack[idx..];
    let s = rest.find("<string>")? + "<string>".len();
    let rest = &rest[s..];
    let e = rest.find("</string>")?;
    Some(rest[..e].to_string())
}

/// Build a Vec<u32> for lexicographic version comparison: `"26.4.1"` →
/// `[26, 4, 1]`. Non-numeric versions sort to the bottom.
fn version_key(v: &str) -> Vec<u32> {
    if v.is_empty() {
        return vec![0];
    }
    v.split('.')
        .map(|p| p.chars().take_while(|c| c.is_ascii_digit()).collect::<String>())
        .map(|s| s.parse::<u32>().unwrap_or(0))
        .collect()
}

// ── Download with progress ────────────────────────────────────────────────────

/// Download `url` to `output_path`, calling `progress_cb` with (bytes_done, total) updates.
///
/// `cancel` is polled between read chunks. When set, the partial file is
/// removed and `DownloadError::Cancelled` is returned.
pub fn download_iso(
    url: &str,
    output_path: &str,
    cancel: &AtomicBool,
    mut progress_cb: impl FnMut(u64, u64),
) -> Result<u64, DownloadError> {
    let client = build_client()?;
    let mut resp = client
        .get(url)
        .send()
        .map_err(|e| DownloadError::Network(e.to_string()))?;

    if !resp.status().is_success() {
        return Err(DownloadError::Network(format!(
            "HTTP {} when downloading ISO",
            resp.status()
        )));
    }

    let total = resp.content_length().unwrap_or(0);

    let mut file = std::fs::File::create(output_path)
        .map_err(|e| DownloadError::Io(format!("Cannot create output file: {e}")))?;

    let mut buf = vec![0u8; 1024 * 1024]; // 1 MiB chunks
    let mut downloaded: u64 = 0;

    loop {
        if cancel.load(Ordering::Relaxed) {
            drop(file);
            let _ = std::fs::remove_file(Path::new(output_path));
            return Err(DownloadError::Cancelled);
        }
        let n = resp
            .read(&mut buf)
            .map_err(|e| DownloadError::Network(format!("Read error during download: {e}")))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .map_err(|e| DownloadError::Io(format!("Write error: {e}")))?;
        downloaded += n as u64;
        progress_cb(downloaded, total);
    }

    Ok(downloaded)
}

// ── HTTP helpers ──────────────────────────────────────────────────────────────

fn build_client() -> Result<reqwest::blocking::Client, DownloadError> {
    reqwest::blocking::Client::builder()
        .user_agent(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
             AppleWebKit/537.36 (KHTML, like Gecko) \
             Chrome/134.0.0.0 Safari/537.36",
        )
        .cookie_store(true)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| DownloadError::Network(format!("Cannot create HTTP client: {e}")))
}

// ── Fido: Windows ISO direct-download flow ────────────────────────────────────

/// Build an HTTP client suitable for talking to Microsoft's download APIs.
///
/// Microsoft serves a different (Media-Creation-Tool) page when the
/// User-Agent looks like Windows; the JSON download endpoints we need only
/// work for non-Windows UAs. Cookies are required because the session that
/// `vlscppe` and `ov-df` whitelist is tracked via cookies.
fn build_ms_client() -> Result<reqwest::blocking::Client, DownloadError> {
    reqwest::blocking::Client::builder()
        .user_agent(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:149.0) \
             Gecko/20100101 Firefox/149.0",
        )
        .cookie_store(true)
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .map_err(|e| DownloadError::Network(format!("Cannot create HTTP client: {e}")))
}

/// Given a Windows catalog entry and a locale code (e.g. `"en-us"`), drive
/// Microsoft's Fido-style download flow and return the direct ISO URL.
///
/// This implements the same six-step dance as Rufus/Fido:
///
/// 1. Generate a fresh session UUID.
/// 2. Whitelist the session via `vlscppe.microsoft.com/tags`.
/// 3. Fetch `ov-df.microsoft.com/mdt.js` to get the `w` and `rticks`
///    challenge values.
/// 4. POST the challenge reply back to `ov-df`.
/// 5. Call `getskuinformationbyproductedition` to find the SKU id matching
///    our locale.
/// 6. Call `GetProductDownloadLinksBySku` and pull the `.iso` URL out of the
///    JSON response.
pub fn get_windows_iso_url(
    entry: &DownloadEntry,
    locale: &str,
) -> Result<String, DownloadError> {
    let client = build_ms_client()?;
    let session_id = uuid::Uuid::new_v4().to_string();

    // Step 2: vlscppe whitelist (response body unused).
    let vlscppe_url = format!(
        "https://vlscppe.microsoft.com/tags?org_id={}&session_id={}",
        ORG_ID, session_id
    );
    let _ = client.get(&vlscppe_url).send();

    // Step 3: ov-df mdt.js challenge.
    let mdt_url = format!(
        "https://ov-df.microsoft.com/mdt.js?instanceId={}&PageId=si&session_id={}",
        INSTANCE_ID, session_id
    );
    let mdt_body = client
        .get(&mdt_url)
        .send()
        .and_then(|r| r.text())
        .unwrap_or_default();
    let w = extract_w(&mdt_body).unwrap_or_default();
    let rticks = extract_rticks(&mdt_body).unwrap_or_default();

    // Step 4: ov-df reply (best-effort).
    if !w.is_empty() {
        let mdt = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let reply_url = format!(
            "https://ov-df.microsoft.com/?session_id={}&CustomerId={}&PageId=si&w={}&mdt={}&rticks={}",
            session_id, INSTANCE_ID, w, mdt, rticks
        );
        let _ = client.get(&reply_url).send();
    }

    // Step 5: SKU information.
    let sku_url = format!(
        "https://www.microsoft.com/software-download-connector/api/getskuinformationbyproductedition\
         ?profile={}&productEditionId={}&SKU=undefined&friendlyFileName=undefined&Locale={}&sessionID={}",
        PROFILE_ID, entry.product_edition_id, locale, session_id
    );
    let sku_resp = client
        .get(&sku_url)
        .header("Accept", "application/json")
        .header("Referer", "https://www.microsoft.com/software-download/windows11")
        .send()
        .map_err(|e| DownloadError::Network(format!("SKU request failed: {e}")))?;
    let status = sku_resp.status();
    let sku_text = sku_resp
        .text()
        .map_err(|e| DownloadError::Network(format!("SKU read error: {e}")))?;
    if !status.is_success() {
        return Err(DownloadError::Network(format!(
            "SKU lookup HTTP {}: Microsoft may be blocking this request",
            status
        )));
    }

    let sku_id = parse_sku_id(&sku_text, locale).ok_or_else(|| {
        // No SKU array at all means the session was blocked / response was
        // an error envelope. Use the friendly Blocked message rather than
        // exposing raw JSON to the user.
        DownloadError::Blocked(
            "Microsoft is limiting automated downloads from your network. \
             Click \"Open in browser\" to download directly from Microsoft's website."
                .into(),
        )
    })?;

    // Step 6: download links.
    let links_url = format!(
        "https://www.microsoft.com/software-download-connector/api/GetProductDownloadLinksBySku\
         ?profile={}&productEditionId=undefined&SKU={}&friendlyFileName=undefined&Locale={}&sessionID={}",
        PROFILE_ID, sku_id, locale, session_id
    );
    let links_resp = client
        .get(&links_url)
        .header("Accept", "application/json")
        .header("Referer", "https://www.microsoft.com/software-download/windows11")
        .send()
        .map_err(|e| DownloadError::Network(format!("Download-link request failed: {e}")))?;
    let status = links_resp.status();
    let links_text = links_resp
        .text()
        .map_err(|e| DownloadError::Network(format!("Link read error: {e}")))?;
    if !status.is_success() {
        return Err(DownloadError::Network(format!(
            "Download-link lookup HTTP {}",
            status
        )));
    }

    parse_download_uri(&links_text).ok_or_else(|| {
        // Microsoft's connector returns Sentinel rejection messages in
        // `Errors[].Value` (e.g. "Sentinel marked this request as rejected.").
        // Surface a clean, non-technical message — never raw JSON — and
        // signal blockage so the UI can offer the browser fallback.
        if parse_ms_error(&links_text).is_some() {
            DownloadError::Blocked(
                "Microsoft is limiting automated downloads from your network. \
                 Click \"Open in browser\" to download directly from Microsoft's website."
                    .into(),
            )
        } else {
            DownloadError::Blocked(
                "Microsoft blocked the automated download. This can happen due to \
                 network restrictions. Click \"Open in browser\" to download directly."
                    .into(),
            )
        }
    })
}

/// Extract a user-facing error message from a Microsoft connector-API
/// response, if present. Handles both the modern flat `{"Errors":[…]}` shape
/// (as returned by `GetProductDownloadLinksBySku` when blocked) and the older
/// `ValidationContainer.Errors` form.
fn parse_ms_error(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let candidates = [
        v.get("Errors").cloned(),
        v.get("ValidationContainer")
            .and_then(|c| c.get("Errors"))
            .cloned(),
    ];
    for c in candidates.into_iter().flatten() {
        let arr = match c.as_array() {
            Some(a) => a,
            None => continue,
        };
        for e in arr {
            if let Some(s) = e.as_str() {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
            if let Some(s) = e.get("Value").and_then(|x| x.as_str()) {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
            if let Some(s) = e.get("Message").and_then(|x| x.as_str()) {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
        }
    }
    None
}

/// Parse the SKU response JSON and return the SkuId for a matching locale.
///
/// Microsoft has changed the shape of this response several times. We try, in
/// order: an array at the root, an object with `Skus`, or `ProductSkus`.
///
/// The SKU records identify language by an *English* language name
/// (e.g. `"English (United States)"`, `"Brazilian Portuguese"`), not by a
/// BCP-47 code, so we map our internal locale code to that name first and
/// match case-insensitively.
fn parse_sku_id(body: &str, locale: &str) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let arr = v
        .as_array()
        .cloned()
        .or_else(|| v.get("Skus").and_then(|x| x.as_array()).cloned())
        .or_else(|| v.get("ProductSkus").and_then(|x| x.as_array()).cloned())?;

    let want = ms_language_name(locale).to_lowercase();
    let want_code = locale.to_lowercase();

    // Helper: extract the SKU id, which may be a string ("18469") or a number.
    let pick_id = |sku: &serde_json::Value| -> Option<u64> {
        sku.get("Id")
            .and_then(|x| x.as_u64())
            .or_else(|| sku.get("SkuId").and_then(|x| x.as_u64()))
            .or_else(|| {
                sku.get("Id")
                    .and_then(|x| x.as_str())
                    .and_then(|s| s.parse::<u64>().ok())
            })
    };

    // Pass 1: exact match of LocalizedLanguage/Language against the English
    // language name we expect.
    for key in &["LocalizedLanguage", "Language"] {
        for sku in arr.iter() {
            if let Some(s) = sku.get(*key).and_then(|x| x.as_str()) {
                if s.to_lowercase() == want {
                    return pick_id(sku);
                }
            }
        }
    }
    // Pass 2: BCP-47 code (in case Microsoft ever stores it).
    for key in &["LocalizedLanguage", "Language", "LanguageCode", "Locale"] {
        for sku in arr.iter() {
            if let Some(s) = sku.get(*key).and_then(|x| x.as_str()) {
                if s.to_lowercase() == want_code {
                    return pick_id(sku);
                }
            }
        }
    }
    // Pass 3: starts-with on the language family (e.g. any "English …" SKU).
    let family = want.split_whitespace().next().unwrap_or(&want);
    for sku in arr.iter() {
        if let Some(s) = sku.get("LocalizedLanguage").and_then(|x| x.as_str()) {
            if s.to_lowercase().starts_with(family) {
                return pick_id(sku);
            }
        }
    }
    // Last resort: first SKU.
    arr.first().and_then(pick_id)
}

/// Map a BCP-47 locale code to the English language name Microsoft uses
/// inside SKU responses.
fn ms_language_name(locale: &str) -> &'static str {
    match locale.to_lowercase().as_str() {
        "ar-sa"      => "Arabic",
        "bg-bg"      => "Bulgarian",
        "cs-cz"      => "Czech",
        "da-dk"      => "Danish",
        "de-de"      => "German",
        "el-gr"      => "Greek",
        "en-gb"      => "English International",
        "en-us"      => "English",
        "es-es"      => "Spanish",
        "es-mx"      => "Spanish (Mexico)",
        "et-ee"      => "Estonian",
        "fi-fi"      => "Finnish",
        "fr-ca"      => "French Canadian",
        "fr-fr"      => "French",
        "he-il"      => "Hebrew",
        "hr-hr"      => "Croatian",
        "hu-hu"      => "Hungarian",
        "it-it"      => "Italian",
        "ja-jp"      => "Japanese",
        "ko-kr"      => "Korean",
        "lt-lt"      => "Lithuanian",
        "lv-lv"      => "Latvian",
        "nb-no"      => "Norwegian",
        "nl-nl"      => "Dutch",
        "pl-pl"      => "Polish",
        "pt-br"      => "Brazilian Portuguese",
        "pt-pt"      => "Portuguese",
        "ro-ro"      => "Romanian",
        "ru-ru"      => "Russian",
        "sk-sk"      => "Slovak",
        "sl-si"      => "Slovenian",
        "sr-latn-rs" => "Serbian Latin",
        "sv-se"      => "Swedish",
        "th-th"      => "Thai",
        "tr-tr"      => "Turkish",
        "uk-ua"      => "Ukrainian",
        "zh-cn"      => "Chinese Simplified",
        "zh-tw"      => "Chinese Traditional",
        _            => "English",
    }
}

/// Parse the download-links response JSON and return a `.iso` URL (preferring
/// x64, falling back to whatever we can find).
fn parse_download_uri(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let opts = v
        .get("ProductDownloadOptions")
        .and_then(|x| x.as_array())
        .cloned()
        .or_else(|| v.as_array().cloned())?;

    let mut x64: Option<String> = None;
    let mut any: Option<String> = None;
    for opt in opts.iter() {
        let uri = opt.get("Uri").and_then(|x| x.as_str()).unwrap_or("");
        if uri.is_empty() {
            continue;
        }
        let dtype = opt
            .get("DownloadType")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        if dtype.eq_ignore_ascii_case("IsoX64") || uri.contains("_x64") {
            x64 = Some(uri.to_string());
            break;
        }
        if any.is_none() {
            any = Some(uri.to_string());
        }
    }
    x64.or(any)
}

fn extract_w(body: &str) -> Option<String> {
    for pat in &["?w=", "&w=", "\"w\":\""] {
        if let Some(pos) = body.find(pat) {
            let rest = &body[pos + pat.len()..];
            let end = rest
                .find(|c: char| !c.is_ascii_hexdigit())
                .unwrap_or(rest.len());
            if end > 0 {
                return Some(rest[..end].to_string());
            }
        }
    }
    None
}

fn extract_rticks(body: &str) -> Option<String> {
    for pat in &["rticks=\"+", "rticks=\\\"+", "rticks=\""] {
        if let Some(pos) = body.find(pat) {
            let rest = &body[pos + pat.len()..];
            let end = rest
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len());
            if end > 0 {
                return Some(rest[..end].to_string());
            }
        }
    }
    None
}

/// Human-readable display name for a locale code used in the catalog.
pub fn locale_display_name(code: &str) -> &'static str {
    match code {
        "ar-sa"      => "Arabic",
        "bg-bg"      => "Bulgarian",
        "cs-cz"      => "Czech",
        "da-dk"      => "Danish",
        "de-de"      => "German",
        "el-gr"      => "Greek",
        "en-gb"      => "English (UK)",
        "en-us"      => "English (US)",
        "es-es"      => "Spanish",
        "es-mx"      => "Spanish (Mexico)",
        "et-ee"      => "Estonian",
        "fi-fi"      => "Finnish",
        "fr-ca"      => "French (Canada)",
        "fr-fr"      => "French",
        "he-il"      => "Hebrew",
        "hr-hr"      => "Croatian",
        "hu-hu"      => "Hungarian",
        "it-it"      => "Italian",
        "ja-jp"      => "Japanese",
        "ko-kr"      => "Korean",
        "lt-lt"      => "Lithuanian",
        "lv-lv"      => "Latvian",
        "nb-no"      => "Norwegian",
        "nl-nl"      => "Dutch",
        "pl-pl"      => "Polish",
        "pt-br"      => "Portuguese (Brazil)",
        "pt-pt"      => "Portuguese",
        "ro-ro"      => "Romanian",
        "ru-ru"      => "Russian",
        "sk-sk"      => "Slovak",
        "sl-si"      => "Slovenian",
        "sr-latn-rs" => "Serbian (Latin)",
        "sv-se"      => "Swedish",
        "th-th"      => "Thai",
        "tr-tr"      => "Turkish",
        "uk-ua"      => "Ukrainian",
        "zh-cn"      => "Chinese (Simplified)",
        "zh-tw"      => "Chinese (Traditional)",
        _            => "English (US)",
    }
}
