//! BDASL content identifier helpers.
//!
//! A BDASL CID is a CIDv1 with codec=raw (0x55) and hash=BLAKE3 (0x1e),
//! base32-lowercase-encoded with a leading `b` multibase prefix. This is
//! the same CID form used elsewhere in the DASL ecosystem.

use std::fs;
use std::io::Read;
use std::path::Path;

use crate::error::Result;

/// Compute the BDASL CID of `data` in memory.
pub fn from_bytes(data: &[u8]) -> String {
    cid_from_digest(&blake3::hash(data))
}

/// Stream a file through BLAKE3 without buffering the whole contents.
/// Suited to arbitrarily large inputs.
pub fn from_file(path: &Path) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    let mut file = fs::File::open(path)?;
    let mut buf = [0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(cid_from_digest(&hasher.finalize()))
}

fn cid_from_digest(digest: &blake3::Hash) -> String {
    // CIDv1 binary: version(1) + codec(0x55=raw) + hash_fn(0x1e=blake3) +
    // hash_len(0x20) + 32-byte digest.
    let mut cid_bytes = vec![0x01, 0x55, 0x1e, 0x20];
    cid_bytes.extend_from_slice(digest.as_bytes());
    let mut out = String::from("b");
    base32_lower_encode(&cid_bytes, &mut out);
    out
}

fn base32_lower_encode(data: &[u8], out: &mut String) {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut buffer: u64 = 0;
    let mut bits = 0;
    for &byte in data {
        buffer = (buffer << 8) | byte as u64;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buffer >> bits) & 0x1F) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 0x1F) as usize] as char);
    }
}
