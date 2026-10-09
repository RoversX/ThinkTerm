//! Other machines, for the browser.
//!
//! A page cannot open an ssh connection, and the server it was loaded from
//! is often the only machine it can reach. So the page opens a second kind
//! of socket here (`thinkterm.relay.v1`, with the same token) and names a
//! host; this server reaches it over ssh, makes sure ThinkTerm there speaks
//! this bundle's protocol -- installing or updating it, with the person's
//! go-ahead for an install -- and then carries the page's mux connection to
//! that host's own mux server byte for byte. It is what the desktop's
//! SSHMUX domains do from a laptop, with this server in the middle; nothing
//! that passes through is decoded here.
//!
//! Until the bytes start, the socket carries JSON in text messages. The page
//! lists, saves or forgets machines, or opens one; while one opens, this
//! side says how far it got and asks what only a person can answer -- a
//! password, a host key, whether to install. What a person asks to have
//! kept is kept on this server (a password encrypted, beside the desktop's
//! own secrets), so the next connection, and every reconnect, goes through
//! without asking.
//!
//! The token already gives a shell on this server, and through it whatever
//! this server's ssh setup reaches; the relay adds no reach of its own, so
//! there is no list of allowed hosts. It is off until it is turned on --
//! by the desktop's Settings → Web, or by `relay = true` in the listener's
//! `web_servers` entry on a server with no desktop -- and turning it off
//! ends the machines open through it.
//!
//! The same socket is also where the page asks for this machine's tab icon
//! cards: they are edited in the desktop's settings, and a page only draws
//! them.
//!
//! Logs name stages and failure kinds only. Host details and raw command
//! output stay with the page: they may contain names, paths or secrets.

use anyhow::{anyhow, bail, Context};
use codec::{GetCodecVersion, GetCodecVersionResponse, Pdu, CODEC_VERSION};
use config::SshMultiplexing;
use futures::io::{AsyncRead, AsyncWrite};
use mux::domain::Domain;
use mux::ssh::RemoteSshDomain;
use mux::Mux;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use soketto::connection::{Builder, Mode, Sender};
use soketto::Data;
use std::future::Future;
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use thinkterm_core::ssh_hosts::{SshHostEntry, SshHostSource, SshHostSpec};
use wezterm_ssh::{ChildKiller as _, ExecResult, Session, SessionEvent};

/// Largest message taken from the page. Before the bytes start it sends
/// small JSON; after, what the browser client sends over its own socket.
const MAX_MESSAGE: usize = 8 * 1024 * 1024;
/// Messages either side may run ahead of the other before it waits.
const CHANNEL_DEPTH: usize = 16;
/// How long a host's ThinkTerm gets to answer the version question. A mux
/// server that is not running yet is started by that question.
const HELLO_TIMEOUT: Duration = Duration::from_secs(45);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// How long a question waits for its answer. A page can go away without a
/// word (a phone asleep, a dropped network) and nothing else would notice
/// while the worker waits on a person.
const QUESTION_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// The largest version answer read. The pushes a busy host sends ahead of
/// it are skipped without being kept.
const MAX_HELLO_FRAME: u64 = 1024 * 1024;
/// How long the goodbye to the page may take.
const CLOSE_GRACE: Duration = Duration::from_secs(5);
/// Bytes of a host command's error output kept for the page.
const STDERR_TAIL: usize = 4096;
/// What a page is told when reaching other machines is turned off here.
const OFF: &str = "reaching other machines is turned off on this server";
/// How often a machine open through the relay looks at the switch again,
/// so turning it off ends the machine within this long.
const SWITCH_CHECK: Duration = Duration::from_secs(2);
/// The most a host command may print before it is given up on. What is run
/// prints a few lines; a login script that never stops printing must not
/// fill this server's memory in the minute the command has.
const MAX_COMMAND_OUTPUT: usize = 1024 * 1024;
/// How often the shell holding the remote install lock says it is alive,
/// and how long after its last word another attempt may break the lock.
/// Measured from the holder's last sign of life, not from the lock's
/// start: an install with a question in it can hold the lock for longer
/// than any fixed guess, and a holder that died (killed, the host
/// rebooted) is noticed within minutes.
const LOCK_HEARTBEAT_SECS: u64 = 60;
const LOCK_STALE_MINUTES: u64 = 5;
const LOCK_TIMED_OUT: &str = "the host did not answer about its install lock in time";

/// The ThinkTerm this module and the installer put there, after either.
const PROXY_LOCAL: &str = r#"exec "$HOME/.local/bin/thinkterm" cli --prefer-mux proxy"#;

/// What the host is, and what ThinkTerm is there, one `key=value` a line:
/// the program (a Mac's app first, whose session server owns the account's
/// socket whenever the app runs; then `~/.local/bin`, as the desktop's
/// SSHMUX domains look, then PATH), whether the desktop's GUI is there,
/// the variant install.sh recorded, and whether a session server is up.
/// One line: a csh login shell cannot carry a newline inside `sh -c '…'`.
const PROBE: &str = r#"echo "os=$(uname -s)"; echo "arch=$(uname -m)"; echo "libc=$( (getconf GNU_LIBC_VERSION || { command -v ldd >/dev/null && ldd --version 2>&1 | head -n1; }) 2>/dev/null)"; bin=""; for b in "/Applications/ThinkTerm.app/Contents/MacOS/thinkterm" "$HOME/Applications/ThinkTerm.app/Contents/MacOS/thinkterm" "$HOME/.local/bin/thinkterm" "$(command -v thinkterm 2>/dev/null)"; do if [ -n "$b" ] && [ -x "$b" ]; then bin="$b"; break; fi; done; echo "bin=$bin"; if { [ -n "$bin" ] && [ -x "$(dirname "$bin")/thinkterm-gui" ]; } || command -v thinkterm-gui >/dev/null 2>&1; then echo "gui=1"; fi; echo "variant=$(sed -n "s/^variant=//p" "$HOME/.local/share/thinkterm/install-manifest" 2>/dev/null | head -n1)"; for s in "${XDG_RUNTIME_DIR:-/nonexistent}/thinkterm/sock" "$HOME/.local/share/thinkterm/sock"; do if [ -S "$s" ]; then echo "server=1"; break; fi; done"#;

/// Stop the running server there -- the one at that account's default
/// socket and pid file, and nothing else. Never by process name: every
/// ThinkTerm server of the account would match, the desktop's own session
/// server included, and so would this one when the "other machine" is this
/// machine under another name. Run with the ThinkTerm just installed.
const STOP_SERVER: &str =
    r#"export PATH="$HOME/.local/bin:$PATH"; thinkterm cli stop-server --force"#;

/// The two programs a host needs to carry a mux connection: the CLI that
/// runs `proxy`, and the server it starts.
const PROGRAMS: [&str; 2] = ["thinkterm", "thinkterm-mux-server"];

// ---- what the page and this side say ---------------------------------------

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
enum Request {
    List,
    Save {
        machine: NewMachine,
    },
    Forget {
        id: String,
    },
    Open {
        id: String,
    },
    /// This machine's tab icon cards.
    TabIcons,
    Answer {
        id: u32,
        /// None declines: the question was dismissed.
        #[serde(default)]
        value: Option<String>,
        #[serde(default)]
        remember: bool,
    },
}

/// A machine typed into the page.
#[derive(Debug, Deserialize)]
struct NewMachine {
    #[serde(default)]
    label: Option<String>,
    host: String,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    password: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
enum Reply {
    Machines {
        /// This server's own name, for the page to head its own Spaces.
        here: String,
        /// Reaching other machines is turned on here. Off, the list is
        /// empty: nothing about the machines is told to the page.
        enabled: bool,
        machines: Vec<MachineView>,
    },
    Saved {
        id: String,
    },
    Step {
        step: Step,
        /// How much of a copy has gone, for the steps that copy.
        #[serde(skip_serializing_if = "Option::is_none")]
        percent: Option<u8>,
    },
    /// A line of what a command on the host printed, or of the ssh banner.
    Log {
        text: String,
    },
    Ask(Ask),
    /// From the next message on, the socket carries the host's mux
    /// connection.
    Ready {
        version: String,
    },
    Failed {
        reason: Reason,
        message: String,
        /// A plain ssh domain on this server for the same host, when a
        /// terminal there is still worth having without ThinkTerm.
        ssh_domain: Option<String>,
    },
    Error {
        message: String,
    },
    TabIcons(thinkterm_tab_icons::WireCatalog),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Step {
    Connecting,
    Authenticating,
    Checking,
    Installing,
    Updating,
    Starting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum AskKind {
    /// A host key not seen before; `detail` is what ssh says about it.
    HostKey,
    /// A password: answered from what is kept when there is one, and
    /// kept if the person asks.
    Password,
    /// Hidden input that is not a password (a one-time code): never kept.
    Secret,
    /// Visible input.
    Text,
    /// Install ThinkTerm there: asked once per machine.
    Install,
    /// The host runs a newer ThinkTerm than this server; replace it.
    Replace,
    /// The running server there could not hand its sessions over; stop it.
    StopServer,
}

#[derive(Debug, Serialize)]
struct Ask {
    id: u32,
    kind: AskKind,
    prompt: String,
    detail: String,
    /// The answer may be kept.
    remember: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Reason {
    Unreachable,
    HostKey,
    Auth,
    Cancelled,
    Declined,
    CannotInstall,
    /// ThinkTerm there is the desktop app's, which its own updater keeps;
    /// it is never replaced from here.
    Desktop,
    SameServer,
    /// Not on this server's list any more.
    Gone,
    /// Reaching other machines is turned off on this server.
    Off,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Source {
    /// `~/.ssh/config` on this server.
    SshConfig,
    /// The desktop's host catalog on this server.
    Saved,
    /// Added from a browser.
    Web,
}

#[derive(Debug, Serialize)]
struct MachineView {
    id: String,
    label: String,
    endpoint: String,
    source: Source,
    /// A password is kept for it.
    password: bool,
    /// ThinkTerm may be installed there without asking.
    install: bool,
    /// Forgetting it changes something: it was added from a browser, or
    /// a password or the go-ahead to install is kept for it here.
    forgettable: bool,
}

// ---- what is kept ------------------------------------------------------------

/// What this server keeps for the page, per machine.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Kept {
    id: String,
    /// A host added from a browser; None for one this server knows from
    /// `~/.ssh/config` or the desktop's catalog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    host: Option<SshHostSpec>,
    /// Encrypted (`thinkterm_core::secret`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    password: Option<String>,
    /// A key's passphrase, encrypted the same way. Kept apart from the
    /// password: a host may ask for both, and each answer is used once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    passphrase: Option<String>,
    /// The person said yes to installing ThinkTerm there.
    #[serde(default)]
    install: bool,
    /// The `user@host:port` these answers were given for. An alias in
    /// `~/.ssh/config` can be pointed somewhere else later; what was kept
    /// for the old address is not used for the new one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    address: Option<String>,
}

/// Its own file, never the desktop's catalog: a desktop running on this
/// machine holds that catalog in memory and writes it back whole, which
/// would drop whatever a browser added in the meantime.
#[derive(Debug, Default, Serialize, Deserialize)]
struct KeptFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    machines: Vec<Kept>,
}

/// One writer at a time: two tabs saving at once must not lose either.
static KEPT: Mutex<()> = Mutex::new(());

fn kept_path() -> anyhow::Result<PathBuf> {
    Ok(thinkterm_core::credential_data_dir()?.join("web_machines.json"))
}

/// The file, or an empty one where there is none. One that does not parse
/// is an error rather than empty: it may hold passwords, and writing over it
/// would lose them.
fn load_kept(path: &Path) -> anyhow::Result<KeptFile> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("{} is unreadable; move it aside to start afresh", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(KeptFile::default()),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

fn change_kept_at(path: &Path, change: impl FnOnce(&mut KeptFile)) -> anyhow::Result<()> {
    let _one = KEPT.lock().unwrap_or_else(|e| e.into_inner());
    let mut file = load_kept(path)?;
    change(&mut file);
    file.version = 1;
    let text = serde_json::to_vec_pretty(&file)?;
    crate::private_file::replace(path, ".web_machines.", &text)
}

/// Change the file, and let go of what is kept for machines this server no
/// longer lists. A list is only taken at its word when its file is there
/// and reads: a moment's missing or unreadable `~/.ssh/config` (an editor
/// writing it anew, a HOME not mounted yet) does not cost every password
/// kept for its hosts.
fn change_kept(change: impl FnOnce(&mut KeptFile)) -> anyhow::Result<()> {
    let ids = |entries: Vec<SshHostEntry>| entries.into_iter().map(|e| e.id).collect::<Vec<_>>();
    let saved = thinkterm_core::ssh_hosts::saved_hosts_path()
        .ok()
        .filter(|path| path.exists())
        .and_then(|_| thinkterm_core::ssh_hosts::list_saved_hosts().ok())
        .map(ids);
    let system = Some(config::HOME_DIR.join(".ssh").join("config"))
        .filter(|path| path.exists())
        .and_then(|_| thinkterm_core::ssh_hosts::list_system_hosts().ok())
        .map(ids);
    change_kept_at(&kept_path()?, |file| {
        change(file);
        file.machines.retain(|k| {
            // `~/.ssh/config`'s ids are made from the alias with this
            // prefix (thinkterm_core::ssh_hosts); the catalog's are not.
            let list = if k.id.starts_with("system-ssh-") {
                &system
            } else {
                &saved
            };
            k.host.is_some() || list.as_ref().map_or(true, |ids| ids.contains(&k.id))
        });
    })
}

fn kept_entry<'a>(file: &'a mut KeptFile, id: &str) -> &'a mut Kept {
    match file.machines.iter().position(|k| k.id == id) {
        Some(at) => &mut file.machines[at],
        None => {
            file.machines.push(Kept {
                id: id.to_string(),
                ..Kept::default()
            });
            file.machines.last_mut().expect("just pushed")
        }
    }
}

// ---- the machines --------------------------------------------------------------

#[derive(Debug, Clone)]
struct Machine {
    id: String,
    source: Source,
    spec: SshHostSpec,
    /// Where it leads now, as `user@host:port`; see `Kept::address`.
    address: String,
    kept: Kept,
}

impl Machine {
    fn view(&self) -> MachineView {
        MachineView {
            id: self.id.clone(),
            label: self.spec.label.clone(),
            endpoint: thinkterm_core::ssh_hosts::endpoint(&self.spec),
            source: self.source,
            password: self.kept.password.is_some()
                || self.kept.passphrase.is_some()
                || self.spec.password.is_some(),
            install: self.kept.install,
            forgettable: self.source == Source::Web
                || self.kept.password.is_some()
                || self.kept.passphrase.is_some()
                || self.kept.install,
        }
    }

    /// The password kept for it here, else the desktop's.
    fn password(&self) -> Option<String> {
        self.kept
            .password
            .as_deref()
            .or(self.spec.password.as_deref())
            .map(thinkterm_core::secret::reveal)
            .filter(|p| !p.is_empty())
    }

    fn passphrase(&self) -> Option<String> {
        self.kept
            .passphrase
            .as_deref()
            .map(thinkterm_core::secret::reveal)
            .filter(|p| !p.is_empty())
    }
}

/// What is kept for a machine at `address`: nothing of what was kept for
/// the same name at another address.
fn kept_for(kept: &KeptFile, id: &str, address: &str) -> Kept {
    match kept.machines.iter().find(|k| k.id == id) {
        Some(k) if k.address.as_deref().map_or(true, |a| a == address) => k.clone(),
        Some(k) => Kept {
            id: id.to_string(),
            host: k.host.clone(),
            ..Kept::default()
        },
        None => Kept {
            id: id.to_string(),
            ..Kept::default()
        },
    }
}

/// Every machine the page may open: added from a browser, the desktop's
/// catalog, and `~/.ssh/config`, by name. `address_of` says where an
/// `~/.ssh/config` alias leads now.
fn machines_from(
    kept: &KeptFile,
    saved: Vec<SshHostEntry>,
    system: Vec<SshHostEntry>,
    address_of: impl Fn(&SshHostSpec) -> String,
) -> Vec<Machine> {
    let mut machines: Vec<Machine> = kept
        .machines
        .iter()
        .filter_map(|k| {
            let spec = k.host.clone()?;
            let address = thinkterm_core::ssh_hosts::endpoint(&spec);
            Some(Machine {
                id: k.id.clone(),
                source: Source::Web,
                kept: kept_for(kept, &k.id, &address),
                address,
                spec,
            })
        })
        .collect();
    for entry in saved.into_iter().chain(system) {
        if machines.iter().any(|m| m.id == entry.id) {
            continue;
        }
        let source = match entry.source {
            SshHostSource::ThinkTerm => Source::Saved,
            SshHostSource::System => Source::SshConfig,
        };
        let address = match source {
            Source::SshConfig => address_of(&entry.spec),
            _ => thinkterm_core::ssh_hosts::endpoint(&entry.spec),
        };
        machines.push(Machine {
            kept: kept_for(kept, &entry.id, &address),
            address,
            source,
            id: entry.id,
            spec: entry.spec,
        });
    }
    machines.sort_by(|a, b| {
        a.spec
            .label
            .to_lowercase()
            .cmp(&b.spec.label.to_lowercase())
    });
    machines
}

/// Where an `~/.ssh/config` alias leads, as ssh itself would resolve it.
fn resolved_address(spec: &SshHostSpec) -> String {
    let dom = thinkterm_core::ssh_hosts::build_ssh_domain(spec);
    match mux::ssh::ssh_domain_to_ssh_config(&dom) {
        Ok(config) => format!(
            "{}@{}:{}",
            config.get("user").map(String::as_str).unwrap_or(""),
            config.get("hostname").map(String::as_str).unwrap_or(&spec.host),
            config.get("port").map(String::as_str).unwrap_or("22"),
        ),
        Err(_) => thinkterm_core::ssh_hosts::endpoint(spec),
    }
}

fn machines() -> Vec<Machine> {
    let kept = kept_path()
        .and_then(|p| load_kept(&p))
        .unwrap_or_else(|_| {
            log::warn!("nothing kept for the web relay is used: the store is unreadable");
            KeptFile::default()
        });
    let saved = thinkterm_core::ssh_hosts::list_saved_hosts().unwrap_or_else(|_| {
        log::warn!("the desktop's host catalog is unreadable");
        vec![]
    });
    let system = thinkterm_core::ssh_hosts::list_system_hosts().unwrap_or_else(|_| {
        log::warn!("the ssh config is unreadable");
        vec![]
    });
    machines_from(&kept, saved, system, resolved_address)
}

fn machines_reply(enabled: bool) -> Reply {
    Reply::Machines {
        here: hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_default(),
        enabled,
        machines: if enabled {
            machines().iter().map(Machine::view).collect()
        } else {
            vec![]
        },
    }
}

/// Whether the desktop's Settings → Web turned reaching other machines on,
/// as its settings file at `path` says. A file that is not there, or does
/// not read, leaves it off.
fn desktop_allows_at(path: &Path) -> bool {
    #[derive(Default, Deserialize)]
    struct Web {
        #[serde(default)]
        relay: bool,
    }
    #[derive(Default, Deserialize)]
    struct Settings {
        #[serde(default)]
        web: Web,
    }
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Settings>(&text).ok())
        .is_some_and(|settings| settings.web.relay)
}

/// Whether a page may reach other machines through this server: the
/// listener's current `relay = true`, or the desktop's switch. Neither is
/// captured at listener startup, so a config reload also reaches open relays.
fn relay_allowed(listener: &str) -> bool {
    relay_allowed_at(
        &config::configuration().web_servers,
        listener,
        &config::native_settings_path(),
    )
}

fn relay_allowed_at(servers: &[config::WebServer], listener: &str, settings: &Path) -> bool {
    servers
        .iter()
        .find(|server| server.bind_address == listener)
        .is_some_and(|server| server.relay)
        || desktop_allows_at(settings)
}

/// Ends the relay once reaching other machines is turned off: an open
/// machine is let go of within `SWITCH_CHECK` of the switch.
fn watch_switch(attempt: &Attempt, listener: String) -> smol::Task<()> {
    let attempt = attempt.clone();
    crate::connections::spawn_task(async move {
        loop {
            smol::Timer::after(SWITCH_CHECK).await;
            let listener = listener.clone();
            if !smol::unblock(move || relay_allowed(&listener)).await {
                attempt.stop("reaching other machines was turned off on this server");
                return;
            }
        }
    })
}

/// The tab icon cards of the desktop on this machine, as a page draws them:
/// the built-in cards with whatever its settings changed, and the user's
/// own, each with its glyph's SVG. A machine with no desktop settings has
/// the built-in cards.
fn tab_icons_at(settings_path: &Path) -> thinkterm_tab_icons::WireCatalog {
    #[derive(Default, Deserialize)]
    struct Settings {
        #[serde(default)]
        tab_icons: thinkterm_tab_icons::TabIconSettings,
    }
    let settings = std::fs::read_to_string(settings_path)
        .ok()
        .and_then(|text| serde_json::from_str::<Settings>(&text).ok())
        .unwrap_or_default();
    let icons = settings_path.with_file_name("tab-icons");
    // Names are the desktop's to show; a page draws only the icons.
    thinkterm_tab_icons::Catalog::build(&settings.tab_icons, |key| key.to_string()).wire(|hash| {
        let path = icons.join(format!("{hash}.svg"));
        // The desktop keeps no larger ones; a file that grew is not an icon.
        let len = std::fs::metadata(&path).ok()?.len();
        (len <= 256 * 1024)
            .then(|| std::fs::read_to_string(&path).ok())
            .flatten()
    })
}

fn tab_icons_reply() -> Reply {
    Reply::TabIcons(tab_icons_at(&config::native_settings_path()))
}

/// A name that cannot be read as an option or carry a second word.
fn plain_word(text: &str) -> bool {
    !text.is_empty()
        && !text.starts_with('-')
        && !text.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// The record a typed machine becomes; its id follows its address, as the
/// desktop's do, so typing one in twice finds the first.
fn new_machine_spec(new: &NewMachine) -> anyhow::Result<(String, SshHostSpec)> {
    let host = new.host.trim();
    if !plain_word(host) {
        bail!("a host name is one word");
    }
    let user = new.user.as_deref().map(str::trim).filter(|u| !u.is_empty());
    if let Some(user) = user {
        if !plain_word(user) || user.contains('@') {
            bail!("a user name is one word");
        }
    }
    if new.port == Some(0) {
        bail!("port 0 is not a port");
    }
    let label = new
        .label
        .as_deref()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .unwrap_or(host);
    // Everything else the catalog's records carry has a default.
    let mut spec: SshHostSpec = serde_json::from_value(serde_json::json!({
        "label": label,
        "host": host,
    }))?;
    spec.port = new.port.filter(|p| *p != 22);
    spec.username = user.map(str::to_string);
    spec.multiplexing = true;
    let endpoint = thinkterm_core::ssh_hosts::endpoint(&spec);
    let digest = Sha256::digest(endpoint.as_bytes());
    let id = format!("web-{}", hex::encode(&digest[..8]));
    Ok((id, spec))
}

fn save_machine(new: NewMachine) -> anyhow::Result<String> {
    let (id, spec) = new_machine_spec(&new)?;
    let password = match new.password.as_deref().filter(|p| !p.is_empty()) {
        Some(password) => Some(thinkterm_core::secret::encrypt(password)?),
        None => None,
    };
    let saved_id = id.clone();
    change_kept(move |file| {
        let entry = kept_entry(file, &id);
        entry.host = Some(spec);
        if password.is_some() {
            entry.password = password;
        }
    })?;
    Ok(saved_id)
}

/// A machine added from a browser goes; for any other, what was kept for
/// it (its password, the go-ahead to install) does.
fn forget_machine(id: &str) -> anyhow::Result<()> {
    change_kept(|file| file.machines.retain(|k| k.id != id))
}

/// Which kept answer a hidden prompt takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Secret {
    Password,
    Passphrase,
}

/// A password or a key's passphrase: the same answer every time, so worth
/// keeping. A one-time code is neither.
fn secret_kind(prompt: &str) -> Option<Secret> {
    let prompt = prompt.to_lowercase();
    if prompt.contains("passphrase") {
        Some(Secret::Passphrase)
    } else if prompt.contains("password") {
        Some(Secret::Password)
    } else {
        None
    }
}

/// Keep something for `machine`, with the address it was given for.
fn remember(machine: &Machine, what: &'static str, change: impl FnOnce(&mut Kept)) {
    let address = machine.address.clone();
    if change_kept(|file| {
        let entry = kept_entry(file, &machine.id);
        if entry.address.as_deref().is_some_and(|a| a != address) {
            // Kept for another address: none of it belongs to this one.
            entry.password = None;
            entry.passphrase = None;
            entry.install = false;
        }
        entry.address = Some(address);
        change(entry);
    })
    .is_err()
    {
        log::warn!("not keeping {what}: the store could not be updated");
    }
}

fn remember_secret(machine: &Machine, kind: Secret, value: &str) {
    let stored = match thinkterm_core::secret::encrypt(value) {
        Ok(stored) => stored,
        Err(_) => {
            log::warn!("not keeping a {kind:?}: encryption failed");
            return;
        }
    };
    remember(machine, "a password", |entry| match kind {
        Secret::Password => entry.password = Some(stored),
        Secret::Passphrase => entry.passphrase = Some(stored),
    });
}

fn remember_install(machine: &Machine) {
    remember(machine, "the go-ahead to install", |entry| entry.install = true);
}

// ---- the page, from the worker's side ----------------------------------------

#[derive(Default)]
struct AttemptState {
    /// The relay is over: the page went away, or its token was revoked.
    stopped: Option<&'static str>,
    /// The work on the host is over, the relay is not: one of its steps
    /// ran out of time. The page is still there to be told why.
    halted: Option<&'static str>,
    session: Option<Session>,
}

#[derive(Clone)]
struct Attempt {
    state: Arc<Mutex<AttemptState>>,
    /// Closed when the relay stops.
    ended: smol::channel::Receiver<()>,
    end: smol::channel::Sender<()>,
    /// Closed when the work on the host ends: a halt, or the relay
    /// stopping.
    over: smol::channel::Receiver<()>,
    halt: smol::channel::Sender<()>,
}

impl Attempt {
    fn new() -> Self {
        let (end, ended) = smol::channel::bounded(1);
        let (halt, over) = smol::channel::bounded(1);
        Self {
            state: Arc::new(Mutex::new(AttemptState::default())),
            end,
            ended,
            halt,
            over,
        }
    }

    /// End the relay, and the work on the host with it.
    fn stop(&self, reason: &'static str) {
        let session = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.stopped.get_or_insert(reason);
            state.session.take()
        };
        self.end.close();
        self.halt.close();
        if let Some(session) = session {
            session.shutdown();
        }
    }

    /// End the work on the host only: the socket stays, so the page hears
    /// why it ended.
    fn halt(&self, reason: &'static str) {
        let session = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.halted.get_or_insert(reason);
            state.session.take()
        };
        self.halt.close();
        if let Some(session) = session {
            session.shutdown();
        }
    }

    /// Why the work on the host was halted, if it was.
    fn halted(&self) -> Option<&'static str> {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).halted
    }

    /// The worker's check: the relay still runs and its work is not halted.
    fn check(&self) -> anyhow::Result<()> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match state.stopped.or(state.halted) {
            Some(reason) => bail!("{reason}"),
            None => Ok(()),
        }
    }

    /// The socket's check: the relay still runs.
    fn check_page(&self) -> anyhow::Result<()> {
        match self.state.lock().unwrap_or_else(|e| e.into_inner()).stopped {
            Some(reason) => bail!("{reason}"),
            None => Ok(()),
        }
    }

    fn bind(&self, session: &Session) -> anyhow::Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(reason) = state.stopped.or(state.halted) {
            session.shutdown();
            bail!("{reason}");
        }
        state.session = Some(session.clone());
        Ok(())
    }

    /// `future`, on the socket's side: given up on once the relay stops.
    async fn until_stopped<T>(&self, future: impl Future<Output = T>) -> anyhow::Result<T> {
        self.check_page()?;
        smol::future::or(
            async {
                let _ = self.ended.recv().await;
                self.check_page()?;
                bail!("the relay stopped")
            },
            async { Ok(future.await) },
        )
        .await
    }

    /// `future`, on the worker's side: given up on once its work ends.
    fn wait<T>(&self, future: impl Future<Output = T>) -> anyhow::Result<T> {
        smol::block_on(async {
            self.check()?;
            smol::future::or(
                async {
                    let _ = self.over.recv().await;
                    self.check()?;
                    bail!("the relay stopped")
                },
                async { Ok(future.await) },
            )
            .await
        })
    }

    /// A deadline on a step of the work on the host, `why` its message.
    /// Dropping the task disarms it; expiry halts the work -- the SSH
    /// transport, and the worker's waits on its pipes and replies -- and
    /// leaves the socket to say why.
    fn deadline(&self, timeout: Duration, why: &'static str) -> smol::Task<()> {
        let attempt = self.clone();
        crate::connections::spawn_task(async move {
            smol::Timer::after(timeout).await;
            attempt.halt(why);
        })
    }

    /// A deadline on the socket's own side: the page, or once the bytes
    /// run the host, stopped reading. Expiry ends the relay.
    fn stop_after(&self, timeout: Duration, why: &'static str) -> smol::Task<()> {
        let attempt = self.clone();
        crate::connections::spawn_task(async move {
            smol::Timer::after(timeout).await;
            attempt.stop(why);
        })
    }
}

/// One deadline over every send while the bytes run: a send that has
/// waited `limit` stops the relay. A deadline of its own for each chunk
/// cost a task and a timer per chunk, thousands a second when output
/// floods.
struct Watchdog {
    /// When the send under way began, in ms after `start`; 0 for none.
    since: Arc<AtomicU64>,
    start: Instant,
    _task: smol::Task<()>,
}

impl Watchdog {
    fn new(attempt: &Attempt, limit: Duration, why: &'static str) -> Self {
        let since = Arc::new(AtomicU64::new(0));
        let start = Instant::now();
        let watched = Arc::clone(&since);
        let attempt = attempt.clone();
        let task = crate::connections::spawn_task(async move {
            loop {
                smol::Timer::after(limit / 4).await;
                let began = watched.load(Ordering::Relaxed);
                let now = start.elapsed().as_millis() as u64;
                if began != 0 && now >= began + limit.as_millis() as u64 {
                    attempt.stop(why);
                    return;
                }
            }
        });
        Self {
            since,
            start,
            _task: task,
        }
    }

    /// A send begins.
    fn arm(&self) {
        let now = self.start.elapsed().as_millis() as u64;
        self.since.store(now.max(1), Ordering::Relaxed);
    }

    /// It is done.
    fn done(&self) {
        self.since.store(0, Ordering::Relaxed);
    }
}

struct StopAttempt(Attempt);

impl Drop for StopAttempt {
    fn drop(&mut self) {
        self.0.stop("the page went away");
    }
}

struct Answered {
    id: u32,
    value: Option<String>,
    remember: bool,
}

/// What the worker sends the socket, in order: what it says to the page,
/// then what it came to.
enum Worker {
    Say(Reply),
    Done(Result<Reached, Failure>),
}

/// What the ssh work says to the page, and the answers it waits on. It runs
/// on a thread of its own: ssh and the commands it runs block.
struct Page {
    attempt: Attempt,
    tx: smol::channel::Sender<Worker>,
    answers: smol::channel::Receiver<Answered>,
    next_ask: std::cell::Cell<u32>,
}

impl Page {
    fn say(&self, reply: Reply) {
        let _ = self.attempt.wait(self.tx.send(Worker::Say(reply)));
    }

    fn step(&self, step: Step) {
        self.say(Reply::Step {
            step,
            percent: None,
        });
    }

    fn log(&self, text: &str) {
        for line in text.lines() {
            self.say(Reply::Log {
                text: line.to_string(),
            });
        }
    }

    /// The person's answer and whether to keep it; None when they
    /// dismissed the question or the page went away.
    fn ask(&self, kind: AskKind, prompt: &str, detail: &str) -> Option<(String, bool)> {
        let _deadline = self
            .attempt
            .deadline(QUESTION_TIMEOUT, "the question went unanswered for ten minutes");
        let id = self.next_ask.get();
        self.next_ask.set(id + 1);
        self.say(Reply::Ask(Ask {
            id,
            kind,
            prompt: prompt.to_string(),
            detail: detail.to_string(),
            remember: kind == AskKind::Password,
        }));
        loop {
            let answered = self.attempt.wait(self.answers.recv()).ok()?.ok()?;
            // An answer to an earlier question, arriving late.
            if answered.id != id {
                continue;
            }
            let remember = answered.remember;
            return answered.value.map(|value| (value, remember));
        }
    }

    fn agrees(&self, kind: AskKind, detail: &str) -> bool {
        matches!(self.ask(kind, "", detail), Some((answer, _)) if answer == "yes")
    }
}

#[derive(Debug)]
struct Failure {
    reason: Reason,
    message: String,
    /// A plain ssh terminal could still be had there.
    fallback: bool,
    /// The domain for that terminal, once there is one.
    ssh_domain: Option<String>,
}

impl Failure {
    fn new(reason: Reason, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
            fallback: false,
            ssh_domain: None,
        }
    }

    fn with_fallback(mut self) -> Self {
        self.fallback = true;
        self
    }

    fn failed(err: anyhow::Error) -> Self {
        Self::new(Reason::Failed, format!("{err:#}"))
    }
}

// ---- reaching a machine ----------------------------------------------------------

/// A host command's error output, kept short for the page and out of logs.
#[derive(Clone, Default)]
struct Tail(Arc<Mutex<String>>);

impl Tail {
    fn text(&self) -> String {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .trim()
            .to_string()
    }

    /// Drain `stream` on a thread of its own until it ends.
    fn drain(
        &self,
        mut stream: wezterm_ssh::FileDescriptor,
        what: &'static str,
    ) -> Option<std::thread::JoinHandle<()>> {
        let tail = self.clone();
        std::thread::Builder::new()
            .name("web-relay-stderr".into())
            .spawn(move || {
                let mut buf = [0u8; 2048];
                while let Ok(n) = stream.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    let text = String::from_utf8_lossy(&buf[..n]);
                    log::debug!("{what}: received {n} bytes of output");
                    let mut kept = tail.0.lock().unwrap_or_else(|e| e.into_inner());
                    kept.push_str(&text);
                    if kept.len() > STDERR_TAIL {
                        let mut cut = kept.len() - STDERR_TAIL;
                        while !kept.is_char_boundary(cut) {
                            cut += 1;
                        }
                        kept.drain(..cut);
                    }
                }
            })
            .ok()
    }
}

/// The host's mux connection, past its first question.
struct Proxy {
    stdin: wezterm_ssh::FileDescriptor,
    stdout: wezterm_ssh::FileDescriptor,
    /// Never waited on: a wait on a channel whose input is still ours can
    /// block for good. It goes with the session.
    child: wezterm_ssh::SshChildProcess,
    stderr: Tail,
}

enum Hello {
    /// It speaks this bundle's protocol.
    Speaks(GetCodecVersionResponse),
    /// A ThinkTerm of another protocol answered.
    Other(GetCodecVersionResponse),
    /// Something answered that this build cannot read: an older ThinkTerm.
    Unreadable,
    /// Nothing answered; the error output says why.
    Silent(String),
}

struct Reached {
    session: Session,
    proxy: Proxy,
    version: String,
}

/// A script for `sh`, whatever the login shell is.
fn sh(script: &str) -> String {
    format!("sh -c '{}'", script.replace('\'', r"'\''"))
}

/// A command whose input is not ours to give: closing our end of a
/// channel closes all of it, so it reads from /dev/null instead.
fn sh_no_input(script: &str) -> String {
    format!("{} </dev/null", sh(script))
}

struct Ran {
    ok: bool,
    out: String,
    err: String,
}

/// Run `script` and wait for it, collecting what it printed. With `page`,
/// its output goes there as it comes.
fn run(session: &Session, script: &str, page: &Page, output: bool) -> anyhow::Result<Ran> {
    run_for(
        session,
        script,
        page,
        output,
        COMMAND_TIMEOUT,
        "a command on the host did not finish in time",
    )
}

/// `run`, given `timeout` rather than a minute; `why` says what ran out of
/// time if it does.
fn run_for(
    session: &Session,
    script: &str,
    page: &Page,
    output: bool,
    timeout: Duration,
    why: &'static str,
) -> anyhow::Result<Ran> {
    page.attempt.check()?;
    let _deadline = page.attempt.deadline(timeout, why);
    let ExecResult {
        stdin,
        mut stdout,
        stderr,
        mut child,
    } = page
        .attempt
        .wait(session.exec(&sh_no_input(script), None))?
        .with_context(|| format!("running `{script}`"))?;
    let tail = Tail::default();
    let errors = tail.drain(stderr, "host command");
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        page.attempt.check()?;
        let n = stdout.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if output {
            page.log(&String::from_utf8_lossy(&buf[..n]));
        }
        if out.len() + n > MAX_COMMAND_OUTPUT {
            bail!(
                "a command on the host printed more than {} KiB",
                MAX_COMMAND_OUTPUT / 1024
            );
        }
        out.extend_from_slice(&buf[..n]);
    }
    if let Some(errors) = errors {
        let _ = errors.join();
    }
    let status = page.attempt.wait(child.async_wait())??;
    // Ours until the command is done; see `sh_no_input`.
    drop(stdin);
    let err = tail.text();
    if output {
        page.log(&err);
    }
    Ok(Ran {
        ok: status.success(),
        out: String::from_utf8_lossy(&out).into_owned(),
        err,
    })
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Probe {
    os: String,
    arch: String,
    libc: String,
    /// Where ThinkTerm is there, if it is.
    bin: Option<String>,
    /// The desktop's GUI sits beside it.
    gui: bool,
    /// The variant install.sh recorded, when install.sh put it there.
    variant: Option<String>,
    /// A ThinkTerm session server is running there.
    server: bool,
}

fn parse_probe(text: &str) -> Probe {
    let mut probe = Probe::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().to_string();
        match key {
            "os" => probe.os = value,
            "arch" => probe.arch = value,
            "libc" => probe.libc = value,
            "bin" if !value.is_empty() => probe.bin = Some(value),
            "gui" => probe.gui = value == "1",
            "variant" if !value.is_empty() => probe.variant = Some(value),
            "server" => probe.server = value == "1",
            _ => {}
        }
    }
    probe
}

impl Probe {
    /// ThinkTerm there belongs to the desktop app: its GUI is there, or a
    /// Mac's app, or install.sh's desktop variant, or a session server is
    /// up with no program this probe could find. The app and its own
    /// updater keep its programs; replacing two of them from here would
    /// leave the rest at another version.
    fn desktop(&self) -> bool {
        self.gui
            || self.variant.as_deref() == Some("desktop")
            || self
                .bin
                .as_deref()
                .is_some_and(|bin| bin.contains(".app/Contents/MacOS/"))
            || (self.bin.is_none() && self.server)
    }

    /// The ThinkTerm found there, as one word for `sh`.
    fn quoted_bin(&self) -> Option<String> {
        let bin = self.bin.as_deref()?;
        Some(format!("'{}'", bin.replace('\'', r"'\''")))
    }

    /// The host's mux connection through the ThinkTerm found there.
    fn proxy(&self) -> Option<String> {
        Some(format!("exec {} cli --prefer-mux proxy", self.quoted_bin()?))
    }

    /// Ask the ThinkTerm found there which version it is.
    fn version_command(&self) -> Option<String> {
        Some(format!("{} --version", self.quoted_bin()?))
    }

    /// Why a release cannot run there, if it cannot: the checks install.sh
    /// makes first, made here so the answer comes before the question.
    fn cannot_take_a_release(&self) -> Option<String> {
        match self.os.as_str() {
            "Darwin" => None,
            "Linux" => {
                if !matches!(self.arch.as_str(), "x86_64" | "aarch64") {
                    return Some(format!("{} has no release build", self.arch));
                }
                if self.libc.to_ascii_lowercase().contains("musl") {
                    return Some("the host uses musl libc".to_string());
                }
                let glibc = thinkterm_update::parse_glibc_version(&self.libc)?;
                (glibc < thinkterm_update::MIN_GLIBC).then(|| {
                    format!(
                        "the host has glibc {}.{} and the release binaries need {}.{}",
                        glibc.0,
                        glibc.1,
                        thinkterm_update::MIN_GLIBC.0,
                        thinkterm_update::MIN_GLIBC.1
                    )
                })
            }
            other => Some(format!("{other} has no release build")),
        }
    }

    /// Whether this server's own programs can run there as they are.
    fn runs_ours(&self) -> bool {
        let os = match self.os.as_str() {
            "Linux" => "linux",
            "Darwin" => "macos",
            _ => return false,
        };
        let arch = match self.arch.as_str() {
            "arm64" => "aarch64",
            other => other,
        };
        if os != std::env::consts::OS || arch != std::env::consts::ARCH {
            return false;
        }
        // A Linux build needs at most the glibc it runs on here; a host
        // with one as new runs it, and with an older one (or musl, or a
        // glibc it would not name) it may not.
        os != "linux"
            || matches!(
                (thinkterm_update::parse_glibc_version(&self.libc), local_glibc()),
                (Some(there), Some(here)) if there >= here
            )
    }
}

/// The glibc this server runs on; None off Linux, or where it cannot say.
fn local_glibc() -> Option<(u32, u32)> {
    static GLIBC: std::sync::OnceLock<Option<(u32, u32)>> = std::sync::OnceLock::new();
    if !cfg!(target_os = "linux") {
        return None;
    }
    *GLIBC.get_or_init(|| {
        let out = std::process::Command::new("getconf")
            .arg("GNU_LIBC_VERSION")
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        thinkterm_update::parse_glibc_version(&String::from_utf8_lossy(&out.stdout))
    })
}

/// The version `thinkterm --version` printed, among whatever else a login
/// script there put around it: the word after `thinkterm`, compared whole
/// (0.1.1 is not 0.1.10).
fn version_in(text: &str) -> Option<&str> {
    text.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        if words.next() == Some("thinkterm") {
            words.next()
        } else {
            None
        }
    })
}

/// What ssh asks, as one line for a heading: without its empty lines and
/// the bare `>` some prompts end with.
fn clean_prompt(prompt: &str) -> String {
    prompt
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && *line != ">")
        .collect::<Vec<_>>()
        .join(" ")
}

/// The agent socket this server's panes are given, when it leads anywhere,
/// else the one it started with.
fn relay_agent() -> Option<String> {
    Mux::try_get()
        .and_then(|mux| mux.ssh_agent_path())
        .filter(|path| path.exists())
        .map(|path| path.to_string_lossy().into_owned())
        .or_else(mux::ssh_agent::AgentProxy::default_ssh_auth_sock)
}

/// Open the ssh session, answering what can be answered from what is kept
/// and asking the page the rest.
fn authenticate(machine: &Machine, page: &Page) -> Result<Session, Failure> {
    page.attempt.check().map_err(Failure::failed)?;
    let dom = thinkterm_core::ssh_hosts::build_ssh_domain(&machine.spec);
    let mut config = mux::ssh::ssh_domain_to_ssh_config(&dom).map_err(Failure::failed)?;
    // This server scrubs SSH_AUTH_SOCK from its own environment, and ssh
    // here only finds an agent through it: name the one its panes get, as
    // `ssh` typed into a pane here would use, unless the config names one.
    if !config.contains_key("identityagent") {
        if let Some(agent) = relay_agent() {
            config.insert("identityagent".to_string(), agent);
        }
    }
    // Each kept answer once: one that is refused is asked for again.
    let mut kept_password = machine.password();
    let mut kept_passphrase = machine.passphrase();
    // What the person typed and asked to keep, the latest of each kind:
    // the host asking again for one means the one before was refused.
    let mut typed: Vec<(Secret, String)> = vec![];
    let (session, events) = Session::connect(config)
        .map_err(|err| Failure::new(Reason::Unreachable, format!("{err:#}")))?;
    page.attempt.bind(&session).map_err(Failure::failed)?;
    loop {
        let event = {
            let _deadline = page
                .attempt
                .deadline(COMMAND_TIMEOUT, "the host stopped answering while signing in");
            page.attempt.wait(events.recv()).map_err(Failure::failed)?
        };
        let event = match event {
            Ok(event) => event,
            Err(_) => break,
        };
        match event {
            SessionEvent::Banner(banner) => {
                if let Some(banner) = banner {
                    page.log(&banner);
                }
            }
            SessionEvent::HostVerify(verify) => {
                let ok = page.agrees(AskKind::HostKey, &verify.message);
                // Answered either way, so ssh is never left waiting.
                smol::block_on(verify.answer(ok)).map_err(Failure::failed)?;
                if !ok {
                    return Err(Failure::new(
                        Reason::Cancelled,
                        "the host key was not accepted",
                    ));
                }
            }
            SessionEvent::Authenticate(auth) => {
                page.step(Step::Authenticating);
                let mut detail = auth.instructions.trim().to_string();
                if !auth.username.is_empty() {
                    if !detail.is_empty() {
                        detail.push('\n');
                    }
                    detail.push_str(&auth.username);
                }
                let mut answers = vec![];
                for prompt in &auth.prompts {
                    let secret = if prompt.echo {
                        None
                    } else {
                        secret_kind(&prompt.prompt)
                    };
                    let kept = match secret {
                        Some(Secret::Password) => kept_password.take(),
                        Some(Secret::Passphrase) => kept_passphrase.take(),
                        None => None,
                    };
                    if let Some(kept) = kept {
                        answers.push(kept);
                        continue;
                    }
                    let kind = if prompt.echo {
                        AskKind::Text
                    } else if secret.is_some() {
                        AskKind::Password
                    } else {
                        AskKind::Secret
                    };
                    match page.ask(kind, &clean_prompt(&prompt.prompt), &detail) {
                        Some((value, remember)) => {
                            if let Some(secret) = secret {
                                typed.retain(|(kind, _)| *kind != secret);
                                if remember {
                                    typed.push((secret, value.clone()));
                                }
                            }
                            answers.push(value);
                        }
                        None => {
                            return Err(Failure::new(
                                Reason::Cancelled,
                                "the sign-in was cancelled",
                            ))
                        }
                    }
                }
                smol::block_on(auth.answer(answers)).map_err(Failure::failed)?;
            }
            SessionEvent::HostVerificationFailed(failed) => {
                return Err(Failure::new(
                    Reason::HostKey,
                    format!(
                        "the host key of {} has changed (it is now {}){}",
                        failed.remote_address,
                        failed.key,
                        failed
                            .file
                            .map(|f| format!("; the old one is in {}", f.display()))
                            .unwrap_or_default()
                    ),
                ));
            }
            SessionEvent::Error(err) => {
                let reason = if refused(&err) {
                    Reason::Auth
                } else {
                    Reason::Unreachable
                };
                return Err(Failure::new(reason, err));
            }
            SessionEvent::Authenticated => {
                for (secret, value) in &typed {
                    remember_secret(machine, *secret, value);
                }
                return Ok(session);
            }
        }
    }
    Err(Failure::new(
        Reason::Unreachable,
        "the connection ended during the sign-in",
    ))
}

/// Whether an ssh error is the host turning the sign-in down, rather than
/// the connection failing under it. Only a refusal is final: the page stops
/// trying a machine whose sign-in failed, so a network that dropped while
/// a kept password was on its way must not read as one. Both backends put
/// every error of the sign-in under "authentication"; these are the ones
/// that mean no.
fn refused(err: &str) -> bool {
    let err = err.to_lowercase();
    // libssh: "password auth status: Denied", "interactive auth status:
    // Denied"; and "unhandled auth case" once no method is left to try.
    err.contains("status: denied")
        || err.contains("unhandled auth case")
        // libssh2's own words for a refused password or key.
        || err.contains("authentication failed")
        || err.contains("permission denied")
}

/// A LEB128 number off the stream, kept as read as well.
fn read_varint(r: &mut impl Read, raw: &mut Vec<u8>) -> std::io::Result<u64> {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
        let mut byte = [0u8; 1];
        r.read_exact(&mut byte)?;
        raw.push(byte[0]);
        if shift >= 64 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "a number too long"));
        }
        value |= u64::from(byte[0] & 0x7f) << shift;
        if byte[0] & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
    }
}

/// The answer to request 1 on a fresh mux connection, as `Pdu::decode`
/// reads it. A server subscribes a connection before it is asked anything,
/// so a busy host's pushes (serial 0) can come first; they are skipped
/// whole, without being decoded -- a server of another protocol may send
/// what this build cannot read.
fn read_answer(r: &mut impl Read) -> std::io::Result<Vec<u8>> {
    loop {
        let mut frame = Vec::new();
        // The high bit marks a compressed frame; the rest is its length.
        let len = read_varint(r, &mut frame)? & !(1u64 << 63);
        let mut header = Vec::new();
        let serial = read_varint(r, &mut header)?;
        let rest = len.checked_sub(header.len() as u64).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "a frame shorter than its serial")
        })?;
        if serial != 1 {
            std::io::copy(&mut r.by_ref().take(rest), &mut std::io::sink())?;
            continue;
        }
        if rest > MAX_HELLO_FRAME {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "an answer too large"));
        }
        frame.extend_from_slice(&header);
        let at = frame.len();
        frame.resize(at + rest as usize, 0);
        r.read_exact(&mut frame[at..])?;
        return Ok(frame);
    }
}

/// Start the host's mux connection with `command` and ask it which
/// protocol it speaks.
fn hello(session: &Session, page: &Page, command: &str) -> anyhow::Result<(Proxy, Hello)> {
    page.attempt.check()?;
    let _deadline = page
        .attempt
        .deadline(HELLO_TIMEOUT, "ThinkTerm there did not answer in time");
    let ExecResult {
        mut stdin,
        stdout,
        stderr,
        child,
    } = page
        .attempt
        .wait(session.exec(&sh(command), None))?
        .context("starting ThinkTerm there")?;
    let tail = Tail::default();
    // It runs as long as the connection does.
    let _ = tail.drain(stderr, "thinkterm proxy");
    Pdu::GetCodecVersion(GetCodecVersion {})
        .encode(&mut stdin, 1)
        .context("asking ThinkTerm there for its version")?;
    stdin.flush()?;

    // On a thread of its own so a host that never answers can be given up
    // on; the read ends when the session does.
    struct Counted(wezterm_ssh::FileDescriptor, usize);
    impl Read for Counted {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.0.read(buf)?;
            self.1 += n;
            Ok(n)
        }
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("web-relay-hello".into())
        .spawn(move || {
            let mut counted = Counted(stdout, 0);
            let decoded = read_answer(&mut counted)
                .map_err(anyhow::Error::from)
                .and_then(|frame| Pdu::decode(std::io::Cursor::new(frame)));
            let _ = tx.send((decoded, counted));
        })?;
    let (decoded, counted) = rx
        .recv_timeout(HELLO_TIMEOUT)
        .map_err(|_| anyhow!("ThinkTerm there did not answer within {HELLO_TIMEOUT:?}"))?;
    page.attempt.check()?;
    let hello = match decoded {
        Ok(decoded) => match decoded.pdu {
            Pdu::GetCodecVersionResponse(info) if info.codec_vers == CODEC_VERSION => {
                Hello::Speaks(info)
            }
            Pdu::GetCodecVersionResponse(info) => Hello::Other(info),
            _ => Hello::Unreadable,
        },
        Err(_) if counted.1 > 0 => Hello::Unreadable,
        Err(_) => {
            // The error output's end may still be on its way.
            std::thread::sleep(Duration::from_millis(200));
            Hello::Silent(tail.text())
        }
    };
    Ok((
        Proxy {
            stdin,
            stdout: counted.0,
            child,
            stderr: tail,
        },
        hello,
    ))
}

/// Copy `local` to `~/.local/bin/<name>` there, through the command's input:
/// `head -c` stops at the size, so the input never has to be closed.
fn upload(
    session: &Session,
    local: &Path,
    name: &str,
    page: &Page,
    step: Step,
) -> anyhow::Result<()> {
    page.attempt.check()?;
    let _deadline = page
        .attempt
        .deadline(INSTALL_TIMEOUT, "copying ThinkTerm there did not finish in time");
    let mut file =
        std::fs::File::open(local).with_context(|| format!("opening {}", local.display()))?;
    let size = file.metadata()?.len();
    let script = format!(
        r#"umask 077; mkdir -p "$HOME/.local/bin" && head -c {size} > "$HOME/.local/bin/{name}" && chmod 755 "$HOME/.local/bin/{name}""#
    );
    let ExecResult {
        mut stdin,
        stdout,
        stderr,
        mut child,
    } = page
        .attempt
        .wait(session.exec(&sh(&script), None))?
        .context("copying ThinkTerm there")?;
    let tail = Tail::default();
    let errors = tail.drain(stderr, "copy");
    let output = tail.drain(stdout, "copy");
    let mut buf = vec![0u8; 256 * 1024];
    let (mut sent, mut shown) = (0u64, 0u8);
    loop {
        page.attempt.check()?;
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        stdin.write_all(&buf[..n])?;
        sent += n as u64;
        let percent = (sent * 100 / size.max(1)) as u8;
        if percent >= shown.saturating_add(5) {
            shown = percent;
            page.say(Reply::Step {
                step,
                percent: Some(percent),
            });
        }
    }
    stdin.flush()?;
    // A vector, not an array: this crate is on the 2018 edition, where an
    // array's `into_iter` yields references.
    for drain in vec![errors, output].into_iter().flatten() {
        let _ = drain.join();
    }
    let status = page.attempt.wait(child.async_wait())??;
    drop(stdin);
    if !status.success() {
        bail!("copying {name} failed: {}", tail.text());
    }
    Ok(())
}

/// Put this server's own ThinkTerm on the host. Where install.sh put
/// ThinkTerm, install.sh does it again at this version, so its programs,
/// web bundle and manifest move together. Elsewhere (nothing there yet, or
/// this module's own earlier copy) the programs this server runs from are
/// copied when the host can run them as they are, else the release of the
/// same version is fetched there by the installer. A desktop install never
/// gets here; see `Probe::desktop`.
fn install(
    session: &Session,
    probe: &Probe,
    page: &Page,
    step: Step,
    lock: &InstallLock,
) -> Result<(), Failure> {
    // No deadline over the whole: each copy and the installer have their
    // own, and one over all of them left the installer whatever a slow
    // copy had not used.
    page.attempt.check().map_err(Failure::failed)?;
    if copies_ours(probe) {
        match copy_ours(session, page, step, &lock.directory) {
            Ok(()) => return Ok(()),
            Err(err) => page.log(&format!("Could not copy ThinkTerm there: {err:#}")),
        }
    }
    page.attempt.check().map_err(Failure::failed)?;
    if let Some(reason) = release_blocker(probe) {
        return Err(Failure::new(Reason::CannotInstall, reason).with_fallback());
    }
    let version = thinkterm_update::running_release_version();
    let variant = "server";
    page.attempt.check().map_err(Failure::failed)?;
    let command = thinkterm_update::install_command(variant, &version);
    page.log(&format!("Installing ThinkTerm {version} ({variant}) there"));
    match run_for(
        session,
        &command,
        page,
        true,
        INSTALL_TIMEOUT,
        "installing ThinkTerm there did not finish in time",
    ) {
        Ok(ran) if ran.ok => Ok(()),
        Ok(_) => {
            Err(Failure::new(Reason::CannotInstall, "the installer failed there").with_fallback())
        }
        Err(err) => Err(Failure::new(Reason::CannotInstall, format!("{err:#}")).with_fallback()),
    }
}

/// Whether `install` copies this server's own programs there first: where
/// nothing of install.sh's is there to keep in step, and they run there.
fn copies_ours(probe: &Probe) -> bool {
    probe.variant.is_none() && probe.runs_ours() && programs_to_copy().is_ok()
}

/// Why no release can be installed there, if none can.
fn release_blocker(probe: &Probe) -> Option<String> {
    let version = thinkterm_update::running_release_version();
    if !thinkterm_update::is_release_version(&version) {
        return Some(format!(
            "this server runs a development build ({version}), which has no release to install \
             there{}",
            if probe.variant.is_some() {
                "; ThinkTerm there was put there by install.sh, which updates it"
            } else {
                ""
            }
        ));
    }
    probe.cannot_take_a_release()
}

/// Why this server cannot put its ThinkTerm there at all, if it cannot:
/// asked before any question, so a host that can take nothing is told so
/// rather than asked to agree first. `install` makes the same choices.
fn install_blocker(probe: &Probe) -> Option<String> {
    if copies_ours(probe) {
        return None;
    }
    release_blocker(probe)
}

/// Where the programs this server runs from are, when they are worth
/// sending: both of them there, and the build this server runs.
fn programs_to_copy() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow!("{} has no directory", exe.display()))?;
    for name in PROGRAMS {
        let local = dir.join(name);
        if !local.is_file() {
            bail!("{} is not here", local.display());
        }
    }
    // What lies beside this server may be newer than what it runs: updated
    // on disk, not yet restarted. Only the build that speaks this
    // server's protocol is worth sending.
    let ours = config::wezterm_version();
    let beside = std::process::Command::new(dir.join("thinkterm"))
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .context("asking the ThinkTerm beside this server for its version")?;
    let beside = String::from_utf8_lossy(&beside.stdout);
    if version_in(&beside) != Some(ours) {
        bail!(
            "the ThinkTerm beside this server is {}, not the {ours} it runs; restart this \
             server to send it",
            beside.trim()
        );
    }
    Ok(dir.to_path_buf())
}

/// Copy the programs this server runs from, then swap them in together
/// once the copy runs there.
fn copy_ours(session: &Session, page: &Page, step: Step, directory: &str) -> anyhow::Result<()> {
    let dir = programs_to_copy()?;
    let ours = config::wezterm_version();
    for name in PROGRAMS {
        page.log(&format!("Copying {name}"));
        upload(
            session,
            &dir.join(name),
            &format!("{directory}/{name}"),
            page,
            step,
        )?;
    }
    let check = run(
        session,
        &format!(r#""$HOME/.local/bin/{directory}/thinkterm" --version"#),
        page,
        false,
    )?;
    if !check.ok {
        bail!("the copy does not run there: {}", check.err);
    }
    if version_in(&check.out) != Some(ours) {
        bail!("the copy there reports {}, not {ours}", check.out.trim());
    }
    page.log(check.out.trim());
    let moved = run(
        session,
        &format!(
            r#"cd "$HOME/.local/bin" && mv -f "{directory}/thinkterm" thinkterm && mv -f "{directory}/thinkterm-mux-server" thinkterm-mux-server"#
        ),
        page,
        false,
    )?;
    if !moved.ok {
        bail!("putting the copy in place failed: {}", moved.err);
    }
    Ok(())
}

/// The lock belongs to a live remote shell, not this server's host alias:
/// different aliases and different relay servers must exclude each other.
/// Its input stays open through install and takeover. EOF or HUP cleans
/// up only this attempt's private staging directory and releases the lock.
struct InstallLock {
    directory: String,
    command: Option<ExecResult>,
}

/// One line, for the reason `PROBE` is one. While the shell holds the lock
/// a helper of its own touches it every `LOCK_HEARTBEAT_SECS`, for as long
/// as that shell lives (`touch -c`: never making the lock again after the
/// cleanup took it). A lock untouched for `LOCK_STALE_MINUTES` is one whose
/// shell died without its cleanup (killed, or the host rebooted): it is
/// broken rather than waited on for ever.
fn install_lock_script(directory: &str) -> String {
    format!(
        r#"umask 077; mkdir -p "$HOME/.local/bin" || exit 1; cd "$HOME/.local/bin" || exit 1; if [ -n "$(find .thinkterm-relay-install.lock -maxdepth 0 -mmin +{stale} 2>/dev/null)" ]; then rm -rf .thinkterm-relay-install.lock; fi; if ! mkdir .thinkterm-relay-install.lock 2>/dev/null; then printf 'busy\n'; exit 75; fi; holder=$$; ( while sleep {beat}; do kill -0 "$holder" 2>/dev/null || exit 0; touch -c .thinkterm-relay-install.lock 2>/dev/null; done ) </dev/null >/dev/null 2>&1 & alive=$!; cleanup() {{ kill "$alive" 2>/dev/null; rm -f "{directory}/thinkterm" "{directory}/thinkterm-mux-server"; rmdir "{directory}" 2>/dev/null; rmdir .thinkterm-relay-install.lock; }}; trap cleanup EXIT; trap 'exit 129' HUP; trap 'exit 130' INT; trap 'exit 143' TERM; mkdir "{directory}" || exit 1; printf 'ready\n'; read -r release; exit 0"#,
        beat = LOCK_HEARTBEAT_SECS,
        stale = LOCK_STALE_MINUTES,
    )
}

impl InstallLock {
    fn acquire(session: &Session, page: &Page) -> anyhow::Result<Self> {
        page.attempt.check()?;
        let _deadline = page.attempt.deadline(COMMAND_TIMEOUT, LOCK_TIMED_OUT);
        let directory = format!(".thinkterm-relay-{}", uuid::Uuid::new_v4().simple());
        let command = page
            .attempt
            .wait(session.exec(&sh(&install_lock_script(&directory)), None))??;
        let mut lock = Self {
            directory,
            command: Some(command),
        };
        let mut reply = String::new();
        std::io::BufReader::new(&mut lock.command.as_mut().expect("lock command").stdout)
            .take(256)
            .read_line(&mut reply)?;
        page.attempt.check()?;
        match reply.trim() {
            "ready" => Ok(lock),
            "busy" => bail!("another ThinkTerm installation holds the remote install lock; try again after it finishes"),
            _ => bail!("could not acquire the remote ThinkTerm install lock"),
        }
    }

    /// Let go of the lock, without letting its trouble cost the work it
    /// guarded: a holder whose release fails is told to end anyway (see
    /// `Drop`), and a lock it leaves behind goes stale within minutes.
    fn let_go(self, page: &Page) {
        if self.release(page).is_err() {
            log::warn!("web relay: releasing the remote install lock failed");
        }
    }

    fn release(mut self, page: &Page) -> anyhow::Result<()> {
        let _deadline = page.attempt.deadline(COMMAND_TIMEOUT, LOCK_TIMED_OUT);
        let command = self.command.as_mut().expect("lock command");
        page.attempt.check()?;
        command.stdin.write_all(b"\n")?;
        let status = page.attempt.wait(command.child.async_wait())??;
        if !status.success() {
            bail!("releasing the remote ThinkTerm install lock failed");
        }
        self.command.take();
        Ok(())
    }
}

impl Drop for InstallLock {
    fn drop(&mut self) {
        if let Some(command) = self.command.as_mut() {
            // Never wait on a host while unwinding a cancelled attempt.
            let _ = command.child.kill();
            if command.stdin.set_non_blocking(true).is_ok() {
                let _ = command.stdin.write_all(b"\n");
            }
        }
    }
}

/// The running server there is older: hand its sessions to the one just
/// installed, or, failing that, stop it -- only if the person says so,
/// since that ends every session in it.
fn hand_over(session: &Session, page: &Page) -> Result<(), Failure> {
    page.step(Step::Starting);
    match run(session, &thinkterm_update::takeover_command(), page, true) {
        Ok(ran) if ran.ok => return Ok(()),
        Ok(_) => {}
        Err(err) => page.log(&format!("{err:#}")),
    }
    if !page.agrees(AskKind::StopServer, "") {
        return Err(Failure::new(
            Reason::Declined,
            "the old mux server there is still running and still speaks the old protocol",
        ));
    }
    let stopped = run(session, STOP_SERVER, page, true).map_err(Failure::failed)?;
    if !stopped.ok {
        page.log("Stopping the old server there did not succeed; its output is above.");
    }
    Ok(())
}

fn leads_back_here(info: &GetCodecVersionResponse) -> bool {
    Mux::try_get().is_some_and(|mux| mux.runtime_server_id() == info.server_id)
}

/// What the person's go-ahead covers so far in one `reach`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Consent {
    install: bool,
    replace: bool,
}

/// What the version question met there.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Met {
    /// No ThinkTerm there to ask.
    Nothing,
    /// A ThinkTerm of another protocol answered, `newer` than this
    /// server's or older.
    Other { newer: bool, version: String },
    /// The one there did not answer (`running` false, `why` what it said
    /// on its way out), or answered what this build cannot read (`running`
    /// true). `version` is what its `--version` said, when it said.
    Mute {
        running: bool,
        why: String,
        version: Option<String>,
    },
}

/// What a look at the host came to, short of a connection that works.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Plan {
    /// ThinkTerm there is the desktop app's: not ours to change.
    Refuse(String),
    /// This server can put no ThinkTerm there, so nothing is asked.
    Cannot(String),
    /// ThinkTerm there is already this server's version: changing it would
    /// change nothing, so what is wrong is not its version.
    Fail(String),
    /// Ask first: install where there is none, replace a newer one.
    Ask(AskKind, String),
    /// Change it: `running_old` hands the old server's sessions over.
    Install { updating: bool, running_old: bool },
}

/// The decision, apart from the asking: what the probe found, what the
/// version question met, this server's own version (`ours`), why nothing
/// can be installed there (`blocked`, see `install_blocker`), and what has
/// been agreed to.
fn plan(
    probe: &Probe,
    met: &Met,
    ours: &str,
    blocked: Option<&str>,
    consent: Consent,
) -> Plan {
    let remote = match met {
        Met::Nothing => None,
        Met::Other { version, .. } => Some(version.as_str()),
        Met::Mute { version, .. } => version.as_deref(),
    };
    if probe.desktop() {
        return Plan::Refuse(format!(
            "ThinkTerm on that machine is the desktop app{}; update it there to {ours}",
            remote.map(|v| format!(" ({v})")).unwrap_or_default(),
        ));
    }
    if let Met::Mute {
        running,
        why,
        version: Some(theirs),
    } = met
    {
        if theirs == ours {
            return Plan::Fail(if *running {
                format!(
                    "ThinkTerm there is {ours}, as here, but its answer could not be read; \
                     something there may be printing before it, a login script perhaps"
                )
            } else {
                format!("ThinkTerm there ({ours}) did not start: {why}")
            });
        }
    }
    if let Some(why) = blocked {
        return Plan::Cannot(why.to_string());
    }
    if probe.bin.is_none() && !consent.install {
        return Plan::Ask(AskKind::Install, format!("{} {}", probe.os, probe.arch));
    }
    let newer = match met {
        Met::Other { newer, .. } => *newer,
        // Older by its own word is ours to update. Newer, or a version
        // that does not compare with this one, is the person's to decide.
        Met::Mute {
            version: Some(theirs),
            ..
        } => !thinkterm_update::is_newer_release(ours, theirs),
        // One with nothing to say for itself is broken, and ours to mend.
        Met::Mute { version: None, .. } | Met::Nothing => false,
    };
    if newer && !consent.replace {
        return Plan::Ask(AskKind::Replace, remote.unwrap_or_default().to_string());
    }
    Plan::Install {
        updating: probe.bin.is_some(),
        running_old: matches!(met, Met::Other { .. } | Met::Mute { running: true, .. }),
    }
}

/// Everything between "open this machine" and its mux connection. The
/// questions come first, without the install lock: a person may take
/// minutes over one, and the lock keeps every other page off the host
/// meanwhile. Under the lock the host is looked at again -- another page
/// may have finished an install since -- before anything is changed.
fn reach(machine: &Machine, page: &Page) -> Result<Reached, Failure> {
    page.step(Step::Connecting);
    let session = authenticate(machine, page)?;
    let mut consent = Consent {
        install: machine.kept.install,
        replace: false,
    };
    let mut lock: Option<InstallLock> = None;
    // Past the sign-in, a plain ssh terminal is still worth offering
    // whatever goes wrong with ThinkTerm there.
    let failed = |err: anyhow::Error| Failure::failed(err).with_fallback();
    loop {
        page.attempt.check().map_err(Failure::failed)?;
        page.step(Step::Checking);
        let probe = run(&session, PROBE, page, false)
            .map(|ran| parse_probe(&ran.out))
            .map_err(failed)?;
        log::info!(
            "web relay: host probe finished (ThinkTerm found: {})",
            probe.bin.is_some()
        );

        let mut met = Met::Nothing;
        if let Some(command) = probe.proxy() {
            let (proxy, hello) = hello(&session, page, &command).map_err(failed)?;
            match hello {
                Hello::Speaks(info) => {
                    if leads_back_here(&info) {
                        return Err(Failure::new(
                            Reason::SameServer,
                            "this is the server the page is already connected to",
                        ));
                    }
                    if let Some(lock) = lock.take() {
                        lock.let_go(page);
                    }
                    return Ok(Reached {
                        session,
                        proxy,
                        version: info.version_string,
                    });
                }
                Hello::Other(info) => {
                    page.log(&format!(
                        "ThinkTerm there is {} (protocol {}); this server is {} (protocol {CODEC_VERSION})",
                        info.version_string,
                        info.codec_vers,
                        config::wezterm_version()
                    ));
                    met = Met::Other {
                        newer: info.codec_vers > CODEC_VERSION,
                        version: info.version_string,
                    };
                }
                Hello::Unreadable => {
                    page.log("ThinkTerm there answered in a way this server cannot read");
                    met = Met::Mute {
                        running: true,
                        why: String::new(),
                        version: None,
                    };
                }
                Hello::Silent(err) => {
                    page.log(&format!("ThinkTerm there did not start: {err}"));
                    met = Met::Mute {
                        running: false,
                        why: err,
                        version: None,
                    };
                }
            }
            // Its connection to the old server goes before that server is
            // asked to hand over.
            drop(proxy);
            // An answer that said nothing of its version: ask the program
            // itself, so an older one is updated, a newer one is asked
            // about, and this server's own is left alone.
            if let Met::Mute { version, .. } = &mut met {
                *version = probe
                    .version_command()
                    .and_then(|command| run(&session, &command, page, false).ok())
                    .and_then(|ran| version_in(&ran.out).map(str::to_string));
                if let Some(version) = version {
                    page.log(&format!("It says it is {version}"));
                }
            }
        }

        let blocked = install_blocker(&probe);
        let ours = config::wezterm_version();
        let (updating, running_old) = match plan(&probe, &met, ours, blocked.as_deref(), consent) {
            Plan::Refuse(message) => {
                return Err(Failure::new(Reason::Desktop, message).with_fallback());
            }
            Plan::Cannot(message) => {
                return Err(Failure::new(Reason::CannotInstall, message).with_fallback());
            }
            Plan::Fail(message) => {
                return Err(Failure::new(Reason::Failed, message).with_fallback());
            }
            Plan::Ask(kind, detail) => {
                if let Some(lock) = lock.take() {
                    lock.let_go(page);
                }
                if !page.agrees(kind, &detail) {
                    let message = match kind {
                        AskKind::Install => "ThinkTerm is not installed there".to_string(),
                        _ => format!("ThinkTerm there ({detail}) is newer than this server's"),
                    };
                    return Err(Failure::new(Reason::Declined, message).with_fallback());
                }
                match kind {
                    AskKind::Install => {
                        remember_install(machine);
                        consent.install = true;
                    }
                    _ => consent.replace = true,
                }
                continue;
            }
            Plan::Install {
                updating,
                running_old,
            } => (updating, running_old),
        };
        if lock.is_none() {
            lock = Some(InstallLock::acquire(&session, page).map_err(failed)?);
            continue;
        }

        let step = if updating {
            Step::Updating
        } else {
            Step::Installing
        };
        page.step(step);
        install(
            &session,
            &probe,
            page,
            step,
            lock.as_ref().expect("install lock"),
        )?;
        if running_old {
            hand_over(&session, page)?;
        }
        page.step(Step::Starting);
        let (proxy, hello) = hello(&session, page, PROXY_LOCAL).map_err(failed)?;
        // Past an install, trying again would only install again: what
        // goes wrong now is not worth a retry.
        let outcome = match hello {
            Hello::Speaks(info) if !leads_back_here(&info) => Ok(Reached {
                session,
                proxy,
                version: info.version_string,
            }),
            Hello::Speaks(_) => Err(Failure::new(
                Reason::SameServer,
                "this is the server the page is already connected to",
            )),
            Hello::Other(info) => Err(Failure::new(
                Reason::CannotInstall,
                format!(
                    "ThinkTerm there still answers as {} (protocol {}) after the update",
                    info.version_string, info.codec_vers
                ),
            )
            .with_fallback()),
            Hello::Unreadable => Err(Failure::new(
                Reason::CannotInstall,
                "ThinkTerm there still answers in a way this server cannot read after the \
                 update",
            )
            .with_fallback()),
            Hello::Silent(err) => Err(Failure::new(
                Reason::CannotInstall,
                format!("ThinkTerm there did not start: {err}"),
            )
            .with_fallback()),
        };
        lock.take().expect("install lock").let_go(page);
        return outcome;
    }
}

/// A plain ssh domain on this server for the machine, registered if it is
/// not there yet: what the page can still open where ThinkTerm is not.
fn plain_ssh_domain(machine: &Machine) -> Option<String> {
    let mux = Mux::try_get()?;
    let name = match machine.source {
        // `update_mux_domains` registered these by their alias.
        Source::SshConfig => format!("SSH:{}", machine.spec.host),
        // By id: a bare host name could be an alias's domain above.
        _ => format!("SSH:{}", machine.id),
    };
    if mux.get_domain_by_name(&name).is_some() {
        return Some(name);
    }
    let mut dom = thinkterm_core::ssh_hosts::build_ssh_domain(&machine.spec);
    dom.name = name.clone();
    dom.multiplexing = SshMultiplexing::None;
    // No password: a domain holds it in plain text for as long as this
    // server runs, Forget or not. The pane asks for one when it is needed.
    match RemoteSshDomain::with_ssh_domain(&dom) {
        Ok(domain) => {
            let domain: Arc<dyn Domain> = Arc::new(domain);
            mux.add_domain(&domain);
            Some(name)
        }
        Err(_) => {
            log::warn!("web relay: creating a plain ssh domain failed");
            None
        }
    }
}

// ---- the socket ------------------------------------------------------------------

enum Incoming {
    Text(String),
    Bytes(Vec<u8>),
}

struct Relay<S> {
    sender: Sender<S>,
    incoming: smol::channel::Receiver<Incoming>,
    attempt: Attempt,
    /// The address whose current configuration governs this connection.
    listener: String,
}

/// One relay socket, from its first message to its close. `listener` is
/// its bind address, used to read the current policy (see `relay_allowed`).
pub async fn serve<S>(socket: S, revoked: smol::channel::Receiver<()>, listener: String)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut builder = Builder::new(socket, Mode::Server);
    builder.set_max_message_size(MAX_MESSAGE);
    builder.set_max_frame_size(MAX_MESSAGE);
    let (sender, mut receiver) = builder.finish();
    let (tx, incoming) = smol::channel::bounded::<Incoming>(CHANNEL_DEPTH);
    let attempt = Attempt::new();
    let stop = StopAttempt(attempt.clone());
    let revoke_attempt = attempt.clone();
    let _revocation = crate::connections::spawn_task(async move {
        let _ = revoked.recv().await;
        revoke_attempt.stop("the web token was revoked");
    });
    let read_attempt = attempt.clone();
    // Read whole messages on a task of its own: a read cut short loses
    // its frame, so nothing may race it.
    let _reader = crate::connections::spawn_task(async move {
        let mut message = Vec::new();
        loop {
            message.clear();
            let item = match receiver.receive_data(&mut message).await {
                Ok(Data::Text(_)) => Incoming::Text(String::from_utf8_lossy(&message).into_owned()),
                Ok(Data::Binary(_)) if message.is_empty() => continue,
                Ok(Data::Binary(_)) => Incoming::Bytes(std::mem::take(&mut message)),
                Err(_) => {
                    log::debug!("web relay socket read ended");
                    read_attempt.stop("the page went away");
                    return;
                }
            };
            if tx.send(item).await.is_err() {
                return;
            }
        }
    });
    let mut relay = Relay {
        sender,
        incoming,
        attempt,
        listener,
    };
    if relay.run().await.is_err() {
        log::debug!("web relay ended with an error");
    }
    drop(stop);
    smol::future::or(
        async {
            let _ = relay.sender.close().await;
        },
        async {
            smol::Timer::after(CLOSE_GRACE).await;
        },
    )
    .await;
}

impl<S> Relay<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    async fn reply(&mut self, reply: &Reply) -> anyhow::Result<()> {
        let text = serde_json::to_string(reply)?;
        let _deadline = self
            .attempt
            .stop_after(COMMAND_TIMEOUT, "the page stopped reading");
        let sender = &mut self.sender;
        self.attempt
            .until_stopped(async {
                sender.send_text(text).await?;
                sender.flush().await
            })
            .await??;
        Ok(())
    }

    /// The next message, or None once the page or its token has gone.
    async fn next(&self) -> Option<Incoming> {
        self.attempt
            .until_stopped(self.incoming.recv())
            .await
            .ok()?
            .ok()
    }

    async fn run(&mut self) -> anyhow::Result<()> {
        loop {
            let text = match self.next().await {
                None => return Ok(()),
                Some(Incoming::Text(text)) => text,
                Some(Incoming::Bytes(_)) => bail!("bytes before a machine was opened"),
            };
            let request = match serde_json::from_str::<Request>(&text) {
                Ok(request) => request,
                Err(err) => {
                    self.reply(&Reply::Error {
                        message: format!("not a request: {err}"),
                    })
                    .await?;
                    continue;
                }
            };
            let listener = self.listener.clone();
            let enabled = smol::unblock(move || relay_allowed(&listener)).await;
            if !enabled {
                // Off: no machine is listed, saved, forgotten or opened. The
                // tab icon cards are this machine's own and still answered.
                match &request {
                    Request::Open { .. } => {
                        return self
                            .reply(&Reply::Failed {
                                reason: Reason::Off,
                                message: OFF.to_string(),
                                ssh_domain: None,
                            })
                            .await;
                    }
                    Request::Save { .. } | Request::Forget { .. } => {
                        self.reply(&Reply::Error {
                            message: OFF.to_string(),
                        })
                        .await?;
                        continue;
                    }
                    Request::List | Request::TabIcons | Request::Answer { .. } => {}
                }
            }
            match request {
                Request::List => {}
                Request::Save { machine } => match smol::unblock(move || save_machine(machine)).await {
                    Ok(id) => self.reply(&Reply::Saved { id }).await?,
                    Err(err) => {
                        self.reply(&Reply::Error {
                            message: format!("{err:#}"),
                        })
                        .await?;
                        continue;
                    }
                },
                Request::Forget { id } => {
                    if let Err(err) = smol::unblock(move || forget_machine(&id)).await {
                        self.reply(&Reply::Error {
                            message: format!("{err:#}"),
                        })
                        .await?;
                        continue;
                    }
                }
                Request::Open { id } => return self.open(&id).await,
                Request::TabIcons => {
                    let icons = smol::unblock(tab_icons_reply).await;
                    self.reply(&icons).await?;
                    continue;
                }
                // Nothing was asked.
                Request::Answer { .. } => continue,
            }
            let list = smol::unblock(move || machines_reply(enabled)).await;
            self.reply(&list).await?;
        }
    }

    async fn open(&mut self, id: &str) -> anyhow::Result<()> {
        // For as long as the machine is open, through its questions and
        // then its bytes: turned off meanwhile, it ends.
        let _switch = watch_switch(&self.attempt, self.listener.clone());
        let all = smol::unblock(machines).await;
        let Some(machine) = all.into_iter().find(|m| m.id == id) else {
            return self
                .reply(&Reply::Failed {
                    reason: Reason::Gone,
                    message: format!("there is no machine {id} on this server"),
                    ssh_domain: None,
                })
                .await;
        };
        let (worker_tx, worker_rx) = smol::channel::bounded::<Worker>(CHANNEL_DEPTH);
        let (answer_tx, answers) = smol::channel::bounded::<Answered>(CHANNEL_DEPTH);
        let attempt = self.attempt.clone();
        std::thread::Builder::new()
            .name("web-relay".into())
            .spawn(move || {
                let page = Page {
                    attempt,
                    tx: worker_tx,
                    answers,
                    next_ask: std::cell::Cell::new(1),
                };
                let reached = reach(&machine, &page).map_err(|mut failure| {
                    // A step that ran out of time is why, whatever the
                    // work hit after it: a closed channel, or a question
                    // that reads as dismissed but went unanswered.
                    if let Some(why) = page.attempt.halted() {
                        failure.message = why.to_string();
                        if failure.reason == Reason::Cancelled {
                            failure.reason = Reason::Failed;
                        }
                    }
                    log::warn!("web relay connection failed: {:?}", failure.reason);
                    if failure.fallback && page.attempt.check_page().is_ok() {
                        failure.ssh_domain = plain_ssh_domain(&machine);
                    }
                    failure
                });
                // A page that went away meanwhile drops this, and with it
                // the session.
                let _ = page.tx.send_blocking(Worker::Done(reached));
            })?;

        enum Event {
            Worker(Option<Worker>),
            Page(Option<Incoming>),
        }
        let reached = loop {
            let event = smol::future::or(
                async { Event::Worker(worker_rx.recv().await.ok()) },
                async { Event::Page(self.next().await) },
            )
            .await;
            match event {
                Event::Worker(Some(Worker::Say(reply))) => self.reply(&reply).await?,
                Event::Worker(Some(Worker::Done(Ok(reached)))) => break reached,
                Event::Worker(Some(Worker::Done(Err(failure)))) => {
                    return self
                        .reply(&Reply::Failed {
                            reason: failure.reason,
                            message: failure.message,
                            ssh_domain: failure.ssh_domain,
                        })
                        .await;
                }
                Event::Worker(None) => bail!("the relay worker went away"),
                Event::Page(Some(Incoming::Text(text))) => {
                    if let Ok(Request::Answer {
                        id,
                        value,
                        remember,
                    }) = serde_json::from_str(&text)
                    {
                        let _ = answer_tx.try_send(Answered {
                            id,
                            value,
                            remember,
                        });
                    }
                }
                Event::Page(Some(Incoming::Bytes(_))) => {
                    bail!("bytes before the machine was reached")
                }
                // The reader or revocation watcher has interrupted the
                // SSH transport too, including a worker blocked in IO.
                Event::Page(None) => return Ok(()),
            }
        };
        drop(answer_tx);
        self.reply(&Reply::Ready {
            version: reached.version.clone(),
        })
        .await?;
        log::info!("web relay is carrying its mux connection");
        self.carry(reached).await
    }

    /// Carry bytes both ways until either end stops.
    async fn carry(&mut self, reached: Reached) -> anyhow::Result<()> {
        let Reached { session, proxy, .. } = reached;
        let Proxy {
            mut stdin,
            mut stdout,
            child,
            stderr,
        } = proxy;
        let (to_host, host_rx) = smol::channel::bounded::<Vec<u8>>(CHANNEL_DEPTH);
        let (host_tx, from_host) = smol::channel::bounded::<Vec<u8>>(CHANNEL_DEPTH);
        std::thread::Builder::new()
            .name("web-relay-in".into())
            .spawn(move || {
                while let Ok(chunk) = host_rx.recv_blocking() {
                    if stdin
                        .write_all(&chunk)
                        .and_then(|()| stdin.flush())
                        .is_err()
                    {
                        break;
                    }
                }
                // Dropping our end of the input closes the channel, which
                // is what ends the host's proxy.
            })?;
        std::thread::Builder::new()
            .name("web-relay-out".into())
            .spawn(move || {
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    match stdout.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if host_tx.send_blocking(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                    }
                }
            })?;

        enum Event {
            Page(Option<Incoming>),
            Host(Option<Vec<u8>>),
        }
        // Each side gets the first look in turn: a host flooding output
        // must not keep the page's keys (Ctrl-C among them) waiting behind
        // it, nor a busy page the host's output.
        let mut host_first = true;
        let watchdog = Watchdog::new(&self.attempt, COMMAND_TIMEOUT, "a send stalled for a minute");
        let ended = loop {
            host_first = !host_first;
            let host = async { Event::Host(from_host.recv().await.ok()) };
            let page = async { Event::Page(self.next().await) };
            let event = if host_first {
                smol::future::or(host, page).await
            } else {
                smol::future::or(page, host).await
            };
            match event {
                Event::Page(Some(Incoming::Bytes(bytes))) => {
                    watchdog.arm();
                    let sent = self.attempt.until_stopped(to_host.send(bytes)).await?;
                    watchdog.done();
                    if sent.is_err() {
                        break "the host's input closed";
                    }
                }
                Event::Page(Some(Incoming::Text(_))) => {
                    break "the page sent text where bytes belong"
                }
                Event::Page(None) => break "the page went away",
                Event::Host(Some(bytes)) => {
                    watchdog.arm();
                    let sender = &mut self.sender;
                    self.attempt
                        .until_stopped(async {
                            sender.send_binary(&bytes).await?;
                            sender.flush().await
                        })
                        .await??;
                    watchdog.done();
                }
                Event::Host(None) => {
                    break if stderr.text().is_empty() {
                        "the host's mux connection ended"
                    } else {
                        "the host's mux connection ended with error output"
                    };
                }
            }
        };
        log::info!("web relay finished: {ended}");
        drop(to_host);
        // Let go of the host off this thread: a session's goodbye waits on
        // its own thread.
        let _ = std::thread::Builder::new()
            .name("web-relay-close".into())
            .spawn(move || drop((child, session)));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_an_attempt_wakes_a_blocked_worker() {
        let attempt = Attempt::new();
        let worker_attempt = attempt.clone();
        let (sent, received) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = worker_attempt.wait(std::future::pending::<()>());
            sent.send(result.is_err()).unwrap();
        });
        drop(StopAttempt(attempt.clone()));
        assert!(received.recv_timeout(Duration::from_secs(1)).unwrap());
        assert!(attempt.check().is_err());
        worker.join().unwrap();
    }

    #[test]
    fn a_stopped_attempt_never_starts_the_next_operation() {
        let attempt = Attempt::new();
        attempt.stop("cancelled");
        let ran = std::cell::Cell::new(false);
        assert!(attempt.wait(async { ran.set(true) }).is_err());
        assert!(!ran.get());
    }

    #[test]
    fn deadlines_stop_stalled_work_but_disarm_on_completion() {
        let attempt = Attempt::new();
        let deadline = attempt.deadline(Duration::from_millis(10), "it took too long");
        assert!(attempt
            .wait(smol::Timer::after(Duration::from_secs(1)))
            .is_err());
        assert_eq!(attempt.check().unwrap_err().to_string(), "it took too long");
        assert_eq!(attempt.halted(), Some("it took too long"));
        // The socket is still the page's, to tell it why.
        assert!(attempt.check_page().is_ok());
        assert!(smol::block_on(attempt.until_stopped(async { 1 })).is_ok());
        drop(deadline);

        let attempt = Attempt::new();
        drop(attempt.deadline(Duration::from_millis(10), "it took too long"));
        assert!(attempt
            .wait(smol::Timer::after(Duration::from_millis(30)))
            .is_ok());
        assert_eq!(attempt.halted(), None);
    }

    #[test]
    fn a_stopped_relay_ends_both_sides() {
        let attempt = Attempt::new();
        attempt.stop("the page went away");
        assert!(attempt.check().is_err() && attempt.check_page().is_err());
        assert!(smol::block_on(attempt.until_stopped(async { 1 })).is_err());
        assert_eq!(attempt.halted(), None, "a stop is not a step that ran out of time");
    }

    #[test]
    fn a_stalled_send_stops_the_relay_and_a_finished_one_does_not() {
        let attempt = Attempt::new();
        let watchdog = Watchdog::new(&attempt, Duration::from_millis(40), "stalled");
        watchdog.arm();
        watchdog.done();
        smol::block_on(smol::Timer::after(Duration::from_millis(80)));
        assert!(attempt.check_page().is_ok());
        watchdog.arm();
        smol::block_on(smol::Timer::after(Duration::from_millis(120)));
        assert_eq!(attempt.check_page().unwrap_err().to_string(), "stalled");
    }

    #[test]
    fn only_a_refusal_is_a_failed_sign_in() {
        assert!(refused("authentication: password auth status: Denied"));
        assert!(refused("authentication: unhandled auth case; methods=[], status={}"));
        assert!(refused("authentication: [Session(-18)] Authentication failed (username/password)"));
        // The network giving way under a kept password is worth another try.
        assert!(!refused("authentication: [Session(-7)] Unable to send userauth-password request"));
        assert!(!refused("authentication: Connection reset by peer (os error 54)"));
        assert!(!refused("ssh handshake with server-a:22: timed out"));
    }

    #[cfg(unix)]
    #[test]
    fn remote_install_lock_excludes_other_attempts_and_cleans_up_on_eof() {
        use std::process::{Command, Stdio};

        struct Holder(std::process::Child);
        impl Drop for Holder {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        impl Holder {
            fn start(home: &Path, directory: &str) -> (Self, String) {
                let mut child = Command::new("sh")
                    .args(["-c", &install_lock_script(directory)])
                    .env("HOME", home)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap();
                let mut reply = String::new();
                std::io::BufReader::new(child.stdout.take().unwrap())
                    .read_line(&mut reply)
                    .unwrap();
                (Self(child), reply.trim().to_string())
            }

            fn finish(&mut self) -> std::process::ExitStatus {
                self.0.stdin.take();
                self.wait()
            }

            fn wait(&mut self) -> std::process::ExitStatus {
                for _ in 0..200 {
                    if let Some(status) = self.0.try_wait().unwrap() {
                        return status;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                panic!("install lock did not release after EOF");
            }
        }

        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join(".local/bin");
        let (mut first, reply) = Holder::start(home.path(), ".thinkterm-relay-first");
        assert_eq!(reply, "ready");
        let staged = bin.join(".thinkterm-relay-first/thinkterm");
        std::fs::write(&staged, b"first upload").unwrap();
        let (mut second, reply) = Holder::start(home.path(), ".thinkterm-relay-second");
        assert_eq!(reply, "busy");
        assert_eq!(second.finish().code(), Some(75));
        assert_eq!(std::fs::read(&staged).unwrap(), b"first upload");
        assert!(!bin.join(".thinkterm-relay-second").exists());
        assert!(first.finish().success());
        assert!(!bin.join(".thinkterm-relay-install.lock").exists());
        assert!(!bin.join(".thinkterm-relay-first").exists());

        let (mut next, reply) = Holder::start(home.path(), ".thinkterm-relay-next");
        assert_eq!(reply, "ready");
        assert!(Command::new("kill")
            .args(["-HUP", &next.0.id().to_string()])
            .status()
            .unwrap()
            .success());
        assert_eq!(next.wait().code(), Some(129));
        assert_eq!(std::fs::read_dir(&bin).unwrap().count(), 0);

        // A lock touched lately is held; one its holder stopped touching
        // minutes ago (killed, or the host rebooted) is broken.
        let lock = bin.join(".thinkterm-relay-install.lock");
        std::fs::create_dir(&lock).unwrap();
        let (mut fresh, reply) = Holder::start(home.path(), ".thinkterm-relay-fresh");
        assert_eq!(reply, "busy");
        assert_eq!(fresh.finish().code(), Some(75));
        assert!(Command::new("touch")
            .args(["-t", "202001010000"])
            .arg(&lock)
            .status()
            .unwrap()
            .success());
        let (mut stale, reply) = Holder::start(home.path(), ".thinkterm-relay-stale");
        assert_eq!(reply, "ready");
        assert!(stale.finish().success());
        assert!(!lock.exists());
    }

    fn spec(label: &str, host: &str) -> SshHostSpec {
        serde_json::from_value(serde_json::json!({"label": label, "host": host})).unwrap()
    }

    #[test]
    fn requests_read_as_the_page_writes_them() {
        assert!(matches!(
            serde_json::from_str::<Request>(r#"{"op":"list"}"#).unwrap(),
            Request::List
        ));
        match serde_json::from_str::<Request>(
            r#"{"op":"answer","id":3,"value":"yes","remember":true}"#,
        )
        .unwrap()
        {
            Request::Answer {
                id,
                value,
                remember,
            } => {
                assert_eq!((id, value.as_deref(), remember), (3, Some("yes"), true));
            }
            other => panic!("{:?}", other),
        }
        // A dismissed question carries no value.
        match serde_json::from_str::<Request>(r#"{"op":"answer","id":4}"#).unwrap() {
            Request::Answer { value, .. } => assert_eq!(value, None),
            other => panic!("{:?}", other),
        }
        match serde_json::from_str::<Request>(
            r#"{"op":"save","machine":{"host":"server-a","port":2222,"user":"user"}}"#,
        )
        .unwrap()
        {
            Request::Save { machine } => {
                assert_eq!(machine.host, "server-a");
                assert_eq!(machine.port, Some(2222));
            }
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn reaching_other_machines_is_off_until_turned_on() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        // No desktop here, or one that never touched the switch: off.
        assert!(!desktop_allows_at(&settings));
        std::fs::write(&settings, r#"{"theme":"x","web":{"reachable":true}}"#).unwrap();
        assert!(!desktop_allows_at(&settings));
        std::fs::write(&settings, r#"{"theme":"x","web":{"reachable":true,"relay":true}}"#).unwrap();
        assert!(desktop_allows_at(&settings));
        // A file that does not read is no go-ahead.
        std::fs::write(&settings, "{").unwrap();
        assert!(!desktop_allows_at(&settings));
        // Off, the page hears so, and of no machine.
        let text = serde_json::to_string(&Reply::Machines {
            here: "server-a".into(),
            enabled: false,
            machines: vec![],
        })
        .unwrap();
        assert_eq!(text, r#"{"op":"machines","here":"server-a","enabled":false,"machines":[]}"#);
        assert_eq!(serde_json::to_string(&Reason::Off).unwrap(), r#""off""#);
    }

    #[test]
    fn relay_policy_tracks_each_listener_and_the_desktop_switch() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        let mut servers = vec![
            config::WebServer {
                bind_address: "server-a:8088".into(),
                relay: true,
                ..Default::default()
            },
            config::WebServer {
                bind_address: "server-b:8088".into(),
                ..Default::default()
            },
        ];
        assert!(relay_allowed_at(&servers, "server-a:8088", &settings));
        assert!(!relay_allowed_at(&servers, "server-b:8088", &settings));
        assert!(!relay_allowed_at(&servers, "server-c:8088", &settings));

        // The desktop switch can change the other listeners independently.
        std::fs::write(&settings, r#"{"web":{"relay":true}}"#).unwrap();
        assert!(relay_allowed_at(&servers, "server-b:8088", &settings));
        assert!(relay_allowed_at(&servers, "server-c:8088", &settings));
        std::fs::write(&settings, r#"{"web":{"relay":false}}"#).unwrap();
        assert!(relay_allowed_at(&servers, "server-a:8088", &settings));
        assert!(!relay_allowed_at(&servers, "server-b:8088", &settings));

        // A reloaded entry, or its removal, must replace the startup policy.
        servers[0].relay = false;
        assert!(!relay_allowed_at(&servers, "server-a:8088", &settings));
        servers[1].relay = true;
        assert!(relay_allowed_at(&servers, "server-b:8088", &settings));
        servers.clear();
        assert!(!relay_allowed_at(&servers, "server-b:8088", &settings));
    }

    #[test]
    fn replies_are_tagged_for_the_page() {
        let text = serde_json::to_string(&Reply::Step {
            step: Step::Installing,
            percent: Some(40),
        })
        .unwrap();
        assert_eq!(text, r#"{"op":"step","step":"installing","percent":40}"#);
        let text = serde_json::to_string(&Reply::Step {
            step: Step::Checking,
            percent: None,
        })
        .unwrap();
        assert_eq!(text, r#"{"op":"step","step":"checking"}"#);
        let text = serde_json::to_string(&Reply::Ask(Ask {
            id: 2,
            kind: AskKind::HostKey,
            prompt: String::new(),
            detail: "fingerprint".into(),
            remember: false,
        }))
        .unwrap();
        assert_eq!(
            text,
            r#"{"op":"ask","id":2,"kind":"host-key","prompt":"","detail":"fingerprint","remember":false}"#
        );
        let text = serde_json::to_string(&Reply::Failed {
            reason: Reason::CannotInstall,
            message: "no".into(),
            ssh_domain: Some("SSH:server-a".into()),
        })
        .unwrap();
        assert_eq!(
            text,
            r#"{"op":"failed","reason":"cannot-install","message":"no","ssh_domain":"SSH:server-a"}"#
        );
    }

    #[test]
    fn a_probe_reads_its_lines() {
        let probe = parse_probe(
            "os=Linux\narch=aarch64\nlibc=glibc 2.36\nbin=/home/user/.local/bin/thinkterm\n",
        );
        assert_eq!(
            probe,
            Probe {
                os: "Linux".into(),
                arch: "aarch64".into(),
                libc: "glibc 2.36".into(),
                bin: Some("/home/user/.local/bin/thinkterm".into()),
                ..Probe::default()
            }
        );
        // Nothing found: an empty value is no binary, and no variant.
        let none = parse_probe("os=Linux\nbin=\nvariant=\n");
        assert_eq!((none.bin, none.variant), (None, None));
        let desktop = parse_probe("bin=/home/user/.local/bin/thinkterm\ngui=1\nvariant=desktop\nserver=1\n");
        assert!(desktop.gui && desktop.server);
        assert_eq!(desktop.variant.as_deref(), Some("desktop"));
        // A banner a login shell printed first is not a key.
        assert_eq!(parse_probe("Welcome!\nos=Darwin\n").os, "Darwin");
    }

    #[test]
    fn our_programs_run_only_where_the_platform_is_ours() {
        let other = Probe {
            os: "FreeBSD".into(),
            ..Probe::default()
        };
        assert!(!other.runs_ours());
        let os = match std::env::consts::OS {
            "macos" => "Darwin",
            "linux" => "Linux",
            // This server's programs are sent nowhere from here.
            _ => {
                for os in ["Linux", "Darwin"] {
                    let probe = Probe {
                        os: os.into(),
                        arch: std::env::consts::ARCH.into(),
                        libc: "glibc 9.99".into(),
                        ..Probe::default()
                    };
                    assert!(!probe.runs_ours());
                }
                return;
            }
        };
        // On Linux, a host with this server's glibc or newer.
        let glibc = local_glibc();
        if os == "Linux" && glibc.is_none() {
            return;
        }
        let (major, minor) = glibc.unwrap_or_default();
        let here = Probe {
            os: os.into(),
            arch: std::env::consts::ARCH.into(),
            libc: format!("glibc {major}.{minor}"),
            ..Probe::default()
        };
        assert!(here.runs_ours());
        let musl = Probe {
            libc: "musl libc (x86_64)".into(),
            ..here.clone()
        };
        assert_eq!(musl.runs_ours(), os != "Linux");
        if os == "Linux" && minor > 0 {
            let older = Probe {
                libc: format!("glibc {major}.{}", minor - 1),
                ..here.clone()
            };
            assert!(!older.runs_ours());
        }
        let elsewhere = Probe {
            arch: "riscv64".into(),
            ..here
        };
        assert!(!elsewhere.runs_ours());
    }

    #[test]
    fn a_version_is_read_whole() {
        assert_eq!(version_in("thinkterm 0.1.10\n"), Some("0.1.10"));
        assert_ne!(version_in("thinkterm 0.1.10\n"), Some("0.1.1"));
        // A login script's words around it are not it.
        assert_eq!(
            version_in("Welcome to server-a\nthinkterm 20261001-000000-abcdef01\n"),
            Some("20261001-000000-abcdef01")
        );
        assert_eq!(version_in("sh: thinkterm: not found\n"), None);
    }

    #[test]
    fn scripts_survive_any_login_shell() {
        assert_eq!(sh("echo 'a'"), r#"sh -c 'echo '\''a'\'''"#);
        assert!(sh_no_input("true").ends_with(" </dev/null"));
        // The proxy reads the page's bytes: never from /dev/null.
        assert!(!sh(PROXY_LOCAL).contains("/dev/null"));
        // A path is passed as one word, whatever it holds.
        let probe = Probe {
            bin: Some("/opt/it's here/thinkterm".into()),
            ..Probe::default()
        };
        assert_eq!(
            probe.proxy().unwrap(),
            r#"exec '/opt/it'\''s here/thinkterm' cli --prefer-mux proxy"#
        );
        // csh cannot carry a newline inside `sh -c '…'`.
        assert!(!PROBE.contains('\n') && !install_lock_script(".x").contains('\n'));
        // Stopping a server names that one server, not every process
        // that shares its name.
        assert!(!STOP_SERVER.contains("pkill") && STOP_SERVER.contains("stop-server"));
    }

    #[test]
    fn a_prompt_becomes_one_clean_line() {
        assert_eq!(
            clean_prompt("Passphrase to decrypt /home/user/.ssh/id for user@server-a:\n> "),
            "Passphrase to decrypt /home/user/.ssh/id for user@server-a:"
        );
        assert_eq!(clean_prompt("  Password: "), "Password:");
    }

    #[test]
    fn only_a_password_or_a_passphrase_is_answered_from_what_is_kept() {
        assert_eq!(secret_kind("Password: "), Some(Secret::Password));
        assert_eq!(secret_kind("(user@server-a) Password:"), Some(Secret::Password));
        assert_eq!(
            secret_kind("Passphrase to decrypt /home/user/.ssh/id for user@server-a:"),
            Some(Secret::Passphrase)
        );
        assert_eq!(secret_kind("Verification code: "), None);
    }

    #[test]
    fn the_version_answer_is_found_behind_pushes() {
        let mut stream = Vec::new();
        // A push a busy host sends first, then the answer to request 1.
        Pdu::GetCodecVersion(GetCodecVersion {})
            .encode(&mut stream, 0)
            .unwrap();
        let answer = GetCodecVersionResponse {
            codec_vers: CODEC_VERSION,
            version_string: "test".into(),
            server_id: "server".into(),
            executable_path: "/x".into(),
            config_file_path: None,
        };
        Pdu::GetCodecVersionResponse(answer).encode(&mut stream, 1).unwrap();
        let frame = read_answer(&mut stream.as_slice()).unwrap();
        match Pdu::decode(std::io::Cursor::new(frame)).unwrap().pdu {
            Pdu::GetCodecVersionResponse(info) => assert_eq!(info.version_string, "test"),
            other => panic!("{:?}", other),
        }
        // A stream that ends first is an error, not an answer.
        let mut push_only = Vec::new();
        Pdu::GetCodecVersion(GetCodecVersion {})
            .encode(&mut push_only, 0)
            .unwrap();
        assert!(read_answer(&mut push_only.as_slice()).is_err());
    }

    const OURS: &str = "20261001-000000-aaaaaaaa";

    fn other(newer: bool) -> Met {
        Met::Other {
            newer,
            version: if newer { "new" } else { "old" }.into(),
        }
    }

    #[test]
    fn a_desktop_install_is_never_replaced_and_questions_come_first() {
        let consent = Consent::default();
        let ours = Probe {
            os: "Linux".into(),
            arch: "x86_64".into(),
            bin: Some("/home/user/.local/bin/thinkterm".into()),
            ..Probe::default()
        };
        // Our own earlier copy, an older protocol: updated without asking.
        assert_eq!(
            plan(&ours, &other(false), OURS, None, consent),
            Plan::Install {
                updating: true,
                running_old: true
            }
        );
        // Newer there: asked first.
        assert!(matches!(
            plan(&ours, &other(true), OURS, None, consent),
            Plan::Ask(AskKind::Replace, _)
        ));
        // Nothing there: asked once, then installed.
        let empty = Probe {
            bin: None,
            ..ours.clone()
        };
        assert!(matches!(
            plan(&empty, &Met::Nothing, OURS, None, consent),
            Plan::Ask(AskKind::Install, _)
        ));
        let agreed = Consent {
            install: true,
            replace: false,
        };
        assert_eq!(
            plan(&empty, &Met::Nothing, OURS, None, agreed),
            Plan::Install {
                updating: false,
                running_old: false
            }
        );
        // Nothing that can be put there: said at once, not asked first.
        assert_eq!(
            plan(&empty, &Met::Nothing, OURS, Some("no release for armv7l"), consent),
            Plan::Cannot("no release for armv7l".into())
        );
        // The desktop's, however it shows: refused, whatever was agreed.
        let everything = Consent {
            install: true,
            replace: true,
        };
        for desktop in [
            Probe { gui: true, ..ours.clone() },
            Probe { variant: Some("desktop".into()), ..ours.clone() },
            Probe {
                bin: Some("/Applications/ThinkTerm.app/Contents/MacOS/thinkterm".into()),
                ..ours.clone()
            },
            // A session server with no program to be found: the app's.
            Probe { server: true, ..empty.clone() },
        ] {
            assert!(
                matches!(
                    plan(&desktop, &other(false), OURS, None, everything),
                    Plan::Refuse(_)
                ),
                "{:?}",
                desktop
            );
        }
    }

    #[test]
    fn a_thinkterm_that_says_nothing_is_asked_its_version() {
        let consent = Consent::default();
        let there = Probe {
            os: "Linux".into(),
            arch: "x86_64".into(),
            bin: Some("/home/user/.local/bin/thinkterm".into()),
            ..Probe::default()
        };
        let mute = |running: bool, version: Option<&str>| Met::Mute {
            running,
            why: "it exited".into(),
            version: version.map(str::to_string),
        };
        // This server's own version: installing it again would change
        // nothing, so nothing is installed.
        assert!(matches!(
            plan(&there, &mute(true, Some(OURS)), OURS, None, consent),
            Plan::Fail(_)
        ));
        assert!(matches!(
            plan(&there, &mute(false, Some(OURS)), OURS, None, consent),
            Plan::Fail(message) if message.contains("it exited")
        ));
        // Newer, or of another scheme: the person's to decide.
        for theirs in ["20271001-000000-bbbbbbbb", "0.2.0"] {
            assert!(matches!(
                plan(&there, &mute(false, Some(theirs)), OURS, None, consent),
                Plan::Ask(AskKind::Replace, detail) if detail == theirs
            ));
        }
        // Older by its own word, or with nothing to say: ours to update.
        assert_eq!(
            plan(&there, &mute(true, Some("20250101-000000-cccccccc")), OURS, None, consent),
            Plan::Install {
                updating: true,
                running_old: true
            }
        );
        assert_eq!(
            plan(&there, &mute(false, None), OURS, None, consent),
            Plan::Install {
                updating: true,
                running_old: false
            }
        );
    }

    #[test]
    fn a_release_is_refused_where_install_sh_would_refuse_it() {
        let linux = |arch: &str, libc: &str| Probe {
            os: "Linux".into(),
            arch: arch.into(),
            libc: libc.into(),
            ..Probe::default()
        };
        assert_eq!(linux("x86_64", "glibc 2.39").cannot_take_a_release(), None);
        assert!(linux("armv7l", "glibc 2.39").cannot_take_a_release().is_some());
        assert!(linux("x86_64", "musl libc (x86_64)").cannot_take_a_release().is_some());
        assert!(linux("aarch64", "glibc 2.31").cannot_take_a_release().is_some());
        let mac = Probe {
            os: "Darwin".into(),
            arch: "arm64".into(),
            ..Probe::default()
        };
        assert_eq!(mac.cannot_take_a_release(), None);
    }

    #[test]
    fn a_typed_machine_is_checked_and_keyed_by_its_address() {
        let (id, spec) = new_machine_spec(&NewMachine {
            label: None,
            host: " server-a ".into(),
            port: Some(22),
            user: Some("user".into()),
            password: None,
        })
        .unwrap();
        assert!(id.starts_with("web-"));
        assert_eq!(spec.label, "server-a");
        assert_eq!(spec.port, None, "22 is the default and is left out");
        assert_eq!(thinkterm_core::ssh_hosts::endpoint(&spec), "user@server-a");
        let (again, _) = new_machine_spec(&NewMachine {
            label: Some("Build box".into()),
            host: "server-a".into(),
            port: None,
            user: Some("user".into()),
            password: None,
        })
        .unwrap();
        assert_eq!(id, again, "the same address is the same machine");

        for host in ["", "-oProxyCommand=x", "two words"] {
            assert!(
                new_machine_spec(&NewMachine {
                    label: None,
                    host: host.into(),
                    port: None,
                    user: None,
                    password: None,
                })
                .is_err(),
                "{:?}",
                host
            );
        }
        assert!(new_machine_spec(&NewMachine {
            label: None,
            host: "server-a".into(),
            port: None,
            user: Some("a@b".into()),
            password: None,
        })
        .is_err());
    }

    #[test]
    fn the_list_joins_the_sources_and_keeps_what_was_kept() {
        let kept = KeptFile {
            version: 1,
            machines: vec![
                Kept {
                    id: "web-1".into(),
                    host: Some(spec("devbox", "devbox.example")),
                    ..Kept::default()
                },
                Kept {
                    id: "system-ssh-a".into(),
                    password: Some("enc:v1:x".into()),
                    install: true,
                    address: Some("user@server-a.example:22".into()),
                    ..Kept::default()
                },
                Kept {
                    id: "system-ssh-moved".into(),
                    password: Some("enc:v1:y".into()),
                    install: true,
                    address: Some("user@old.example:22".into()),
                    ..Kept::default()
                },
            ],
        };
        let system = vec![
            SshHostEntry {
                id: "system-ssh-a".into(),
                source: SshHostSource::System,
                spec: spec("server-a", "server-a"),
            },
            // Its alias now leads to another machine.
            SshHostEntry {
                id: "system-ssh-moved".into(),
                source: SshHostSource::System,
                spec: spec("moved", "moved"),
            },
        ];
        let saved = vec![SshHostEntry {
            id: "saved-b".into(),
            source: SshHostSource::ThinkTerm,
            spec: spec("Build", "build.example"),
        }];
        let list = machines_from(&kept, saved, system, |spec| match spec.host.as_str() {
            "server-a" => "user@server-a.example:22".to_string(),
            _ => "user@new.example:22".to_string(),
        });
        let labels: Vec<_> = list.iter().map(|m| m.spec.label.as_str()).collect();
        assert_eq!(labels, ["Build", "devbox", "moved", "server-a"]);
        let moved = list.iter().find(|m| m.id == "system-ssh-moved").unwrap();
        assert!(
            !moved.kept.install && moved.kept.password.is_none(),
            "what was kept for the old address is not used for the new one"
        );
        let a = list.iter().find(|m| m.id == "system-ssh-a").unwrap();
        assert_eq!(a.source, Source::SshConfig);
        assert!(a.kept.install);
        assert!(a.view().password);
        assert!(a.view().forgettable);
        let saved = list.iter().find(|m| m.id == "saved-b").unwrap();
        assert!(!saved.view().forgettable, "nothing of it is kept here");
        let web = list.iter().find(|m| m.id == "web-1").unwrap();
        assert_eq!(web.source, Source::Web);
        assert!(!web.view().password);
    }

    #[test]
    fn a_page_gets_this_machines_tab_icons() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        // No desktop here: the built-in cards.
        let built_in = tab_icons_at(&settings);
        assert_eq!(built_in.terminal, "terminal");
        assert!(built_in.cards.iter().any(|card| card.id == "claude"));
        // The desktop's own card, with its imported glyph.
        let hash = "c".repeat(64);
        std::fs::write(
            &settings,
            format!(
                r##"{{"theme":"x","tab_icons":{{"cards":[{{"id":"custom-1","programs":["deploy"],"svg":"{hash}","circle":"#112233"}}]}}}}"##
            ),
        )
        .unwrap();
        std::fs::create_dir(dir.path().join("tab-icons")).unwrap();
        std::fs::write(dir.path().join("tab-icons").join(format!("{hash}.svg")), "<svg id='mine'/>").unwrap();
        let wire = tab_icons_at(&settings);
        let custom = wire.cards.iter().find(|card| card.id == "custom-1").unwrap();
        assert_eq!(custom.svg, "<svg id='mine'/>");
        assert_eq!(custom.circle, "#112233");
        let text = serde_json::to_string(&Reply::TabIcons(wire)).unwrap();
        assert!(text.starts_with(r#"{"op":"tab-icons","enabled":true,"terminal":"terminal","cards":["#));
    }

    #[test]
    fn kept_answers_survive_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("web_machines.json");
        change_kept_at(&path, |file| {
            kept_entry(file, "system-ssh-a").install = true
        })
        .unwrap();
        change_kept_at(&path, |file| {
            kept_entry(file, "web-1").host = Some(spec("devbox", "devbox.example"))
        })
        .unwrap();
        let file = load_kept(&path).unwrap();
        assert_eq!(file.machines.len(), 2);
        assert!(file.machines[0].install);
        assert_eq!(file.machines[1].host.as_ref().unwrap().label, "devbox");
        change_kept_at(&path, |file| file.machines.retain(|k| k.id != "web-1")).unwrap();
        assert_eq!(load_kept(&path).unwrap().machines.len(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // A file that does not parse is never written over: it may hold
        // passwords.
        std::fs::write(&path, "{").unwrap();
        assert!(load_kept(&path).is_err());
        assert!(change_kept_at(&path, |file| kept_entry(file, "web-2").install = true).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{");
    }
}
