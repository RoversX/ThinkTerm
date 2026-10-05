//! What the host's tests share: a host of their own in a temporary
//! directory, started by the client library or by hand.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use thinkterm_plugin_channel::client::{Host, Notice, Session};
use thinkterm_plugin_channel::wire::ToHost;

pub const WAIT: Duration = Duration::from_secs(10);

pub fn host_in(dir: &Path) -> Host {
    Host {
        socket: dir.join("sock"),
        lock: dir.join("lock"),
        data_dir: dir.join("data"),
        log: dir.join("log"),
        program: PathBuf::from(env!("CARGO_BIN_EXE_thinkterm-plugin-server")),
    }
}

/// A host started by hand, with arguments the client library does not pass.
pub fn spawn(host: &Host, extra: &[&str]) -> Child {
    spawn_with_env(host, extra, &[])
}

/// [`spawn`], with variables in the host's environment.
pub fn spawn_with_env(host: &Host, extra: &[&str], env: &[(&str, &str)]) -> Child {
    Command::new(&host.program)
        .arg("--socket")
        .arg(&host.socket)
        .arg("--lock")
        .arg(&host.lock)
        .arg("--data-dir")
        .arg(&host.data_dir)
        .args(extra)
        .envs(env.iter().copied())
        .spawn()
        .unwrap()
}

/// Leaves no host running after the test.
pub fn stop(host: &Host) {
    if let Ok(mut connection) = host.connect() {
        let _ = connection.send(&ToHost::Quit);
    }
}

pub fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Lets installed plugin `id`, in `dir`, run, as turning it on does: in
/// the host's switches, which it reads again whenever they change.
pub fn allow(host: &Host, id: &str, dir: &Path) {
    let path = host.data_dir.join("plugins.json");
    let mut switches: serde_json::Value = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_else(|| serde_json::json!({"plugins": {}}));
    let real = std::fs::canonicalize(dir).unwrap().display().to_string();
    switches["plugins"][id] = serde_json::json!({"enabled": true, "allowed": real});
    std::fs::create_dir_all(&host.data_dir).unwrap();
    // Whole or not at all, as the host writes it: a host reading meanwhile
    // never finds half a file.
    let partial = host.data_dir.join("plugins.json.partial");
    std::fs::write(&partial, serde_json::to_vec_pretty(&switches).unwrap()).unwrap();
    std::fs::rename(&partial, &path).unwrap();
}

/// A session, and the notices it gives.
pub fn session(host: &Host) -> (Session, mpsc::Receiver<Notice>) {
    let (tx, rx) = mpsc::channel();
    let session = Session::start(host.clone(), move |notice| {
        let _ = tx.send(notice);
    })
    .unwrap();
    (session, rx)
}

pub fn next_notice(notices: &mpsc::Receiver<Notice>, wanted: impl Fn(&Notice) -> bool) -> Notice {
    let deadline = Instant::now() + WAIT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let notice = notices.recv_timeout(left).expect("a notice");
        if wanted(&notice) {
            return notice;
        }
    }
}
