use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

pub const MAX_PANES: usize = 1024;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Snapshot {
    #[serde(default)]
    pub version: u32,
    pub workspaces: Vec<Workspace>,
    #[serde(default)]
    pub active: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Workspace {
    #[serde(default)]
    pub custom_name: Option<String>,
    pub identity_cwd: String,
    pub tabs: Vec<Tab>,
    #[serde(default)]
    pub active_tab: usize,
}

impl Workspace {
    pub fn display_name(&self, index: usize) -> String {
        self.custom_name
            .clone()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| {
                std::path::Path::new(&self.identity_cwd)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| format!("Project {}", index + 1))
            })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Tab {
    #[serde(default)]
    pub custom_name: Option<String>,
    pub layout: Layout,
    pub panes: BTreeMap<u32, Pane>,
    #[serde(default)]
    pub focused: Option<u32>,
    #[serde(default)]
    pub zoomed: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Pane {
    pub cwd: String,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum Layout {
    Pane(u32),
    Split {
        direction: Direction,
        ratio: f32,
        first: Box<Layout>,
        second: Box<Layout>,
    },
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub enum Direction {
    Horizontal,
    Vertical,
}

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub source_version: String,
    pub snapshot: Snapshot,
    pub panes: Vec<Runtime>,
}

#[derive(Debug, Deserialize)]
pub struct Runtime {
    pub pane_id: u32,
    pub child_pid: u32,
    pub rows: u16,
    pub cols: u16,
    #[serde(default)]
    pub cell_width_px: u32,
    #[serde(default)]
    pub cell_height_px: u32,
    #[serde(default)]
    pub keyboard_protocol_flags: u16,
    #[serde(default)]
    pub keyboard_protocol_ansi: Option<String>,
    #[serde(default)]
    pub input_state: Option<InputState>,
    #[serde(default)]
    pub terminal_title: Option<String>,
    #[serde(default)]
    pub initial_history_ansi: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct InputState {
    pub alternate_screen: bool,
    pub application_cursor: bool,
    pub bracketed_paste: bool,
    pub focus_reporting: bool,
    pub mouse_protocol_mode: String,
    pub mouse_protocol_encoding: String,
    pub mouse_alternate_scroll: bool,
    pub modify_other_keys: bool,
    pub color_scheme_reporting: bool,
}

impl Layout {
    fn validate(
        &self,
        depth: usize,
        panes: &BTreeMap<u32, Pane>,
        seen: &mut HashSet<u32>,
    ) -> Result<()> {
        ensure!(depth <= 64, "Herdr layout is too deeply nested");
        match self {
            Self::Pane(id) => {
                ensure!(
                    panes.contains_key(id),
                    "Herdr layout references missing pane {id}"
                );
                ensure!(seen.insert(*id), "Herdr layout repeats pane {id}");
                ensure!(seen.len() <= MAX_PANES, "Herdr session has too many panes");
            }
            Self::Split {
                ratio,
                first,
                second,
                ..
            } => {
                ensure!(
                    ratio.is_finite() && *ratio > 0.0 && *ratio < 1.0,
                    "Invalid Herdr split ratio"
                );
                first.validate(depth + 1, panes, seen)?;
                second.validate(depth + 1, panes, seen)?;
            }
        }
        Ok(())
    }
}

impl Snapshot {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let mut value: serde_json::Value =
            serde_json::from_slice(bytes).context("Read Herdr session layout")?;
        // Early snapshots kept one tab directly on each workspace.
        if let Some(workspaces) = value.get_mut("workspaces").and_then(|v| v.as_array_mut()) {
            for workspace in workspaces {
                if workspace.get("tabs").is_none() && workspace.get("layout").is_some() {
                    let tab = serde_json::json!({
                        "layout": workspace["layout"], "panes": workspace["panes"],
                        "focused": workspace.get("focused"), "zoomed": workspace.get("zoomed").and_then(|v| v.as_bool()).unwrap_or(false)
                    });
                    let cwd = workspace["panes"]
                        .as_object()
                        .and_then(|panes| panes.values().next())
                        .and_then(|pane| pane.get("cwd"))
                        .cloned()
                        .context("Legacy Herdr workspace has no directory")?;
                    workspace["identity_cwd"] = cwd;
                    workspace["tabs"] = serde_json::json!([tab]);
                }
            }
        }
        let snapshot: Self = serde_json::from_value(value).context("Unsupported Herdr layout")?;
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version <= 3,
            "This Herdr snapshot format is newer than ThinkTerm supports"
        );
        ensure!(!self.workspaces.is_empty(), "Herdr session is empty");
        let mut seen = HashSet::new();
        for workspace in &self.workspaces {
            ensure!(!workspace.tabs.is_empty(), "Herdr workspace has no tabs");
            ensure!(
                std::path::Path::new(&workspace.identity_cwd).is_absolute(),
                "Herdr workspace directory must be absolute"
            );
            for tab in &workspace.tabs {
                let before = seen.len();
                tab.layout.validate(0, &tab.panes, &mut seen)?;
                ensure!(
                    seen.len() - before == tab.panes.len(),
                    "Herdr tab contains panes outside its layout"
                );
                if let Some(focused) = tab.focused {
                    ensure!(
                        tab.panes.contains_key(&focused),
                        "Herdr focused pane is missing"
                    );
                }
                for pane in tab.panes.values() {
                    ensure!(
                        std::path::Path::new(&pane.cwd).is_absolute(),
                        "Herdr pane directory must be absolute"
                    );
                }
            }
        }
        Ok(())
    }

    pub fn check_working_directories(&self) -> Result<()> {
        for pane in self
            .workspaces
            .iter()
            .flat_map(|workspace| &workspace.tabs)
            .flat_map(|tab| tab.panes.values())
        {
            ensure!(
                std::path::Path::new(&pane.cwd).is_dir(),
                "A saved Herdr working directory is unavailable"
            );
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn pane_count(&self) -> usize {
        self.workspaces
            .iter()
            .flat_map(|w| &w.tabs)
            .map(|t| t.panes.len())
            .sum()
    }
}

impl Manifest {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "Unsupported Herdr handoff version");
        ensure!(
            !self.source_version.is_empty() && self.source_version.len() <= 128,
            "Invalid Herdr source version"
        );
        self.snapshot.validate()?;
        ensure!(
            self.panes.len() <= MAX_PANES,
            "Herdr session has too many live panes"
        );
        let mut ids = HashSet::new();
        let mut pids = HashSet::new();
        let mut cells = 0u64;
        let expected: HashSet<_> = self
            .snapshot
            .workspaces
            .iter()
            .flat_map(|w| &w.tabs)
            .flat_map(|t| t.panes.keys().copied())
            .collect();
        for pane in &self.panes {
            ensure!(
                ids.insert(pane.pane_id) && expected.contains(&pane.pane_id),
                "Unexpected live Herdr pane"
            );
            ensure!(
                pane.child_pid > 1
                    && pane.child_pid <= i32::MAX as u32
                    && pids.insert(pane.child_pid),
                "Invalid Herdr process id"
            );
            ensure!(
                pane.rows > 0
                    && pane.cols > 0
                    && pane.rows <= 4096
                    && pane.cols <= 4096
                    && u32::from(pane.rows) * u32::from(pane.cols) <= 1_000_000,
                "Invalid Herdr terminal size"
            );
            cells += u64::from(pane.rows) * u64::from(pane.cols);
            ensure!(
                cells <= 16_000_000,
                "Herdr session exceeds the terminal cell limit"
            );
            if pane
                .initial_history_ansi
                .as_ref()
                .is_some_and(|s| s.len() > 8192)
                || pane
                    .keyboard_protocol_ansi
                    .as_ref()
                    .is_some_and(|s| s.len() > 4096)
            {
                bail!("Herdr terminal state is too large");
            }
        }
        ensure!(
            ids == expected,
            "Some Herdr panes have no live terminal; the session was not moved"
        );
        Ok(())
    }
}
