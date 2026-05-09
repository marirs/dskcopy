//! Microsoft Windows ISO download support.
//!
//! Maintains a catalog of known Windows product edition IDs and uses the
//! Microsoft Software Download API to obtain time-limited direct download URLs.
//!
//! Usage:
//!   dskcopy download --list
//!   dskcopy download --version win11-24h2 --lang en-us --output ./

use std::io::{Read, Write};

// ── Catalog ───────────────────────────────────────────────────────────────────

/// A single entry in the download catalog.
#[derive(Debug, Clone)]
pub struct DownloadEntry {
    /// Short slug used on the CLI (e.g. "win11-24h2")
    pub id: &'static str,
    /// Human-readable name
    pub name: &'static str,
    /// Microsoft product edition ID (used in the API call)
    pub product_edition_id: u32,
    /// Supported locale codes (e.g. "en-us", "de-de")
    pub locales: &'static [&'static str],
}

/// Curated catalog of Windows ISO editions available from Microsoft.
pub static CATALOG: &[DownloadEntry] = &[
    DownloadEntry {
        id:                  "win11-24h2",
        name:                "Windows 11 24H2",
        product_edition_id:  3113,
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
        id:                  "win11-23h2",
        name:                "Windows 11 23H2",
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
    DownloadEntry {
        id:                  "win-server-2022",
        name:                "Windows Server 2022",
        product_edition_id:  1398,
        locales: &["de-de", "en-us", "es-es", "fr-fr", "it-it", "ja-jp", "ko-kr", "pt-br", "ru-ru", "zh-cn", "zh-tw"],
    },
];

// ── Errors ────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum DownloadError {
    NotFound(String),
    Network(String),
    Api(String),
    Io(String),
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DownloadError::NotFound(s) => write!(f, "Not found: {s}"),
            DownloadError::Network(s)  => write!(f, "Network error: {s}"),
            DownloadError::Api(s)      => write!(f, "Microsoft API error: {s}"),
            DownloadError::Io(s)       => write!(f, "I/O error: {s}"),
        }
    }
}

// ── Catalog helpers ───────────────────────────────────────────────────────────

/// Return all catalog entries.
pub fn list_catalog() -> &'static [DownloadEntry] {
    CATALOG
}

/// Look up a catalog entry by slug (case-insensitive).
pub fn find_entry(id: &str) -> Option<&'static DownloadEntry> {
    let lower = id.to_lowercase();
    CATALOG.iter().find(|e| e.id.to_lowercase() == lower)
}

// ── Microsoft download API ────────────────────────────────────────────────────

/// Obtain a direct, time-limited ISO download URL from Microsoft's CDN.
///
/// Flow:
///   1. Fetch the Windows download page to extract a sessionId.
///   2. POST to the SKU-information endpoint for the given productEditionId.
///   3. Parse the returned HTML/JSON fragment to find a download URL for the
///      requested locale.
///
/// The session-based links expire after ~24 h.
pub fn get_download_url(
    entry: &DownloadEntry,
    locale: &str,
) -> Result<String, DownloadError> {
    // 1. Get session ID from the download page
    let session_id = fetch_session_id(entry.product_edition_id)?;

    // 2. Fetch SKU information
    let sku_id = fetch_sku_id(entry.product_edition_id, locale, &session_id)?;

    // 3. Get download link for the SKU
    let url = fetch_download_link(sku_id, locale, &session_id)?;

    Ok(url)
}

/// Windows 11 download page URL (used to obtain a session ID).
const WIN11_PAGE: &str = "https://www.microsoft.com/en-us/software-download/windows11";
const WIN10_PAGE: &str = "https://www.microsoft.com/en-us/software-download/windows10ISO";

/// Content-include API endpoint template.
const API_BASE: &str = "https://www.microsoft.com/api/controls/contentinclude/html";
const PAGE_ID_WIN11: &str = "a224afab-2097-4dfa-a2ba-463eb191a285";
const PAGE_ID_WIN10: &str = "1f5ef5b3-1fd0-4544-8ee6-4454d4d846b0";

/// Fetch the download page and extract the sessionId embedded in the HTML.
fn fetch_session_id(product_edition_id: u32) -> Result<String, DownloadError> {
    let page_url = if product_edition_id >= 3000 || product_edition_id == 2618 {
        WIN11_PAGE
    } else {
        WIN10_PAGE
    };

    let body = http_get(page_url)?;

    // Look for sessionId in JavaScript: var g_sessionId = "...";
    for pattern in &[r#"g_sessionId = ""#, r#"sessionId":""#, r#"SessionId":""#] {
        if let Some(start) = body.find(pattern) {
            let rest = &body[start + pattern.len()..];
            if let Some(end) = rest.find('"') {
                let sid = &rest[..end];
                if !sid.is_empty() && sid.len() < 64 {
                    return Ok(sid.to_string());
                }
            }
        }
    }

    // Fallback: generate a pseudo-random session ID (sometimes accepted)
    // In practice, anonymous downloads may work with any UUID-like value.
    let fake_sid = pseudo_uuid();
    Ok(fake_sid)
}

/// Fetch the SKU ID for the given product edition and locale.
fn fetch_sku_id(
    product_edition_id: u32,
    locale: &str,
    session_id: &str,
) -> Result<u32, DownloadError> {
    let page_id = if product_edition_id >= 3000 {
        PAGE_ID_WIN11
    } else {
        PAGE_ID_WIN10
    };

    let url = format!(
        "{api}?pageId={page_id}&host=www.microsoft.com\
         &segments=software-download,windows11\
         &query=&action=getskuinformationbyproductedition\
         &sessionId={session_id}&productEditionId={product_edition_id}\
         &sKU=&locale={locale}",
        api = API_BASE,
        page_id = page_id,
        session_id = session_id,
        product_edition_id = product_edition_id,
        locale = locale,
    );

    let body = http_get_with_referer(&url, WIN11_PAGE)?;

    // Response is an HTML fragment; search for skuId values.
    // Pattern: value="12345" or "Id":12345
    if let Some(sku) = extract_first_sku_id(&body) {
        return Ok(sku);
    }

    Err(DownloadError::Api(format!(
        "Could not find a SKU for locale '{locale}' in the Microsoft API response. \
         The product edition ID {product_edition_id} may be outdated or the session \
         has expired. Try again or visit {WIN11_PAGE} directly."
    )))
}

/// Extract first numeric SKU ID from Microsoft's HTML fragment.
fn extract_first_sku_id(html: &str) -> Option<u32> {
    // Look for option value="NNNN" in <select>
    for pat in &[r#"value=""#, r#"skuId":"#, r#"SkuId":"#] {
        if let Some(pos) = html.find(pat) {
            let rest = &html[pos + pat.len()..];
            let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
            if let Ok(id) = rest[..end].parse::<u32>() {
                if id > 0 {
                    return Some(id);
                }
            }
        }
    }
    None
}

/// Fetch the direct ISO download URL for a SKU ID.
fn fetch_download_link(
    sku_id: u32,
    locale: &str,
    session_id: &str,
) -> Result<String, DownloadError> {
    let url = format!(
        "{api}?pageId={page_id}&host=www.microsoft.com\
         &segments=software-download,windows11\
         &query=&action=GetProductDownloadLinksBySku\
         &sessionId={session_id}&skuId={sku_id}&sKU=&locale={locale}",
        api = API_BASE,
        page_id = PAGE_ID_WIN11,
        session_id = session_id,
        sku_id = sku_id,
        locale = locale,
    );

    let body = http_get_with_referer(&url, WIN11_PAGE)?;

    // Look for ISO download URL — these are long HTTPS URLs ending in .iso
    if let Some(iso_url) = extract_iso_url(&body) {
        return Ok(iso_url);
    }

    Err(DownloadError::Api(
        "Microsoft API did not return a download URL. \
         The session may have expired or the request was rate-limited. \
         Please visit https://www.microsoft.com/en-us/software-download/windows11 \
         directly to download the ISO."
            .to_string(),
    ))
}

/// Extract the first .iso URL from an HTML/JSON fragment.
fn extract_iso_url(html: &str) -> Option<String> {
    // ISO URLs from Microsoft look like:
    // https://software.download.prss.microsoft.com/...iso
    let lower = html.to_lowercase();
    if let Some(pos) = lower.find("https://software.download") {
        let rest = &html[pos..];
        let end = rest.find(|c: char| c == '"' || c == '<' || c == '\'' || c.is_whitespace())
            .unwrap_or(rest.len());
        let url = &rest[..end];
        if url.to_lowercase().ends_with(".iso") || url.to_lowercase().contains(".iso?") {
            return Some(url.to_string());
        }
    }
    // Fallback: any .iso HTTP URL
    if let Some(pos) = lower.find("http") {
        if let Some(iso_pos) = lower[pos..].find(".iso") {
            let start = pos;
            let end = pos + iso_pos + 4;
            // Walk back to find the start of the URL
            let before = &html[..start];
            let url_start = before
                .rfind(|c: char| c == '"' || c == '<' || c == '\'' || c.is_whitespace())
                .map(|p| p + 1)
                .unwrap_or(0);
            let candidate = &html[url_start..end];
            if candidate.starts_with("http") {
                return Some(candidate.to_string());
            }
        }
    }
    None
}

// ── Download with progress ────────────────────────────────────────────────────

/// Download `url` to `output_path`, calling `progress_cb` with (bytes_done, total) updates.
pub fn download_iso(
    url: &str,
    output_path: &str,
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
             Chrome/124.0.0.0 Safari/537.36",
        )
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| DownloadError::Network(format!("Cannot create HTTP client: {e}")))
}

fn http_get(url: &str) -> Result<String, DownloadError> {
    let client = build_client()?;
    let body = client
        .get(url)
        .send()
        .map_err(|e| DownloadError::Network(e.to_string()))?
        .text()
        .map_err(|e| DownloadError::Network(e.to_string()))?;
    Ok(body)
}

fn http_get_with_referer(url: &str, referer: &str) -> Result<String, DownloadError> {
    let client = build_client()?;
    let body = client
        .get(url)
        .header("Referer", referer)
        .header("Accept", "text/html,application/xhtml+xml")
        .send()
        .map_err(|e| DownloadError::Network(e.to_string()))?
        .text()
        .map_err(|e| DownloadError::Network(e.to_string()))?;
    Ok(body)
}

/// Generate a pseudo-random UUID-like string for use as a session ID fallback.
fn pseudo_uuid() -> String {
    use std::time::SystemTime;
    let t = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0xdeadbeef);
    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        (t & 0xffffffff) as u32,
        ((t >> 32) & 0xffff) as u16,
        ((t >> 48) & 0xfff) as u16,
        ((t >> 60) & 0x3fff | 0x8000) as u16,
        (t.wrapping_mul(6364136223846793005)) & 0xffffffffffff_u128,
    )
}
