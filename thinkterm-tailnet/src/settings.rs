//! `tailnet.json`: whether this machine joins, whether the tailnet may SSH
//! into it or reach other ports of it, and the secrets that make it the same
//! node every time. It sits beside the SSH host catalog and its encryption
//! key, and the auth key and the node's keys are encrypted with that key the
//! same way host passwords are.
//!
//! On Windows that is in the local half of the profile
//! (`%LOCALAPPDATA%\ThinkTerm\ssh`), which a roaming profile does not copy
//! to the user's other machines: two machines on one node's keys would take
//! the node from each other. Windows has no mode to give the files there,
//! the node's socket among them: they take the permissions of the profile
//! they are in, which let in this user, SYSTEM and the Administrators
//! group, and nobody else, as ThinkTerm's other private files on Windows
//! do.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// What `tailnet.json` says, with the secrets decrypted. Its Debug leaves
/// the secrets out.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Settings {
    /// Whether the machine joins the tailnet: its mux server keeps a node
    /// running while it does.
    pub enabled: bool,
    /// Whether the node takes SSH connections from the tailnet to this
    /// machine's own sshd: what `thinkterm tailnet up` turns on, for a
    /// machine nobody sits at. The app's switch never does.
    pub serve_ssh: bool,
    /// Other TCP ports the node takes connections on from the tailnet, each
    /// handed to the same port on this machine -- a VNC server's, say: what
    /// `thinkterm tailnet serve add` opens, and the app never does. They
    /// stay open only while the tailnet stays on, on the same control
    /// server (see [`Settings::close_ports_unless_same_tailnet`]).
    pub serve_ports: Vec<u16>,
    /// The node, as its tailnet knew it, `serve_ports` were opened to (see
    /// `node::NodeConfig::ports_opened_to`): the same keys let into another
    /// tailnet take in none of them. None without ports.
    pub serve_ports_to: Option<String>,
    /// The control server; None is Tailscale's own.
    pub control_url: Option<String>,
    /// The name to ask the tailnet for; None is one made from the machine's
    /// key (see [`crate::automatic_name`]).
    pub hostname: Option<String>,
    pub auth_key: Option<String>,
    /// The node's identity as the library serializes it. Made on the first
    /// start and kept, so that the machine stays the same node.
    pub node_keys: Option<String>,
    /// Counts the saves. A process acting on the file says which one it
    /// has seen ([`crate::Report::revision`]), so that `thinkterm tailnet
    /// up` can tell the answer to its change from one written before it.
    pub revision: u64,
}

impl Settings {
    /// Blank fields mean the default, so they are kept as absent: a field
    /// emptied in Settings or `--control-url ""` goes back to Tailscale's
    /// own server, the automatic name, or no auth key.
    pub fn normalize(&mut self) {
        for field in [
            &mut self.control_url,
            &mut self.hostname,
            &mut self.auth_key,
        ] {
            *field = field
                .take()
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty());
        }
        self.serve_ports = served_ports(&self.serve_ports);
        if self.serve_ports.is_empty() {
            self.serve_ports_to = None;
        }
    }

    /// The ports the node takes in: SSH's while `serve_ssh` says so, and
    /// the ones `serve_ports` opens. Sorted, each once.
    pub fn forwarded_ports(&self) -> Vec<u16> {
        let mut ports = served_ports(&self.serve_ports);
        if self.serve_ssh {
            ports.push(SSH_PORT);
            ports.sort_unstable();
        }
        ports
    }

    /// Closes the ports `serve_ports` opened unless the tailnet they were
    /// opened to goes on as it was `before` this change: on before it and
    /// after it, on the same control server, as the same node. Turning it
    /// off -- `down`, Log out, the app's switch -- closes them, and turning
    /// it on again, pointing it at another server or joining as a new node
    /// -- whose keys no longer decrypted, say -- opens none: each is opened
    /// to the tailnet it was opened to, and no other.
    pub(crate) fn close_ports_unless_same_tailnet(&mut self, before: &Settings) {
        let same = before.enabled
            && self.enabled
            && crate::same_control_server(&before.control_url, &self.control_url)
            && before.node_keys == self.node_keys;
        if !same {
            self.serve_ports.clear();
            self.serve_ports_to = None;
        }
    }

    pub fn summary(&self) -> Summary {
        Summary {
            enabled: self.enabled,
            serve_ssh: self.serve_ssh,
            serve_ports: self.serve_ports.clone(),
            serve_ports_to: self.serve_ports_to.clone(),
            control_url: self.control_url.clone(),
            hostname: self.hostname.clone(),
            has_auth_key: self.auth_key.is_some(),
            has_node_keys: self.node_keys.is_some(),
            revision: self.revision,
        }
    }
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.summary().fmt(f)
    }
}

/// SSH's port, which `serve_ssh` alone opens.
pub const SSH_PORT: u16 = 22;

/// The most ports `serve_ports` opens: each costs the node a listener on
/// each of its addresses, with buffers of their own, before anything
/// connects.
pub const MAX_SERVED_PORTS: usize = 16;

/// `ports` as `serve_ports` keeps them: sorted, each once, no more than
/// MAX_SERVED_PORTS, and without 0 -- no port -- or SSH's, which
/// `serve_ssh` opens. A file edited by hand opens no more than that either.
pub fn served_ports(ports: &[u16]) -> Vec<u16> {
    let mut ports: Vec<u16> = ports
        .iter()
        .copied()
        .filter(|&port| port != 0 && port != SSH_PORT)
        .collect();
    ports.sort_unstable();
    ports.dedup();
    ports.truncate(MAX_SERVED_PORTS);
    ports
}

/// The settings without their secrets, read without decrypting them: what
/// a page shows, and all a process needs to know whether it holds the node.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub enabled: bool,
    pub serve_ssh: bool,
    pub serve_ports: Vec<u16>,
    pub serve_ports_to: Option<String>,
    pub control_url: Option<String>,
    pub hostname: Option<String>,
    pub has_auth_key: bool,
    pub has_node_keys: bool,
    pub revision: u64,
}

/// The file's own shape: the secrets as `secret::encrypt` left them.
#[derive(Default, Serialize, Deserialize)]
struct Stored {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    serve_ssh: bool,
    #[serde(
        default,
        deserialize_with = "ports_or_none",
        skip_serializing_if = "Vec::is_empty"
    )]
    serve_ports: Vec<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    serve_ports_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    control_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auth_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    node_keys: Option<String>,
    #[serde(default)]
    revision: u64,
}

/// The ports the file names, or none where it names them wrong -- null, a
/// word, a number no port has: the rest of the file is read all the same,
/// for one that cannot be read cannot be turned off either.
fn ports_or_none<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<u16>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

pub(crate) fn path() -> Result<PathBuf> {
    Ok(thinkterm_core::credential_data_dir()?.join("tailnet.json"))
}

/// Where the process holding the node listens for the others (see
/// crate::proxy): beside the settings, which only this user can open.
#[cfg(any(unix, windows))]
pub(crate) fn socket_path() -> Result<PathBuf> {
    Ok(path()?.with_file_name("tailnet.sock"))
}

/// Refuses root a change to the settings it would leave behind for the
/// wrong user: run with sudo, which on macOS keeps the user's HOME -- root
/// would leave the user files they cannot change or remove -- and elsewhere
/// writes root's own settings, which the user's mux server never reads.
#[cfg(unix)]
pub(crate) fn check_writer() -> Result<()> {
    if !nix::unistd::geteuid().is_root() {
        return Ok(());
    }
    if std::env::var_os("SUDO_USER").is_some() {
        anyhow::bail!(
            "not changing the tailnet settings as root: run it without sudo, as the user \
             whose mux server holds the node"
        );
    }
    check_owner(&path()?, 0)
}

#[cfg(not(unix))]
pub(crate) fn check_writer() -> Result<()> {
    Ok(())
}

/// Whether `uid` may write the settings at `path`: the nearest folder of it
/// that exists has to be its own.
#[cfg(unix)]
fn check_owner(path: &Path, uid: u32) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let owner = path
        .ancestors()
        .skip(1)
        .find_map(|dir| std::fs::metadata(dir).ok().map(|meta| (dir, meta.uid())));
    match owner {
        Some((dir, owner)) if owner != uid => anyhow::bail!(
            "not changing the tailnet settings in {} as root: they belong to another user",
            dir.display()
        ),
        _ => Ok(()),
    }
}

/// Held while one process reads, changes and writes the settings back, so
/// that the app, the server and `thinkterm tailnet` do not undo each other's
/// changes. Released when dropped.
pub(crate) fn lock_for_update() -> Result<File> {
    let file = open_lock("tailnet-settings.lock")?;
    file.lock().context("locking the tailnet settings")?;
    Ok(file)
}

/// Held for as long as a process holds the machine's node: two processes on
/// one node's keys would take the node from each other -- a debug build's
/// `thinkterm-tailnet` beside the release one's. None while another process holds
/// it. Released when dropped, or when the process ends.
#[cfg(all(
    feature = "node",
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
pub(crate) fn try_hold_node() -> Result<Option<File>> {
    let file = open_lock("tailnet-node.lock")?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(err)) => {
            Err(err).context("locking this machine's tailnet node")
        }
    }
}

/// Creates `dir`, and the folders it is in, open to this user alone where
/// they are new: the files in it are this user's alone, and so are their
/// names.
pub(crate) fn create_private_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    let created = {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    };
    #[cfg(not(unix))]
    let created = std::fs::create_dir_all(dir);
    created.with_context(|| format!("creating {}", dir.display()))
}

fn open_lock(name: &str) -> Result<File> {
    let path = path()?.with_file_name(name);
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))
}

/// What tells one version of the file from the next: a save puts a new
/// file in place, so its inode tells even where the clock is too coarse to.
/// Windows keeps a file's times to its clock's tick, which two saves of
/// the same length can fall within, and Rust has no stable way to its file
/// ids; the file is small, so what it says tells instead.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stamp {
    modified: Option<std::time::SystemTime>,
    len: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    digest: u64,
}

/// None while there is no file.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows", test))]
pub(crate) fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(Stamp {
        modified: meta.modified().ok(),
        len: meta.len(),
        #[cfg(unix)]
        inode: std::os::unix::fs::MetadataExt::ino(&meta),
        // A read that fails tells as a change, and the next one as another:
        // the settings are read again either way.
        #[cfg(windows)]
        digest: std::fs::read(path).map_or(0, |bytes| {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            bytes.hash(&mut hasher);
            hasher.finish()
        }),
    })
}

/// The settings, or the defaults (off) when there is no file yet.
pub fn load() -> Result<Settings> {
    load_from(&path()?, thinkterm_core::secret::decrypt)
}

/// The settings without their secrets. A secret that is there counts even
/// when it no longer decrypts.
pub fn load_summary() -> Result<Summary> {
    Ok(load_from(&path()?, |_| Some(String::new()))?.summary())
}

pub(crate) fn save(settings: &Settings) -> Result<()> {
    save_to(&path()?, settings, thinkterm_core::secret::encrypt)
}

/// A secret that no longer decrypts -- the key beside it was replaced --
/// reads as absent: the machine signs in again rather than not at all.
pub(crate) fn load_from(path: &Path, decrypt: impl Fn(&str) -> Option<String>) -> Result<Settings> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Settings::default()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    let stored: Stored =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Settings {
        enabled: stored.enabled,
        serve_ssh: stored.serve_ssh,
        serve_ports: served_ports(&stored.serve_ports),
        serve_ports_to: stored.serve_ports_to,
        control_url: stored.control_url,
        hostname: stored.hostname,
        auth_key: stored.auth_key.as_deref().and_then(&decrypt),
        node_keys: stored.node_keys.as_deref().and_then(&decrypt),
        revision: stored.revision,
    })
}

pub(crate) fn save_to(
    path: &Path,
    settings: &Settings,
    encrypt: impl Fn(&str) -> Result<String>,
) -> Result<()> {
    let stored = Stored {
        enabled: settings.enabled,
        serve_ssh: settings.serve_ssh,
        serve_ports: settings.serve_ports.clone(),
        serve_ports_to: settings.serve_ports_to.clone(),
        control_url: settings.control_url.clone(),
        hostname: settings.hostname.clone(),
        auth_key: settings.auth_key.as_deref().map(&encrypt).transpose()?,
        node_keys: settings.node_keys.as_deref().map(&encrypt).transpose()?,
        revision: settings.revision,
    };
    let mut json = serde_json::to_string_pretty(&stored)?;
    json.push('\n');
    write_private(path, json.as_bytes(), true)
}

/// Written whole through a temporary file, readable by this user alone. The
/// temporary file is this write's own: two processes writing at once each
/// put a whole file in place, rather than one file made of both. `durable`
/// waits for the disk too, which a file that is only ever this moment's
/// news does not need.
pub(crate) fn write_private(path: &Path, bytes: &[u8], durable: bool) -> Result<()> {
    static WRITES: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .with_context(|| format!("{} has no folder", path.display()))?;
    create_private_dir(parent)?;
    let tmp = path.with_extension(format!(
        "json.{}.{}.tmp",
        std::process::id(),
        WRITES.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    let written = file
        .write_all(bytes)
        .and_then(|()| if durable { file.sync_all() } else { Ok(()) })
        .with_context(|| format!("writing {}", tmp.display()));
    drop(file);
    let result = written.and_then(|()| {
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
    });
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn settings_are_changed_only_by_whoever_owns_their_folder() {
        let dir = tempfile::tempdir().unwrap();
        // Not made yet: the nearest folder that is says whose they are.
        let path = dir.path().join("ThinkTerm").join("tailnet.json");
        let me = nix::unistd::getuid().as_raw();
        assert!(check_owner(&path, me).is_ok());
        assert!(check_owner(&path, me.wrapping_add(1)).is_err());
    }

    // Stand-ins for `secret`, which would read the real key under the
    // user's data folder.
    fn seal(plain: &str) -> Result<String> {
        Ok(format!("sealed:{plain}"))
    }

    fn unseal(stored: &str) -> Option<String> {
        stored.strip_prefix("sealed:").map(str::to_string)
    }

    #[test]
    fn missing_file_reads_as_off() {
        let dir = tempfile::tempdir().unwrap();
        let settings = load_from(&dir.path().join("tailnet.json"), unseal).unwrap();
        assert_eq!(settings, Settings::default());
        assert!(!settings.enabled && !settings.serve_ssh);
    }

    #[test]
    fn a_file_from_before_ports_opens_none_and_one_edited_by_hand_no_more_than_it_may() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tailnet.json");
        std::fs::write(&path, r#"{"enabled":true,"serve_ssh":true}"#).unwrap();
        let settings = load_from(&path, unseal).unwrap();
        assert!(settings.serve_ports.is_empty());
        assert_eq!(settings.forwarded_ports(), [22]);

        let many: Vec<String> = (6000..6040).map(|port| port.to_string()).collect();
        std::fs::write(
            &path,
            format!(
                r#"{{"enabled":true,"serve_ports":[5902,0,22,5900,5902,{}]}}"#,
                many.join(",")
            ),
        )
        .unwrap();
        let settings = load_from(&path, unseal).unwrap();
        assert_eq!(settings.serve_ports.len(), MAX_SERVED_PORTS);
        assert_eq!(settings.serve_ports[..3], [5900, 5902, 6000]);
        // SSH's port stays closed without serve_ssh, whatever the file says.
        assert!(!settings.forwarded_ports().contains(&SSH_PORT));
    }

    #[test]
    fn ssh_and_the_ports_opened_are_taken_in_sorted_and_once() {
        let settings = Settings {
            enabled: true,
            serve_ssh: true,
            serve_ports: vec![5902, 5900, 5902],
            ..Settings::default()
        };
        assert_eq!(settings.forwarded_ports(), [22, 5900, 5902]);
        let settings = Settings {
            serve_ssh: false,
            ..settings
        };
        assert_eq!(settings.forwarded_ports(), [5900, 5902]);
        assert!(Settings::default().forwarded_ports().is_empty());
    }

    #[test]
    fn ports_stay_open_only_to_the_tailnet_they_were_opened_to() {
        let before = Settings {
            enabled: true,
            serve_ports: vec![5900],
            control_url: Some("https://headscale.example.com/".into()),
            node_keys: Some("keys-a".into()),
            ..Settings::default()
        };
        let left = |after: Settings| {
            let mut after = after;
            after.close_ports_unless_same_tailnet(&before);
            after.serve_ports
        };
        // On before and after, on the same server as the same node: a new
        // name, say, or the server written another way.
        assert_eq!(
            left(Settings {
                hostname: Some("server-b".into()),
                ..before.clone()
            }),
            [5900]
        );
        assert_eq!(
            left(Settings {
                control_url: Some("https://headscale.example.com".into()),
                ..before.clone()
            }),
            [5900]
        );
        // Turned off: down, Log out, the app's switch.
        assert!(left(Settings {
            enabled: false,
            ..before.clone()
        })
        .is_empty());
        let mut off = Settings {
            enabled: false,
            serve_ports_to: Some("example.ts.net/n1".into()),
            ..before.clone()
        };
        off.close_ports_unless_same_tailnet(&before);
        assert_eq!(off.serve_ports_to, None);
        // Pointed at another control server, Tailscale's own included.
        assert!(left(Settings {
            control_url: None,
            ..before.clone()
        })
        .is_empty());
        // Tailscale's own, left out or named, is the one server.
        let on_tailscale = Settings {
            control_url: None,
            ..before.clone()
        };
        let mut named = Settings {
            control_url: Some("https://controlplane.tailscale.com".into()),
            ..on_tailscale.clone()
        };
        named.close_ports_unless_same_tailnet(&on_tailscale);
        assert_eq!(named.serve_ports, [5900]);
        // Joining as another node: keys made anew.
        assert!(left(Settings {
            node_keys: Some("keys-b".into()),
            ..before.clone()
        })
        .is_empty());
        // Turned on again, from a file that still named ports.
        let mut on_again = before.clone();
        on_again.close_ports_unless_same_tailnet(&Settings {
            enabled: false,
            ..before.clone()
        });
        assert!(on_again.serve_ports.is_empty());
    }

    #[test]
    fn ports_written_wrong_open_none_and_leave_the_file_readable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tailnet.json");
        for ports in ["null", "\"5900\"", "[5900,\"vnc\"]", "[70000]", "{}"] {
            std::fs::write(
                &path,
                format!(r#"{{"enabled":false,"serve_ssh":true,"serve_ports":{ports}}}"#),
            )
            .unwrap();
            let settings = load_from(&path, unseal).unwrap();
            assert!(settings.serve_ports.is_empty(), "{ports}");
            // The rest is read as it was written: the switch among it.
            assert!(!settings.enabled && settings.serve_ssh, "{ports}");
        }
    }

    #[test]
    fn secrets_are_written_sealed_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tailnet.json");
        let settings = Settings {
            enabled: true,
            serve_ssh: true,
            serve_ports: vec![5900, 5902],
            serve_ports_to: Some("example.ts.net/n1".into()),
            control_url: Some("https://headscale.example.com".into()),
            hostname: Some("server-a".into()),
            auth_key: Some("tskey-example".into()),
            node_keys: Some("{\"keys\":1}".into()),
            revision: 3,
        };
        save_to(&path, &settings, seal).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("\"tskey-example\""));
        assert!(text.contains("sealed:tskey-example"));
        assert_eq!(load_from(&path, unseal).unwrap(), settings);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn blank_fields_read_as_the_defaults() {
        let mut settings = Settings {
            control_url: Some("  https://headscale.example.com ".into()),
            hostname: Some("   ".into()),
            auth_key: Some(String::new()),
            node_keys: Some("k".into()),
            ..Settings::default()
        };
        settings.normalize();
        assert_eq!(
            settings.control_url.as_deref(),
            Some("https://headscale.example.com")
        );
        assert_eq!(settings.hostname, None);
        assert_eq!(settings.auth_key, None);
        assert_eq!(settings.node_keys.as_deref(), Some("k"));
    }

    #[test]
    fn writes_leave_no_temporary_files_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tailnet.json");
        write_private(&path, b"{}", true).unwrap();
        write_private(&path, b"{\"enabled\":true}", false).unwrap();
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("tailnet.json")]);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"enabled\":true}"
        );
    }

    #[test]
    fn every_save_tells_from_the_last() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tailnet.json");
        assert_eq!(stamp(&path), None);
        write_private(&path, b"{\"enabled\":true }", false).unwrap();
        let first = stamp(&path);
        // The same length, as fast as it goes.
        write_private(&path, b"{\"enabled\":false}", false).unwrap();
        assert!(first.is_some());
        assert_ne!(stamp(&path), first);
    }

    #[test]
    fn debug_and_summary_leave_the_secrets_out() {
        let settings = Settings {
            enabled: true,
            auth_key: Some("tskey-example".into()),
            node_keys: Some("node-secret".into()),
            ..Settings::default()
        };
        let shown = format!("{settings:?}");
        assert!(!shown.contains("tskey-example"));
        assert!(!shown.contains("node-secret"));
        let summary = settings.summary();
        assert!(summary.has_auth_key && summary.has_node_keys);
        assert!(summary.enabled && !summary.serve_ssh);
    }

    #[test]
    fn a_secret_that_no_longer_decrypts_reads_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tailnet.json");
        // An older file's "runs_in" means nothing any more: the mux server
        // holds the node either way, and lets no SSH in until told to.
        std::fs::write(
            &path,
            r#"{"enabled":true,"runs_in":"server","auth_key":"enc:v1:stale","node_keys":"sealed:k"}"#,
        )
        .unwrap();
        let settings = load_from(&path, unseal).unwrap();
        assert_eq!(settings.auth_key, None);
        assert_eq!(settings.node_keys.as_deref(), Some("k"));
        assert!(settings.enabled && !settings.serve_ssh);
    }
}
