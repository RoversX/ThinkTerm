//! Joining a tailnet from inside ThinkTerm, without the Tailscale app: a node
//! of Tailscale's own Rust implementation, held by `thinkterm-tailnet`, a
//! program the machine's mux server keeps running for it.
//!
//! The settings file says whether the machine joins; while it does, the mux
//! server keeps `thinkterm-tailnet` running ([`keep_node`]), which holds the
//! node ([`follow`], with the `node` feature) and reports what the node is
//! doing to a file the app and `thinkterm tailnet` read. The node runs in a
//! process of its own so that whatever goes wrong in the library -- a panic
//! that aborts, a loop that spins, memory that grows -- stays out of the
//! process the terminal sessions live in. SSH connections to the tailnet's
//! machines go through the node ([`use_for_ssh`]) from any ThinkTerm process
//! on the machine, the mux server's included: they ask `thinkterm-tailnet`
//! for them on a socket, and need nothing of the library. The node also
//! takes SSH connections from the tailnet to this machine's own sshd when
//! `thinkterm tailnet up --serve-ssh` said to ([`Settings::serve_ssh`]), and
//! nothing else.

mod settings;
mod status;
#[cfg(windows)]
mod windows;

#[cfg(all(
    feature = "node",
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
mod holder;
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
mod keeper;
// The node itself is thinkterm-tailnet-engine's: the one caller of the
// library, which a phone's app holds too.
#[cfg(all(
    feature = "node",
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
pub(crate) use thinkterm_tailnet_engine::node;
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
mod proxy;

#[cfg(all(
    feature = "node",
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
pub use holder::{follow, is_following, is_off};
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
pub use keeper::{keep_node, will_hold};
pub use settings::{load, load_summary, Settings, Summary};
pub(crate) use status::Family;
pub use status::{read_live_report, Peer, Report, Status};

/// Nothing to hold where the library does not run.
#[cfg(all(
    feature = "node",
    not(any(target_os = "macos", target_os = "linux", target_os = "windows"))
))]
pub fn follow(changed: impl Fn() + Send + Sync + 'static) {
    let _ = changed;
}

/// Nothing is held where the library does not run, so nothing is to let go.
#[cfg(all(
    feature = "node",
    not(any(target_os = "macos", target_os = "linux", target_os = "windows"))
))]
pub fn is_off() -> bool {
    true
}

/// Nothing follows the settings where the library does not run.
#[cfg(all(
    feature = "node",
    not(any(target_os = "macos", target_os = "linux", target_os = "windows"))
))]
pub fn is_following() -> bool {
    false
}

/// Nothing to keep running where the library does not run.
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub fn keep_node() {}

/// Nothing to take over where the library does not run.
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub fn will_hold() {}

use anyhow::{Context, Result};
use std::time::Duration;

/// Whether this build can join a tailnet: the library runs on macOS, Linux
/// and Windows only.
pub const SUPPORTED: bool = cfg!(any(
    target_os = "macos",
    target_os = "linux",
    target_os = "windows"
));

/// How long a connection waits for the other machine when the caller sets
/// no limit of its own.
const DIAL_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a connection selecting the tailnet waits for `thinkterm-tailnet`
/// to take up the node: just after the switch was turned on, or while it
/// starts with the mux server at login.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
const HOLDER_WAIT: Duration = Duration::from_secs(15);

/// Tailscale's own control server, where a node goes when none is set.
const DEFAULT_CONTROL_SERVER: &str = "https://controlplane.tailscale.com";

/// The capability version the key is asked for with: the one the library
/// speaks (ts_control's `CapabilityVersion::CURRENT`). A control server
/// sends its key the same way to every recent one.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
const KEY_VERSION: &str = "130";

/// Changes the settings file: reads it afresh, applies `change`, writes it
/// back and has this process act on it at once if it holds the node. Blank
/// fields are stored as absent (see [`Settings::normalize`]) and the control
/// server in one spelling (see [`control_url_as_kept`]). Settings a node is
/// to start on are not stored unless one could; leaving never needs usable
/// ones.
pub fn update(change: impl FnOnce(&mut Settings)) -> Result<Settings> {
    // Serializes this process's read-modify-write of the file; the file
    // lock does the same between processes.
    static SETTINGS: std::sync::Mutex<()> = std::sync::Mutex::new(());
    settings::check_writer()?;
    let settings = {
        let _guard = SETTINGS.lock().unwrap();
        let _locked = settings::lock_for_update()?;
        let mut settings = settings::load()?;
        let saved_url = settings.control_url.clone();
        change(&mut settings);
        settings.normalize();
        // Only an address that is new, or that a node is to start on, has
        // to be one: an address broken by hand does not keep the node on.
        if settings.enabled || settings.control_url != saved_url {
            settings.control_url = match &settings.control_url {
                Some(url) => control_url_as_kept(url)?,
                None => None,
            };
        }
        settings.revision = settings.revision.wrapping_add(1);
        settings::save(&settings)?;
        settings
    };
    #[cfg(all(
        feature = "node",
        any(target_os = "macos", target_os = "linux", target_os = "windows")
    ))]
    holder::settings_changed();
    Ok(settings)
}

/// A control server's address as the settings keep it: None for
/// Tailscale's own (blank), else in one spelling, so that the same server
/// typed two ways is no change and restarts nothing. Only HTTPS: the
/// library asks a control server for its key over HTTPS whatever the
/// address says, so one reached by plain HTTP would never let a node in.
pub fn control_url_as_kept(text: &str) -> Result<Option<String>> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let url = control_url(text)?;
    if url.scheme() != "https" {
        anyhow::bail!(
            "the control server address {text:?}: a control server is reached over HTTPS"
        );
    }
    Ok(Some(url.to_string()))
}

pub(crate) use thinkterm_tailnet_engine::control_url;

/// Whether a control server answers at `text` -- Tailscale's own when it is
/// empty -- the way it answers a node about to join: with its key, over
/// HTTPS. Takes up to 15 seconds.
pub fn check_control_server(text: &str) -> Result<()> {
    let url = match text.trim() {
        "" => control_url(DEFAULT_CONTROL_SERVER)?,
        text => control_url(text)?,
    };
    fetch_control_key(&url).with_context(|| format!("the control server at {url}"))
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn fetch_control_key(url: &url::Url) -> Result<()> {
    use std::convert::TryFrom;
    let mut key_url = url.join("/key")?;
    // Over HTTPS whatever the address says, as the node asks.
    key_url
        .set_scheme("https")
        .map_err(|()| anyhow::anyhow!("not an address HTTPS can reach"))?;
    key_url.query_pairs_mut().append_pair("v", KEY_VERSION);
    let uri = http_req::uri::Uri::try_from(key_url.as_str())?;
    let mut body = Vec::new();
    let response = http_req::request::Request::new(&uri)
        .timeout(Duration::from_secs(15))
        .send(&mut body)?;
    if !response.status_code().is_success() {
        anyhow::bail!(
            "it answered HTTP {} {}",
            response.status_code(),
            response.reason()
        );
    }
    let keys: serde_json::Value =
        serde_json::from_slice(&body).context("it sent no key it could be told by")?;
    match keys.get("publicKey").and_then(|key| key.as_str()) {
        Some(key) if !key.is_empty() => Ok(()),
        _ => anyhow::bail!("it sent no key"),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn fetch_control_key(_url: &url::Url) -> Result<()> {
    anyhow::bail!("joining a tailnet is not supported on this platform")
}

/// Registers the tailnet for SSH connections that explicitly select it.
/// For the app and `thinkterm`.
pub fn use_for_ssh() {
    if !SUPPORTED {
        return;
    }
    wezterm_ssh::set_dialer(Some(std::sync::Arc::new(
        |host: &str, port: u16, family, timeout, cancelled: &dyn Fn() -> bool| {
            dial(host, port, status::family(family), timeout, cancelled)
        },
    )));
}

/// [`use_for_ssh`] for the mux server: through whichever process holds the
/// node, once the predecessor's node has gone after a takeover (see
/// `server_dial`).
pub fn use_for_ssh_in_server() {
    if !SUPPORTED {
        return;
    }
    wezterm_ssh::set_dialer(Some(std::sync::Arc::new(
        |host: &str, port: u16, family, timeout, cancelled: &dyn Fn() -> bool| {
            server_dial(host, port, status::family(family), timeout, cancelled)
        },
    )));
}

/// A connection to `host:port` through the selected tailnet. Its machine
/// list alone resolves the name. None means the node cannot reach it;
/// the SSH dialer reports that as an error, without trying system DNS.
/// Blocks while connecting, and gives up soon after `cancelled` says so.
///
/// For the app and `thinkterm`: it asks `thinkterm-tailnet`, and refers to
/// nothing of the node itself, so that they link none of it.
pub(crate) fn dial(
    host: &str,
    port: u16,
    family: Family,
    timeout: Option<Duration>,
    cancelled: &dyn Fn() -> bool,
) -> Option<Result<socket2::Socket>> {
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    {
        through_holder(
            host,
            port,
            family,
            timeout.unwrap_or(DIAL_TIMEOUT),
            cancelled,
        )
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = (host, port, family, timeout, cancelled, DIAL_TIMEOUT);
        None
    }
}

/// `dial` for the mux server. During a takeover, it waits for the
/// predecessor's node to go, rather than go through it and end with it (see
/// `keeper::wait_out_takeover`).
pub(crate) fn server_dial(
    host: &str,
    port: u16,
    family: Family,
    timeout: Option<Duration>,
    cancelled: &dyn Fn() -> bool,
) -> Option<Result<socket2::Socket>> {
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    {
        let timeout = timeout.unwrap_or(DIAL_TIMEOUT);
        let started = std::time::Instant::now();
        if !keeper::wait_out_takeover(timeout, cancelled) {
            return None;
        }
        let left = timeout.saturating_sub(started.elapsed());
        through_holder(host, port, family, left, cancelled)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = (host, port, family, timeout, cancelled);
        None
    }
}

/// A connection through the process holding the selected node. While
/// nobody holds it, or the node it holds is on its way out, the connection
/// waits for a holder within its timeout.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn through_holder(
    host: &str,
    port: u16,
    family: Family,
    timeout: Duration,
    cancelled: &dyn Fn() -> bool,
) -> Option<Result<socket2::Socket>> {
    if !load_summary().is_ok_and(|settings| settings.enabled) {
        return None;
    }
    let path = settings::socket_path().ok()?;
    through_holder_at(&path, host, port, family, timeout, cancelled)
}

/// `through_holder`, with where the holder listens passed in.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn through_holder_at(
    path: &std::path::Path,
    host: &str,
    port: u16,
    family: Family,
    timeout: Duration,
    cancelled: &dyn Fn() -> bool,
) -> Option<Result<socket2::Socket>> {
    // One time for all of it: waiting for a holder, and what the holder
    // does with what is left.
    let started = std::time::Instant::now();
    let deadline = started + timeout.min(HOLDER_WAIT);
    loop {
        let left = timeout.saturating_sub(started.elapsed());
        if let proxy::Asked::Answered(dialed) =
            proxy::ask(path, host, port, family, left, cancelled)
        {
            return dialed;
        }
        if cancelled() || std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_urls_get_a_scheme_when_typed_without() {
        assert_eq!(
            control_url("headscale.example.com").unwrap().as_str(),
            "https://headscale.example.com/"
        );
        assert_eq!(
            control_url(" http://headscale.example.com:8080 ")
                .unwrap()
                .as_str(),
            "http://headscale.example.com:8080/"
        );
        assert!(control_url("https://").is_err());
        assert_eq!(control_url_as_kept("  ").unwrap(), None);
        assert_eq!(
            control_url_as_kept("headscale.example.com").unwrap(),
            Some("https://headscale.example.com/".to_string())
        );
        // The key would be asked for over HTTPS all the same.
        assert!(control_url_as_kept("http://headscale.example.com:8080").is_err());
    }

    // What a holder listens with: AF_UNIX on Windows too (see proxy).
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    use std::os::unix::net::UnixListener;
    #[cfg(target_os = "windows")]
    use wezterm_uds::UnixListener;

    /// Where a holder would listen, in a folder of its own: socket paths
    /// are short, and the default temp dir on macOS is not.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn temp_socket() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::Builder::new()
            .prefix("tt-")
            .tempdir_in("/tmp")
            .unwrap();
        let path = dir.path().join("tailnet.sock");
        (dir, path)
    }

    #[cfg(target_os = "windows")]
    fn temp_socket() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::Builder::new().prefix("tt-").tempdir().unwrap();
        let path = dir.path().join("tailnet.sock");
        (dir, path)
    }

    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    #[test]
    fn a_tailnet_host_waits_for_the_server_to_take_up_the_node() {
        use std::io::{Read, Write};
        let (_dir, path) = temp_socket();
        let late = path.clone();
        let holder = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            let listener = UnixListener::bind(&late).unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            let mut byte = [0u8; 1];
            while stream.read(&mut byte).unwrap() == 1 && byte[0] != b'\n' {}
            stream.write_all(b"none\n").unwrap();
        });
        let started = std::time::Instant::now();
        let dialed = through_holder_at(
            &path,
            "server-a",
            22,
            Family::Any,
            Duration::from_secs(5),
            &|| false,
        );
        // The holder's own answer, once it was there to give one.
        assert!(dialed.is_none());
        assert!(started.elapsed() >= Duration::from_millis(300));
        holder.join().unwrap();
    }

    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    #[test]
    fn a_connection_is_asked_again_of_the_node_in_the_place_of_one_that_went() {
        use std::io::{Read, Write};
        let (_dir, path) = temp_socket();
        let listener = UnixListener::bind(&path).unwrap();
        let holder = std::thread::spawn(move || {
            // The node on its way out takes the request and answers nothing.
            let (mut going, _) = listener.accept().unwrap();
            let mut byte = [0u8; 1];
            while going.read(&mut byte).unwrap() == 1 && byte[0] != b'\n' {}
            drop(going);
            let (mut stream, _) = listener.accept().unwrap();
            while stream.read(&mut byte).unwrap() == 1 && byte[0] != b'\n' {}
            stream.write_all(b"ok\n").unwrap();
            // The connection carries on until the asker is done with it.
            let _ = stream.read(&mut byte);
        });
        let dialed = through_holder_at(
            &path,
            "server-a",
            22,
            Family::Any,
            Duration::from_secs(5),
            &|| false,
        );
        match dialed {
            Some(Ok(connection)) => drop(connection),
            Some(Err(err)) => panic!("no connection: {err:#}"),
            None => panic!("no connection"),
        }
        holder.join().unwrap();
    }

    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    #[test]
    fn a_closed_session_does_not_wait() {
        let (_dir, path) = temp_socket();
        let started = std::time::Instant::now();
        let dialed = through_holder_at(
            &path,
            "server-a",
            22,
            Family::Any,
            Duration::from_secs(5),
            &|| true,
        );
        assert!(dialed.is_none());
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
