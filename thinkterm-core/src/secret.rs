//! At-rest encryption for secrets stored by ThinkTerm.

use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use openssl::symm::{decrypt_aead, encrypt_aead, Cipher};
use std::fs;
use std::io::Write;
use std::path::Path;

const MARKER: &str = "enc:v1:";
const IV_LEN: usize = 12;
const TAG_LEN: usize = 16;

fn key_path() -> std::path::PathBuf {
    crate::frontend_data_dir().join("secret.key")
}

fn load_or_create_key() -> Result<[u8; 32]> {
    load_or_create_key_at(&key_path())
}

fn load_or_create_key_at(path: &Path) -> Result<[u8; 32]> {
    if let Ok(bytes) = fs::read(path) {
        if bytes.len() == 32 {
            let mut key = [0u8; 32];
            key.copy_from_slice(&bytes);
            return Ok(key);
        }
    }
    let mut key = [0u8; 32];
    openssl::rand::rand_bytes(&mut key).context("generate secret key")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    write_key_file(path, &key)?;
    Ok(key)
}

#[cfg(unix)]
fn write_key_file(path: &Path, key: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    file.write_all(key)
        .with_context(|| format!("write {}", path.display()))
}

#[cfg(not(unix))]
fn write_key_file(path: &Path, key: &[u8]) -> Result<()> {
    fs::write(path, key).with_context(|| format!("write {}", path.display()))
}

pub fn is_encrypted(value: &str) -> bool {
    value.starts_with(MARKER)
}

pub fn encrypt(plaintext: &str) -> Result<String> {
    let key = load_or_create_key()?;
    let mut iv = [0u8; IV_LEN];
    openssl::rand::rand_bytes(&mut iv).context("generate nonce")?;
    let mut tag = [0u8; TAG_LEN];
    let ciphertext = encrypt_aead(
        Cipher::aes_256_gcm(),
        &key,
        Some(&iv),
        &[],
        plaintext.as_bytes(),
        &mut tag,
    )
    .context("aes-256-gcm encrypt")?;
    let mut blob = Vec::with_capacity(IV_LEN + TAG_LEN + ciphertext.len());
    blob.extend_from_slice(&iv);
    blob.extend_from_slice(&tag);
    blob.extend_from_slice(&ciphertext);
    Ok(format!("{MARKER}{}", STANDARD.encode(&blob)))
}

pub fn decrypt(stored: &str) -> Option<String> {
    let blob = STANDARD.decode(stored.strip_prefix(MARKER)?).ok()?;
    if blob.len() < IV_LEN + TAG_LEN {
        return None;
    }
    let key = load_or_create_key().ok()?;
    let (iv, rest) = blob.split_at(IV_LEN);
    let (tag, ciphertext) = rest.split_at(TAG_LEN);
    let plaintext =
        decrypt_aead(Cipher::aes_256_gcm(), &key, Some(iv), &[], ciphertext, tag).ok()?;
    String::from_utf8(plaintext).ok()
}

pub fn reveal(stored: &str) -> String {
    if is_encrypted(stored) {
        decrypt(stored).unwrap_or_default()
    } else {
        stored.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypted_marker_is_distinct_from_plaintext() {
        assert!(!is_encrypted("plain"));
        assert!(is_encrypted("enc:v1:anything"));
        assert_eq!(reveal("plain"), "plain");
    }
}
