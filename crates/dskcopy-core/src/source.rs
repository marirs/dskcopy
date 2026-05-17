use crate::image::FlashError;
use flate2::read::GzDecoder;
use std::fs::File;
use std::io::{self, BufReader, Read};
use xz2::read::XzDecoder;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    Gz,
    Xz,
    Zip,
    Bzip2,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceKind {
    LocalFile,
    HttpUrl,
}

#[derive(Debug, Clone)]
pub struct SourceDescriptor {
    pub input: String,
    pub kind: SourceKind,
    pub compression: Compression,
}

impl SourceDescriptor {
    pub fn detect(input: &str) -> Result<Self, FlashError> {
        let kind = if input.starts_with("http://") || input.starts_with("https://") {
            SourceKind::HttpUrl
        } else {
            SourceKind::LocalFile
        };

        // First sniff from extension
        let lower = input.to_lowercase();
        let ext_compression = if lower.ends_with(".gz") || lower.ends_with(".tgz") {
            Compression::Gz
        } else if lower.ends_with(".xz") {
            Compression::Xz
        } else if lower.ends_with(".zip") {
            Compression::Zip
        } else if lower.ends_with(".bz2") || lower.ends_with(".bzip2") {
            Compression::Bzip2
        } else {
            Compression::None
        };

        // For local files, confirm/override with magic bytes
        let compression = if kind == SourceKind::LocalFile && ext_compression == Compression::None {
            magic_detect(input).unwrap_or(Compression::None)
        } else if kind == SourceKind::LocalFile {
            // Let magic override extension if they conflict
            magic_detect(input).unwrap_or(ext_compression)
        } else {
            ext_compression
        };

        Ok(SourceDescriptor {
            input: input.to_string(),
            kind,
            compression,
        })
    }

    /// Returns a boxed reader over the (possibly decompressed) source data,
    /// plus the known uncompressed byte count if determinable.
    pub fn open_reader(&self) -> Result<(Box<dyn Read + Send + 'static>, Option<u64>), FlashError> {
        match &self.kind {
            SourceKind::LocalFile => self.open_local_reader(),
            SourceKind::HttpUrl => self.open_url_reader(),
        }
    }

    fn open_local_reader(
        &self,
    ) -> Result<(Box<dyn Read + Send + 'static>, Option<u64>), FlashError> {
        let path = &self.input;
        let file = File::open(path).map_err(|e| {
            FlashError::User(format!("Could not open source file '{}': {}", path, e))
        })?;
        let file_size = file.metadata().map(|m| m.len()).unwrap_or(0);

        match self.compression {
            Compression::None => {
                let size = if file_size > 0 { Some(file_size) } else { None };
                Ok((Box::new(BufReader::new(file)), size))
            }
            Compression::Gz => {
                let uncompressed_size = gz_uncompressed_size(path);
                let reader = GzDecoder::new(BufReader::new(file));
                Ok((Box::new(reader), uncompressed_size))
            }
            Compression::Xz => {
                let reader = XzDecoder::new(BufReader::new(file));
                Ok((Box::new(reader), None))
            }
            Compression::Bzip2 => {
                use bzip2::read::BzDecoder;
                let reader = BzDecoder::new(BufReader::new(file));
                Ok((Box::new(reader), None))
            }
            Compression::Zip => {
                let file2 = File::open(path).map_err(|e| {
                    FlashError::User(format!("Could not reopen zip file '{}': {}", path, e))
                })?;
                let mut archive = zip::ZipArchive::new(file2).map_err(|e| {
                    FlashError::User(format!("Could not parse zip archive '{}': {}", path, e))
                })?;
                if archive.is_empty() {
                    return Err(FlashError::User("Zip archive is empty.".to_string()));
                }
                let entry = archive
                    .by_index(0)
                    .map_err(|e| FlashError::User(format!("Could not open zip entry: {}", e)))?;
                let uncompressed = entry.size();
                let size = if uncompressed > 0 {
                    Some(uncompressed)
                } else {
                    None
                };
                // We need to read the whole entry into a Vec since zip::ZipFile
                // borrows the archive. For streaming we use a pipe via thread.
                let (reader, _size) = zip_entry_to_reader(path, size)?;
                Ok((reader, size))
            }
        }
    }

    fn open_url_reader(&self) -> Result<(Box<dyn Read + Send + 'static>, Option<u64>), FlashError> {
        let url = self.input.clone();
        let compression = self.compression;

        let response = reqwest::blocking::Client::new()
            .get(&url)
            .header("User-Agent", "dskcopy/0.2")
            .send()
            .map_err(|e| FlashError::User(format!("HTTP request failed: {}", e)))?;

        if !response.status().is_success() {
            return Err(FlashError::User(format!(
                "HTTP {} for URL: {}",
                response.status(),
                url
            )));
        }

        let content_length = response.content_length();
        let accepts_ranges = response
            .headers()
            .get("accept-ranges")
            .and_then(|v| v.to_str().ok())
            .map(|v| v != "none")
            .unwrap_or(false);

        let base_reader = ResumeReader::new(url, response, 0, accepts_ranges);

        let (reader, size): (Box<dyn Read + Send + 'static>, Option<u64>) = match compression {
            Compression::None => (Box::new(base_reader), content_length),
            Compression::Gz => {
                let dec = GzDecoder::new(base_reader);
                (Box::new(dec), None)
            }
            Compression::Xz => {
                let dec = XzDecoder::new(base_reader);
                (Box::new(dec), None)
            }
            Compression::Bzip2 => {
                use bzip2::read::BzDecoder;
                let dec = BzDecoder::new(base_reader);
                (Box::new(dec), None)
            }
            Compression::Zip => {
                // For URL+zip: download into memory buffer, then parse
                let mut buf = Vec::new();
                let mut r = base_reader;
                r.read_to_end(&mut buf)
                    .map_err(|e| FlashError::User(format!("Failed to download zip: {}", e)))?;
                let cursor = std::io::Cursor::new(buf);
                let mut archive = zip::ZipArchive::new(cursor).map_err(|e| {
                    FlashError::User(format!("Could not parse downloaded zip: {}", e))
                })?;
                if archive.is_empty() {
                    return Err(FlashError::User(
                        "Downloaded zip archive is empty.".to_string(),
                    ));
                }
                let entry = archive
                    .by_index(0)
                    .map_err(|e| FlashError::User(format!("Could not open zip entry: {}", e)))?;
                let uncompressed = entry.size();
                let mut data = Vec::with_capacity(uncompressed as usize);
                entry
                    .take(uncompressed)
                    .read_to_end(&mut data)
                    .map_err(|e| {
                        FlashError::User(format!("Failed to decompress zip entry: {}", e))
                    })?;
                let size = if data.is_empty() {
                    None
                } else {
                    Some(data.len() as u64)
                };
                (Box::new(std::io::Cursor::new(data)), size)
            }
        };

        Ok((reader, size))
    }
}

// ── Resume-capable HTTP reader ────────────────────────────────────────────────

pub struct ResumeReader {
    url: String,
    inner: reqwest::blocking::Response,
    bytes_read: u64,
    accepts_ranges: bool,
}

impl ResumeReader {
    fn new(
        url: String,
        response: reqwest::blocking::Response,
        bytes_read: u64,
        accepts_ranges: bool,
    ) -> Self {
        ResumeReader {
            url,
            inner: response,
            bytes_read,
            accepts_ranges,
        }
    }

    fn reconnect(&mut self) -> io::Result<()> {
        let mut builder = reqwest::blocking::Client::new()
            .get(&self.url)
            .header("User-Agent", "dskcopy/0.2");
        if self.accepts_ranges && self.bytes_read > 0 {
            builder = builder.header("Range", format!("bytes={}-", self.bytes_read));
        }
        let response = builder
            .send()
            .map_err(|e| io::Error::new(io::ErrorKind::ConnectionAborted, e.to_string()))?;
        if !response.status().is_success() && response.status().as_u16() != 206 {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                format!("HTTP {} on resume", response.status()),
            ));
        }
        self.inner = response;
        Ok(())
    }
}

impl Read for ResumeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        const MAX_RETRIES: usize = 3;
        for attempt in 0..MAX_RETRIES {
            match self.inner.read(buf) {
                Ok(n) => {
                    self.bytes_read += n as u64;
                    return Ok(n);
                }
                Err(e) if attempt < MAX_RETRIES - 1 => {
                    if self.accepts_ranges {
                        let _ = self.reconnect();
                    } else {
                        return Err(e);
                    }
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Detect compression from magic bytes (first 16 bytes of file).
fn magic_detect(path: &str) -> Option<Compression> {
    let mut f = File::open(path).ok()?;
    let mut magic = [0u8; 16];
    let n = f.read(&mut magic).ok()?;
    if n < 2 {
        return None;
    }
    // GZ: 1F 8B
    if magic[0] == 0x1f && magic[1] == 0x8b {
        return Some(Compression::Gz);
    }
    // XZ: FD 37 7A 58 5A 00
    if n >= 6 && &magic[..6] == b"\xfd7zXZ\x00" {
        return Some(Compression::Xz);
    }
    // ZIP: 50 4B 03 04
    if n >= 4 && &magic[..4] == b"PK\x03\x04" {
        return Some(Compression::Zip);
    }
    // BZ2: 42 5A 68
    if n >= 3 && &magic[..3] == b"BZh" {
        return Some(Compression::Bzip2);
    }
    None
}

/// Peek the ISIZE field from the last 4 bytes of a .gz file (valid for files < 4 GiB).
fn gz_uncompressed_size(path: &str) -> Option<u64> {
    use std::io::{Seek, SeekFrom};
    let mut f = File::open(path).ok()?;
    f.seek(SeekFrom::End(-4)).ok()?;
    let mut buf = [0u8; 4];
    f.read_exact(&mut buf).ok()?;
    let isize = u32::from_le_bytes(buf);
    if isize == 0 {
        None
    } else {
        Some(isize as u64)
    }
}

/// Extract a zip entry into a pipe so it can be streamed without holding the archive borrow.
fn zip_entry_to_reader(
    path: &str,
    _size_hint: Option<u64>,
) -> Result<(Box<dyn Read + Send + 'static>, ()), FlashError> {
    use std::sync::mpsc;
    use std::thread;

    let path = path.to_string();
    let (tx, rx) = mpsc::sync_channel::<Result<Vec<u8>, String>>(64);

    thread::spawn(move || {
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(e) => {
                let _ = tx.send(Err(format!("Could not open zip: {}", e)));
                return;
            }
        };
        let mut archive = match zip::ZipArchive::new(file) {
            Ok(a) => a,
            Err(e) => {
                let _ = tx.send(Err(format!("Could not parse zip: {}", e)));
                return;
            }
        };
        let mut entry = match archive.by_index(0) {
            Ok(e) => e,
            Err(e) => {
                let _ = tx.send(Err(format!("Could not read zip entry: {}", e)));
                return;
            }
        };
        let mut buf = vec![0u8; 1024 * 1024];
        loop {
            match entry.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(Ok(buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(format!("Read error: {}", e)));
                    break;
                }
            }
        }
    });

    let reader = ChannelReader {
        rx,
        buf: Vec::new(),
        pos: 0,
    };
    Ok((Box::new(reader), ()))
}

struct ChannelReader {
    rx: std::sync::mpsc::Receiver<Result<Vec<u8>, String>>,
    buf: Vec<u8>,
    pos: usize,
}

impl Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.pos >= self.buf.len() {
            match self.rx.recv() {
                Ok(Ok(chunk)) => {
                    self.buf = chunk;
                    self.pos = 0;
                }
                Ok(Err(e)) => return Err(io::Error::other(e)),
                Err(_) => return Ok(0), // channel closed = EOF
            }
        }
        let avail = self.buf.len() - self.pos;
        let n = avail.min(out.len());
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

// ChannelReader is not Send by default due to Receiver. Since we control the
// thread it lives in and never share it, we declare it Send manually.
unsafe impl Send for ChannelReader {}
