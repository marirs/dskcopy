use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Result};

pub fn compute_hash(path: &str) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0_u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn compute_hash_prefix(path: &str, max_len: u64) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0_u8; 1024 * 1024];
    let mut remain = max_len;
    while remain > 0 {
        let to_read = usize::try_from(remain.min(buf.len() as u64)).unwrap_or(buf.len());
        let n = file.read(&mut buf[..to_read])?;
        if n == 0 {
            break;
        }
        remain = remain.saturating_sub(n as u64);
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}
