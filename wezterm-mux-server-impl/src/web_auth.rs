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

struct Inner {
    tokens: Vec<TokenRecord>,
    live: Vec<LiveConnection>,
    persist: Option<PathBuf>,
    next_connection: u64,
    /// Whether `configure` has already run. It used to run once, at
    /// startup; a runtime listener start calls it again, and a second run
    /// that reloaded the file would throw away every token minted since.
    configured: bool,
    /// Set false while the listener is off, so a connection whose handshake
    /// was already in flight when the port closed cannot still be let in.
    admitting: bool,
    /// A use was recorded that has not been written out. Kept so the sweep
    /// can persist `last_used_at` and `last_device` without a file write
    /// per connection.
    used_since_save: bool,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            tokens: Vec::new(),
            live: Vec::new(),
            persist: None,
            next_connection: 0,
            configured: false,
            // A server with a configured listener is admitting from the
            // moment it starts; nothing has to turn this on first.
            admitting: true,
            used_since_save: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TokenRecord {
    id: String,
    /// What a person called it, if a person called it anything. `None` is
    /// not a failure to name: a link minted from the settings window has no
    /// name to give, and inventing one ("token-a1b2c3") only made the list
    /// look like it knew something it did not.
    label: Option<String>,
    /// sha256 of the token, hex.
    digest: String,
    created_at: u64,
    expires_at: Option<u64>,
    last_used_at: Option<u64>,
    /// What the browser said it was, the last time this link was used.
    last_device: Option<String>,
}

struct LiveConnection {
    connection: u64,
    token_id: String,
    kill: smol::channel::Sender<()>,
}

/// A short description of the browser that connected, from its User-Agent.
///
/// This is what the browser *says* it is. It is a description, not an
/// identity: a User-Agent is set by the client and can say anything. It is
/// here so a person can tell their own two devices apart in a list, which
/// is a different job from deciding who gets in -- the token does that.
///
/// `None` when nothing recognisable is there, so a caller can say "unknown"
/// rather than print somebody's raw header back at them.
pub fn describe_user_agent(ua: &str) -> Option<String> {
    // Order matters twice over. Platform: iPad before Macintosh, because
    // iPadOS Safari claims both. Browser: the Chromium family before
    // Safari, because every one of them still carries "Safari" at the end.
    // An iPad with "Request Desktop Website" on (the default for Safari on
    // a large iPad) sends a Macintosh User-Agent with no "iPad" in it at
    // all, and is reported here as a Mac. Nothing in the header
    // distinguishes them; this is the limit of asking the client what it
    // is, and the reason this is a description and not an identity.
    let platform = [
        ("iPhone", "iPhone"),
        ("iPad", "iPad"),
        ("Android", "Android"),
        ("CrOS", "ChromeOS"),
        ("Macintosh", "Mac"),
        ("Mac OS X", "Mac"),
        ("Windows", "Windows"),
        ("Linux", "Linux"),
    ]
    .iter()
    .find(|(needle, _)| ua.contains(needle))
    .map(|(_, name)| *name);
    // The iOS variants come first for the same reason: Chrome on iOS is
    // "CriOS", not "Chrome", and every one of them still ends in "Safari".
    let browser = [
        ("CriOS/", "Chrome"),
        ("FxiOS/", "Firefox"),
        ("EdgiOS/", "Edge"),
        ("Edg/", "Edge"),
        ("OPR/", "Opera"),
        ("Firefox/", "Firefox"),
        ("Chrome/", "Chrome"),
        ("Safari/", "Safari"),
    ]
    .iter()
    .find(|(needle, _)| ua.contains(needle))
    .map(|(_, name)| *name);
    match (platform, browser) {
        (Some(platform), Some(browser)) => Some(format!("{platform} · {browser}")),
        (Some(one), None) | (None, Some(one)) => Some(one.to_string()),
        (None, None) => None,
    }
}

/// What `mint` hands back; the token itself is shown once and not kept.
#[derive(Debug)]
pub struct MintedToken {
    pub id: String,
    pub label: Option<String>,
    pub token: String,
    pub expires_at: Option<u64>,
}

/// A connection the store has admitted. Dropping it forgets the
/// connection; `revoked` fires if its token is revoked while it lives.
pub struct Admission {
    pub token_id: String,
    pub label: Option<String>,
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
    ///
    /// **The first call wins.** This is called once at startup and again by
    /// every runtime listener start, and the later calls must change
    /// nothing: reloading the file would replace tokens minted since with
    /// the stale set on disk, and a start at an address that is not in
    /// `web_servers` arrives with `persist: None`, which would turn a
    /// persistent store into a memory-only one -- so a revoke would report
    /// success, never reach the file, and the token would be admitted again
    /// after a restart.
    pub fn configure(&self, persist: Option<PathBuf>) -> anyhow::Result<()> {
        let mut inner = self.lock();
        if inner.configured {
            return Ok(());
        }
        inner.configured = true;
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
        let label = label.filter(|l| !l.trim().is_empty());
        let record = TokenRecord {
            id: id.clone(),
            label: label.clone(),
            digest: digest_hex(&token),
            created_at: now,
            expires_at,
            last_used_at: None,
            last_device: None,
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
    pub fn verify(&self, presented: &str, device: Option<String>) -> Option<Admission> {
        let digest = digest_hex(presented);
        let now = now_secs();
        let mut inner = self.lock();
        // The listener is off. This connection was accepted before the port
        // closed and finished its handshake afterwards; letting it in would
        // hand out a shell the switch says is not on offer.
        if !inner.admitting {
            return None;
        }
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
        // Recorded in memory; written out by the next sweep, mint or
        // revoke rather than on every connection.
        inner.tokens[idx].last_used_at = Some(now);
        inner.used_since_save = true;
        // Only overwritten by a connection that said something: a client
        // with no User-Agent should not erase what the last one told us.
        if device.is_some() {
            inner.tokens[idx].last_device = device.clone();
        }
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
    /// Let connections in again. Called when a listener starts, so the
    /// refusal `disconnect_all` put in place lasts exactly as long as the
    /// listener is down.
    pub fn resume_admitting(&self) {
        self.lock().admitting = true;
    }

    pub fn sweep(&self) {
        let mut inner = self.lock();
        // Expiry is not the only thing worth writing: `verify` records
        // `last_used_at` and `last_device` in memory, and the settings list
        // names a link after the browser that used it. Without this they
        // survived only until the next mint or revoke -- and a store whose
        // tokens have no expiry never had one of those forced on it.
        let expired = Self::expire(&mut inner, now_secs());
        if expired || inner.used_since_save {
            inner.used_since_save = false;
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
                last_device: record.last_device.clone(),
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

    /// Cut every live browser connection, leaving the tokens alone.
    ///
    /// What "stop accepting browser clients" has to mean: closing the port
    /// only stops the next browser, and someone already inside would keep
    /// a shell for as long as they held the socket. The links stay valid,
    /// so turning the listener back on does not make everyone re-mint --
    /// `revoke` is the one that ends links.
    pub fn disconnect_all(&self) -> u32 {
        let mut inner = self.lock();
        // Closed first, and in the same lock: a connection accepted before
        // the port went away can still be in its handshake, and `verify`
        // has to refuse it after this returns. Cutting only what is already
        // live would let that one through and leave a browser inside a
        // server whose switch reads off.
        inner.admitting = false;
        let mut cut = 0;
        for live in inner.live.iter() {
            if live.kill.try_send(()).is_ok() {
                cut += 1;
            }
        }
        cut
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

fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    crate::private_file::replace(path, ".web-tokens.", bytes)
}

#[cfg(test)]
mod tests {
    use super::describe_user_agent as describe;

    #[test]
    fn a_browser_is_described_by_platform_and_engine() {
        assert_eq!(
            describe("Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) \
                      AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1")
                .as_deref(),
            Some("iPhone · Safari")
        );
        assert_eq!(
            describe("Mozilla/5.0 (iPad; CPU OS 17_5 like Mac OS X) AppleWebKit/605.1.15 \
                      (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1")
                .as_deref(),
            Some("iPad · Safari")
        );
        assert_eq!(
            describe("Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 \
                      (KHTML, like Gecko) Chrome/125.0.0.0 Mobile Safari/537.36")
                .as_deref(),
            Some("Android · Chrome")
        );
    }

    #[test]
    fn the_chromium_family_is_not_reported_as_safari() {
        // Every one of these ends in "Safari/537.36", and Edge and Opera
        // also carry "Chrome/". Matching in the wrong order calls all of
        // them Safari, which is exactly the kind of wrong that looks right.
        let base = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                    (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36";
        assert_eq!(describe(base).as_deref(), Some("Mac · Chrome"));
        assert_eq!(
            describe(&format!("{base} Edg/125.0.0.0")).as_deref(),
            Some("Mac · Edge")
        );
        assert_eq!(
            describe(&format!("{base} OPR/111.0.0.0")).as_deref(),
            Some("Mac · Opera")
        );
        // Chrome on iOS is CriOS, and still ends in Safari.
        assert_eq!(
            describe("Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) \
                      AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/125.0.0.0 Mobile/15E148 Safari/604.1")
                .as_deref(),
            Some("iPhone · Chrome")
        );
    }

    #[test]
    fn half_an_answer_is_still_worth_showing_and_none_is_not() {
        assert_eq!(describe("Firefox/126.0").as_deref(), Some("Firefox"));
        assert_eq!(describe("Mozilla/5.0 (Windows NT 10.0)").as_deref(), Some("Windows"));
        // Nothing recognisable: the caller says "unknown" rather than
        // printing a stranger's raw header into a settings window.
        assert_eq!(describe(""), None);
        assert_eq!(describe("curl/8.6.0"), None);
    }

    use super::*;

    fn fresh() -> WebTokenStore {
        WebTokenStore::default()
    }

    #[test]
    fn a_minted_token_verifies_and_a_stranger_does_not() {
        let store = fresh();
        let minted = store.mint(Some("laptop".into()), None).unwrap();
        let admitted = store.verify(&minted.token, None).expect("the real token");
        assert_eq!(admitted.token_id, minted.id);
        assert_eq!(admitted.label.as_deref(), Some("laptop"));
        assert!(store.verify("not-a-token", None).is_none());
        let mut wrong = minted.token.clone();
        wrong.replace_range(0..1, if minted.token.starts_with('A') { "B" } else { "A" });
        assert!(store.verify(&wrong, None).is_none());
        assert_eq!(store.list()[0].live_connections, 1);
        drop(admitted);
        assert_eq!(store.list()[0].live_connections, 0);
    }

    #[test]
    fn revoking_drops_the_live_connection_and_forgets_the_token() {
        let store = fresh();
        let minted = store.mint(None, None).unwrap();
        let admitted = store.verify(&minted.token, None).unwrap();
        assert_eq!(store.revoke(Some(&minted.id)), 1);
        assert!(admitted.revoked.try_recv().is_ok(), "the kill switch fired");
        assert!(store.verify(&minted.token, None).is_none());
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
        assert!(store.verify(&brief.token, None).is_none(), "already expired");
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("keep"));
        assert!(!on_disk.contains(&keep.token), "the token itself is never written");
        assert!(!on_disk.contains("brief"), "expired tokens are dropped from the file");

        let reloaded = fresh();
        reloaded.configure(Some(path)).unwrap();
        assert!(reloaded.verify(&keep.token, None).is_some());
    }

    #[test]
    fn a_second_configure_changes_nothing() {
        // The runtime listener switch calls configure again on every "on".
        // Reloading there would replace tokens minted since with the stale
        // set on disk, and a start at an address that is not in the
        // configuration arrives with `persist: None` -- which used to turn
        // a persistent store into a memory-only one, so a later revoke
        // reported success, never reached the file, and the token was
        // admitted again after a restart.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("web-tokens.json");
        let store = fresh();
        store.configure(Some(path.clone())).unwrap();
        let first = store.mint(Some("first".into()), None).unwrap();

        // A start at an unconfigured address: persist must survive it.
        store.configure(None).unwrap();
        assert!(store.verify(&first.token, None).is_some(), "the token is still here");
        assert_eq!(store.revoke(Some(&first.id)), 1);
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(!on_disk.contains("first"), "the revoke reached the file");

        // And a start back at the configured address must not reload the
        // file over what is in memory.
        let second = store.mint(Some("second".into()), None).unwrap();
        store.configure(Some(path)).unwrap();
        assert!(store.verify(&second.token, None).is_some(), "a live token was not dropped");
    }

    #[test]
    fn turning_the_listener_off_refuses_a_handshake_that_was_already_in_flight() {
        // Cutting only the connections already admitted let a socket that
        // was accepted before the port closed finish its handshake
        // afterwards and become a full session -- a browser inside a server
        // whose switch reads off.
        let store = fresh();
        let token = store.mint(None, None).unwrap();
        let admitted = store.verify(&token.token, None).unwrap();
        assert_eq!(store.disconnect_all(), 1, "the live one is cut");
        drop(admitted);
        assert!(
            store.verify(&token.token, None).is_none(),
            "no admissions while the listener is down"
        );
        store.resume_admitting();
        assert!(
            store.verify(&token.token, None).is_some(),
            "and they resume when it comes back up"
        );
    }

    #[test]
    fn a_sweep_writes_out_a_use_even_when_nothing_expired() {
        // The settings list names a link after the browser that used it, so
        // losing `last_device` on every restart made the list forget which
        // device each link belonged to. Nothing expires here, which is the
        // case the old sweep never saved in.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("web-tokens.json");
        let store = fresh();
        store.configure(Some(path.clone())).unwrap();
        let token = store.mint(Some("phone".into()), None).unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().contains("iPad"));

        drop(store.verify(&token.token, Some("iPad · Safari".into())).unwrap());
        store.sweep();
        assert!(
            std::fs::read_to_string(&path).unwrap().contains("iPad · Safari"),
            "the device that used the link survives a restart"
        );
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
