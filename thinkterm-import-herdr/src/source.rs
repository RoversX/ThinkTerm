use super::model::{Layout, Snapshot, Tab};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const MAX_BYTES: usize = 16 * 1024 * 1024;

use thinkterm_import::ImportContext;
pub use thinkterm_import::{Preview, PreviewProject, PreviewTerminal, Session};

pub fn config_root(context: &ImportContext) -> Result<PathBuf> {
    let config = context
        .config_home
        .clone()
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| context.home.join(".config"));
    Ok(config.join("herdr"))
}

pub fn session_dir(context: &ImportContext, name: &str) -> Result<PathBuf> {
    ensure!(
        !name.is_empty()
            && name.len() <= 128
            && !name.contains('/')
            && !name.contains('\\')
            && name != "."
            && name != ".."
            && !name.chars().any(char::is_control),
        "Invalid Herdr session name"
    );
    let root = config_root(context)?;
    let path = if name == "default" {
        root
    } else {
        root.join("sessions").join(name)
    };
    let metadata = fs::symlink_metadata(&path).context("Herdr session directory is unavailable")?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "Herdr session must be a directory"
    );
    ensure!(
        metadata.uid() == rustix::process::geteuid().as_raw(),
        "Herdr session belongs to a different user"
    );
    Ok(path)
}

pub fn discover(context: &ImportContext) -> Result<Vec<Session>> {
    let root = config_root(context)?;
    let mut sessions = Vec::new();
    if root.join("session.json").exists() || root.join("herdr.sock").exists() {
        sessions.push(Session {
            name: "default".into(),
        });
    }
    match fs::read_dir(root.join("sessions")) {
        Ok(entries) => {
            for entry in entries.take(256) {
                let entry = entry?;
                if entry.file_type()?.is_dir()
                    && (entry.path().join("session.json").exists()
                        || entry.path().join("herdr.sock").exists())
                {
                    if let Some(name) = entry.file_name().to_str() {
                        if session_dir(context, name).is_ok() {
                            sessions.push(Session { name: name.into() });
                        }
                    }
                }
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    sessions.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(sessions)
}

pub fn read_line(stream: &mut UnixStream, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut byte = [0];
    loop {
        stream.read_exact(&mut byte)?;
        if byte[0] == b'\n' {
            return Ok(bytes);
        }
        ensure!(bytes.len() < limit, "Herdr response exceeds the size limit");
        bytes.push(byte[0]);
    }
}

fn connect(path: &Path, timeout: Duration) -> Result<UnixStream> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.file_type().is_socket() && metadata.uid() == rustix::process::geteuid().as_raw(),
        "Invalid Herdr socket owner or type"
    );
    let stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    Ok(stream)
}

pub fn request(dir: &Path, method: &str, params: Value, timeout: Duration) -> Result<Value> {
    let mut stream = connect(&dir.join("herdr.sock"), timeout)?;
    serde_json::to_writer(
        &mut stream,
        &json!({ "id": "thinkterm:import", "method": method, "params": params }),
    )?;
    stream.write_all(b"\n")?;
    let response: Value = serde_json::from_slice(&read_line(&mut stream, MAX_BYTES)?)?;
    if let Some(error) = response.get("error") {
        bail!(
            "Herdr: {}",
            error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("request failed")
        );
    }
    response
        .get("result")
        .cloned()
        .context("Herdr returned no result")
}

pub fn saved_snapshot(dir: &Path) -> Result<(Snapshot, String)> {
    let path = dir.join("session.json");
    let metadata = fs::symlink_metadata(&path)?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == rustix::process::geteuid().as_raw(),
        "Invalid Herdr snapshot owner or type"
    );
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_BYTES, "Herdr snapshot is too large");
    let fingerprint = format!("{:x}", Sha256::digest(&bytes));
    Ok((Snapshot::parse(&bytes)?, fingerprint))
}

pub(super) fn live_fingerprint(mut identity: Value) -> Result<String> {
    // GUI and mux builds may enable different serde_json map ordering.
    identity.sort_all_objects();
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&identity)?)
    ))
}

fn preview_text(text: &str, limit: usize) -> Option<String> {
    let text: String = text
        .chars()
        .take(limit + 1)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut chars = text.trim().chars();
    let mut result: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        result.push('…');
    }
    (!result.is_empty()).then_some(result)
}

pub(super) fn live_projects(
    workspaces: &[Value],
    tabs: &[Value],
    panes: &[Value],
) -> Vec<PreviewProject> {
    workspaces
        .iter()
        .map(|workspace| {
            let project_tabs: Vec<_> = tabs
                .iter()
                .filter(|tab| tab["workspace_id"] == workspace["workspace_id"])
                .collect();
            let terminals: Vec<_> = panes
                .iter()
                .filter(|pane| pane["workspace_id"] == workspace["workspace_id"])
                .map(|pane| PreviewTerminal {
                    title: [
                        "title",
                        "label",
                        "terminal_title_stripped",
                        "terminal_title",
                        "display_agent",
                        "agent",
                    ]
                    .iter()
                    .find_map(|key| pane[*key].as_str().and_then(|s| preview_text(s, 256))),
                    tab_name: project_tabs
                        .iter()
                        .find(|tab| tab["tab_id"] == pane["tab_id"])
                        .and_then(|tab| tab["label"].as_str())
                        .and_then(|s| preview_text(s, 256)),
                    cwd: ["foreground_cwd", "cwd"]
                        .iter()
                        .find_map(|key| pane[*key].as_str().and_then(|s| preview_text(s, 4096))),
                })
                .collect();
            PreviewProject {
                threads: 1,
                name: workspace["label"].as_str().unwrap_or("Herdr").into(),
                tabs: project_tabs.len(),
                panes: terminals.len(),
                terminals,
            }
        })
        .collect()
}

pub(super) fn saved_projects(snapshot: &Snapshot) -> Vec<PreviewProject> {
    fn collect_terminals(layout: &Layout, tab: &Tab, terminals: &mut Vec<PreviewTerminal>) {
        match layout {
            Layout::Pane(id) => {
                let pane = &tab.panes[id];
                terminals.push(PreviewTerminal {
                    title: pane.label.as_deref().and_then(|s| preview_text(s, 256)),
                    tab_name: tab
                        .custom_name
                        .as_deref()
                        .and_then(|s| preview_text(s, 256)),
                    cwd: preview_text(&pane.cwd, 4096),
                });
            }
            Layout::Split { first, second, .. } => {
                collect_terminals(first, tab, terminals);
                collect_terminals(second, tab, terminals);
            }
        }
    }

    snapshot
        .workspaces
        .iter()
        .enumerate()
        .map(|(index, workspace)| {
            let mut terminals = Vec::new();
            for tab in &workspace.tabs {
                collect_terminals(&tab.layout, tab, &mut terminals);
            }
            PreviewProject {
                threads: 1,
                name: workspace.display_name(index),
                tabs: workspace.tabs.len(),
                panes: terminals.len(),
                terminals,
            }
        })
        .collect()
}

pub fn preview(context: &ImportContext, name: &str) -> Result<Preview> {
    let dir = session_dir(context, name)?;
    let ping = request(&dir, "ping", json!({}), Duration::from_secs(3));
    let ping = match ping {
        Ok(ping) => Some(ping),
        Err(error)
            if error
                .chain()
                .filter_map(|e| e.downcast_ref::<std::io::Error>())
                .any(|e| {
                    matches!(
                        e.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    )
                }) =>
        {
            None
        }
        Err(error) => return Err(error.context("Cannot inspect Herdr; no session was changed")),
    };
    if let Some(ping) = ping {
        let result = request(&dir, "session.snapshot", json!({}), Duration::from_secs(5))?;
        let snapshot = result
            .get("snapshot")
            .context("Herdr did not return its session structure")?;
        let workspaces = snapshot["workspaces"]
            .as_array()
            .context("Missing Herdr workspaces")?;
        let tabs = snapshot["tabs"].as_array().context("Missing Herdr tabs")?;
        let panes = snapshot["panes"]
            .as_array()
            .context("Missing Herdr panes")?;
        ensure!(
            panes.len() <= super::model::MAX_PANES,
            "Herdr session has too many panes"
        );
        let identity = json!({
            "workspaces": workspaces.iter().map(|w| json!([w["workspace_id"], w["label"]])).collect::<Vec<_>>(),
            "tabs": tabs.iter().map(|t| json!([t["tab_id"], t["workspace_id"], t["label"]])).collect::<Vec<_>>(),
            "panes": panes.iter().map(|p| json!([p["pane_id"], p["terminal_id"], p["tab_id"]])).collect::<Vec<_>>()
        });
        let projects = live_projects(workspaces, tabs, panes);
        let available = ping["capabilities"]["live_handoff"]
            .as_bool()
            .unwrap_or(false);
        Ok(Preview {
            session: name.into(),
            live: true,
            version: ping["version"].as_str().map(str::to_string),
            fingerprint: live_fingerprint(identity)?,
            projects,
            unavailable: if panes.is_empty() {
                Some("Herdr session is empty".into())
            } else if !available {
                Some("This running Herdr server does not support live handoff".into())
            } else {
                None
            },
        })
    } else {
        let (snapshot, fingerprint) = saved_snapshot(&dir)?;
        let unavailable = snapshot
            .check_working_directories()
            .err()
            .map(|error| error.to_string());
        Ok(Preview {
            session: name.into(),
            live: false,
            version: None,
            fingerprint,
            projects: saved_projects(&snapshot),
            unavailable,
        })
    }
}
