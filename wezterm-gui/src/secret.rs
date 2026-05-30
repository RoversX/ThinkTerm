//! At-rest encryption for stored secrets (currently SSH passwords).
//!
//! Secrets are encrypted with AES-256-GCM using a per-machine random key kept
//! in ThinkTerm's native data directory as `secret.key` (mode 0600). The encrypted value is
//! stored in `ssh_hosts.json` as `enc:v1:<base64(nonce|tag|ciphertext)>` so the
//! JSON never contains plaintext. This protects against casual disclosure
//! (sync/backups/prying eyes); anyone with both the key file and the ciphertext
//! can still decrypt, which is the documented trade-off for a key-on-disk
//! scheme (vs the macOS Keychain).

use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use openssl::symm::{decrypt_aead, encrypt_aead, Cipher};
use std::fs;
use std::io::Write;
use std::path::PathBuf;

const MARKER: &str = "enc:v1:";
const IV_LEN: usize = 12;
const TAG_LEN: usize = 16;

fn key_path() -> PathBuf {
    crate::native_paths::data_file("secret.key")
}

/// Load the 32-byte key, generating and persisting one (0600) if absent.
fn load_or_create_key() -> Result<[u8; 32]> {
    let path = key_path();
    if let Ok(bytes) = fs::read(&path) {
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
    write_key_file(&path, &key)?;
    Ok(key)
}

#[cfg(unix)]
fn write_key_file(path: &std::path::Path, key: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    file.write_all(key)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_key_file(path: &std::path::Path, key: &[u8]) -> Result<()> {
    fs::write(path, key).with_context(|| format!("write {}", path.display()))
}

/// True if `value` is an encrypted blob produced by [`encrypt`].
pub fn is_encrypted(value: &str) -> bool {
    value.starts_with(MARKER)
}

/// Encrypt `plaintext` into the `enc:v1:...` storage form.
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

/// Decrypt a value produced by [`encrypt`]. Returns `None` if it is not an
/// encrypted blob or cannot be decrypted.
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

/// Decrypt an encrypted value, or pass through a legacy plaintext value
/// (one not produced by [`encrypt`]).
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
    fn round_trip() {
        let secret = "hunter2 · 密码 🔒";
        let enc = encrypt(secret).unwrap();
        assert!(is_encrypted(&enc));
        assert_ne!(enc, secret);
        assert_eq!(decrypt(&enc).as_deref(), Some(secret));
        assert_eq!(reveal(&enc), secret);
        // Legacy plaintext passes through.
        assert_eq!(reveal("plain"), "plain");
    }
}
