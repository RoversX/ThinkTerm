//! Web tokens: the credential a browser presents at the web port.
//!
//! A token is full access as the server's user -- everything the unix
//! socket grants, because everything a mux client can do (spawn a shell,
//! type into it, read every scrollback) is what the browser client does.
//! What the token adds over a TLS client certificate is a lifetime and a
//! revocation: the server keeps only a digest, so a leaked store reveals
//! nothing, and revoking a token drops the connections it admitted on the
//! spot.

use anyhow::Context;
use base64::Engine;
use codec::WebTokenInfo;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The process-wide store. Configured once at server start with where to
/// persist, if anywhere.
pub static WEB_TOKENS: LazyLock<WebTokenStore> = LazyLock::new(WebTokenStore::default);

/// Bytes of entropy in a token; rendered URL-safe base64 without padding.
const TOKEN_BYTES: usize = 32;

#[derive(Default)]
pub struct WebTokenStore {
    /// Shared with every `Admission`, which forgets its connection on drop.
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    tokens: Vec<TokenRecord>,
    live: Vec<LiveConnection>,
    persist: Option<PathBuf>,
    next_connection: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TokenRecord {
    id: String,
    label: String,
    /// sha256 of the token, hex.
    digest: String,
    created_at: u64,
    expires_at: Option<u64>,
    last_used_at: Option<u64>,
}

struct LiveConnection {
    connection: u64,
    token_id: String,
    kill: smol::channel::Sender<()>,
}

/// What `mint` hands back; the token itself is shown once and not kept.
#[derive(Debug)]
pub struct MintedToken {
    pub id: String,
    pub label: String,
    pub token: String,
    pub expires_at: Option<u64>,
}

/// A connection the store has admitted. Dropping it forgets the
/// connection; `revoked` fires if its token is revoked while it lives.
pub struct Admission {
    pub token_id: String,
    pub label: String,
    pub revoked: smol::channel::Receiver<()>,
    connection: u64,
    store: Arc<Mutex<Inner>>,
}

impl std::fmt::Debug for Admission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Admission")
            .field("token_id", &self.token_id)
            .field("label", &self.label)
            .finish()
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        self.store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .live
            .retain(|live| live.connection != self.connection);
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn digest_hex(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// Equal without an early exit, so the time taken says nothing about how
/// many leading bytes matched.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl WebTokenStore {
    /// Say where tokens persist, and load what is there. Memory only when
    /// `persist` is None, in which case a restart forgets every token.
    /// A store that cannot be read is an empty store, said loudly: the
    /// server must start whatever state a cache file is in, and a corrupt
    /// file is moved aside so it can be looked at.
    pub fn configure(&self, persist: Option<PathBuf>) -> anyhow::Result<()> {
        let mut inner = self.lock();
        if let Some(path) = &persist {
            match std::fs::read(path) {
                Ok(bytes) => match serde_json::from_slice::<Vec<TokenRecord>>(&bytes) {
                    Ok(tokens) => inner.tokens = tokens,
                    Err(err) => {
                        let aside = path.with_extension("json.corrupt");
                        log::error!(
                            "web token store {} is unreadable ({err}); starting with no web \
                             tokens and moving it to {}",
                            path.display(),
                            aside.display()
                        );
                        if let Err(err) = std::fs::rename(path, &aside) {
                            log::error!("could not move the corrupt store aside: {err}");
                        }
                    }
                },
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    log::error!(
                        "web token store {} could not be read ({err}); starting with no web tokens",
                        path.display()
                    );
                }
            }
        }
        inner.persist = persist;
        if Self::expire(&mut inner, now_secs()) {
            Self::save(&inner)?;
        }
        Ok(())
    }

    pub fn mint(
        &self,
        label: Option<String>,
        ttl: Option<Duration>,
    ) -> anyhow::Result<MintedToken> {
        let mut raw = [0u8; TOKEN_BYTES];
        getrandom::fill(&mut raw).map_err(|e| anyhow::anyhow!("no entropy: {e}"))?;
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
        let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
        let now = now_secs();
        let expires_at = ttl.map(|ttl| now.saturating_add(ttl.as_secs()));
        let label = label
            .filter(|l| !l.trim().is_empty())
            .unwrap_or_else(|| format!("token-{id}"));
        let record = TokenRecord {
            id: id.clone(),
            label: label.clone(),
            digest: digest_hex(&token),
            created_at: now,
            expires_at,
            last_used_at: None,
        };
        let mut inner = self.lock();
        Self::expire(&mut inner, now);
        inner.tokens.push(record);
        Self::save(&inner)?;
        Ok(MintedToken {
            id,
            label,
            token,
            expires_at,
        })
    }

    /// Check a presented token. On success the connection is registered
    /// so that revoking the token can drop it.
    pub fn verify(&self, presented: &str) -> Option<Admission> {
        let digest = digest_hex(presented);
        let now = now_secs();
        let mut inner = self.lock();
        if Self::expire(&mut inner, now) {
            if let Err(err) = Self::save(&inner) {
                log::warn!("web token store not saved: {err:#}");
            }
        }
        // Compare against every record: the match position must not leak
        // through timing either.
        let mut found: Option<usize> = None;
        for (idx, record) in inner.tokens.iter().enumerate() {
            if constant_time_eq(&record.digest, &digest) {
                found = Some(idx);
            }
        }
        let idx = found?;
        // Recorded in memory; written out with the next mint, revoke or
        // sweep rather than on every connection.
        inner.tokens[idx].last_used_at = Some(now);
        let token_id = inner.tokens[idx].id.clone();
        let label = inner.tokens[idx].label.clone();
        let (kill, revoked) = smol::channel::bounded(1);
        inner.next_connection += 1;
        let connection = inner.next_connection;
        inner.live.push(LiveConnection {
            connection,
            token_id: token_id.clone(),
            kill,
        });
        Some(Admission {
            token_id,
            label,
            revoked,
            connection,
            store: Arc::clone(&self.inner),
        })
    }

    /// Drop expired tokens and cut their connections. The listener runs
    /// this on a timer: a token's lifetime ends when it says, not when
    /// someone next happens to mint or list.
    pub fn sweep(&self) {
        let mut inner = self.lock();
        if Self::expire(&mut inner, now_secs()) {
            if let Err(err) = Self::save(&inner) {
                log::warn!("web token store not saved: {err:#}");
            }
        }
    }

    pub fn list(&self) -> Vec<WebTokenInfo> {
        let mut inner = self.lock();
        if Self::expire(&mut inner, now_secs()) {
            if let Err(err) = Self::save(&inner) {
                log::warn!("web token store not saved: {err:#}");
            }
        }
        inner
            .tokens
            .iter()
            .map(|record| WebTokenInfo {
                id: record.id.clone(),
                label: record.label.clone(),
                created_at: record.created_at,
                expires_at: record.expires_at,
                last_used_at: record.last_used_at,
                live_connections: inner
                    .live
                    .iter()
                    .filter(|live| live.token_id == record.id)
                    .count() as u32,
            })
            .collect()
    }

    /// Revoke one token, or all of them. Returns how many were revoked;
    /// their live connections are told to drop.
    pub fn revoke(&self, id: Option<&str>) -> u32 {
        let mut inner = self.lock();
        let before = inner.tokens.len();
        let gone: Vec<String> = match id {
            Some(id) => {
                inner.tokens.retain(|record| record.id != id);
                vec![id.to_string()]
            }
            None => {
                let ids = inner.tokens.iter().map(|r| r.id.clone()).collect();
                inner.tokens.clear();
                ids
            }
        };
        let revoked = (before - inner.tokens.len()) as u32;
        for live in inner.live.iter().filter(|live| gone.contains(&live.token_id)) {
            let _ = live.kill.try_send(());
        }
        if let Err(err) = Self::save(&inner) {
            log::warn!("web token store not saved: {err:#}");
        }
        revoked
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Drop tokens past their expiry and cut their connections. True when
    /// something was dropped, so the caller knows to save.
    fn expire(inner: &mut Inner, now: u64) -> bool {
        let expired: Vec<String> = inner
            .tokens
            .iter()
            .filter(|r| r.expires_at.is_some_and(|t| t <= now))
            .map(|r| r.id.clone())
            .collect();
        if expired.is_empty() {
            return false;
        }
        inner.tokens.retain(|r| !expired.contains(&r.id));
        for live in inner.live.iter().filter(|l| expired.contains(&l.token_id)) {
            let _ = live.kill.try_send(());
        }
        true
    }

    fn save(inner: &Inner) -> anyhow::Result<()> {
        let Some(path) = &inner.persist else {
            return Ok(());
        };
        write_private(path, &serde_json::to_vec_pretty(&inner.tokens)?)
    }
}

/// Write `bytes` to `path` readable by the owner only, replacing whatever
/// was there in one step. A uniquely named temporary file (created 0600,
/// never following a planted symlink), synced before the rename so a
/// crash leaves either the old file or the new one, never a torn one.
fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".web-tokens.")
        .tempfile_in(dir)
        .with_context(|| format!("creating a temporary file in {}", dir.display()))?;
    std::io::Write::write_all(&mut tmp, bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)
        .with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> WebTokenStore {
        WebTokenStore::default()
    }

    #[test]
    fn a_minted_token_verifies_and_a_stranger_does_not() {
        let store = fresh();
        let minted = store.mint(Some("laptop".into()), None).unwrap();
        let admitted = store.verify(&minted.token).expect("the real token");
        assert_eq!(admitted.token_id, minted.id);
        assert_eq!(admitted.label, "laptop");
        assert!(store.verify("not-a-token").is_none());
        let mut wrong = minted.token.clone();
        wrong.replace_range(0..1, if minted.token.starts_with('A') { "B" } else { "A" });
        assert!(store.verify(&wrong).is_none());
        assert_eq!(store.list()[0].live_connections, 1);
        drop(admitted);
        assert_eq!(store.list()[0].live_connections, 0);
    }

    #[test]
    fn revoking_drops_the_live_connection_and_forgets_the_token() {
        let store = fresh();
        let minted = store.mint(None, None).unwrap();
        let admitted = store.verify(&minted.token).unwrap();
        assert_eq!(store.revoke(Some(&minted.id)), 1);
        assert!(admitted.revoked.try_recv().is_ok(), "the kill switch fired");
        assert!(store.verify(&minted.token).is_none());
        assert_eq!(store.revoke(Some(&minted.id)), 0);
    }

    #[test]
    fn expired_tokens_are_gone_and_nothing_persists_but_digests() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("web-tokens.json");
        let store = fresh();
        store.configure(Some(path.clone())).unwrap();
        let keep = store.mint(Some("keep".into()), None).unwrap();
        let brief = store
            .mint(Some("brief".into()), Some(Duration::from_secs(0)))
            .unwrap();
        assert!(store.verify(&brief.token).is_none(), "already expired");
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("keep"));
        assert!(!on_disk.contains(&keep.token), "the token itself is never written");
        assert!(!on_disk.contains("brief"), "expired tokens are dropped from the file");

        let reloaded = fresh();
        reloaded.configure(Some(path)).unwrap();
        assert!(reloaded.verify(&keep.token).is_some());
    }

    #[test]
    fn revoke_all_clears_everything() {
        let store = fresh();
        store.mint(None, None).unwrap();
        store.mint(None, None).unwrap();
        assert_eq!(store.revoke(None), 2);
        assert!(store.list().is_empty());
    }

    /// A torn or garbage store file must not stop the server: it is moved
    /// aside and the store starts empty.
    #[test]
    fn a_corrupt_store_starts_empty_and_is_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("web-tokens.json");
        std::fs::write(&path, b"[{\"id\": \"trunc").unwrap();
        let store = WebTokenStore::default();
        store.configure(Some(path.clone())).expect("a corrupt store is not an error");
        assert!(store.list().is_empty());
        assert!(!path.exists(), "the corrupt file was left in place");
        assert!(dir.path().join("web-tokens.json.corrupt").exists());
        // And the store is usable: a mint persists a fresh file.
        store.mint(Some("after".into()), None).unwrap();
        assert!(path.exists());
    }
}
