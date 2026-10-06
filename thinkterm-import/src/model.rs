use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

pub type PaneId = u32;
pub const MAX_PANES: usize = 1024;
pub const MAX_DEPTH: usize = 64;
const MAX_TEXT: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum ImportMode {
    Layout,
    Live,
}

impl ImportMode {
    pub fn is_live(self) -> bool {
        self == Self::Live
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ImportRequest {
    pub source: String,
    pub session: String,
    pub mode: ImportMode,
    pub fingerprint: String,
    pub space_name: String,
}

impl ImportRequest {
    pub fn validate(&self) -> Result<()> {
        ensure!(valid_source_id(&self.source), "Invalid import source");
        ensure!(
            !self.session.is_empty() && self.session.len() <= MAX_TEXT,
            "Invalid import session"
        );
        ensure!(
            !self.fingerprint.is_empty() && self.fingerprint.len() <= 256,
            "Invalid import fingerprint"
        );
        ensure!(
            !self.space_name.trim().is_empty() && self.space_name.len() <= 256,
            "Invalid imported Space name"
        );
        Ok(())
    }
}

pub fn valid_source_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && !id.starts_with('-')
        && !id.ends_with('-')
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Session {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PreviewTerminal {
    pub title: Option<String>,
    pub tab_name: Option<String>,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PreviewProject {
    pub name: String,
    pub threads: usize,
    pub tabs: usize,
    pub panes: usize,
    pub terminals: Vec<PreviewTerminal>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Preview {
    pub session: String,
    pub live: bool,
    pub version: Option<String>,
    pub fingerprint: String,
    pub projects: Vec<PreviewProject>,
    pub unavailable: Option<String>,
}

impl Preview {
    pub fn pane_count(&self) -> usize {
        self.projects.iter().map(|p| p.panes).sum()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ImportPlan {
    pub mode: ImportMode,
    pub projects: Vec<Project>,
    pub active: Selection,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
pub struct Selection {
    pub project: usize,
    pub thread: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Project {
    pub name: String,
    pub directory: String,
    pub threads: Vec<Thread>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Thread {
    pub name: String,
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Tab {
    pub name: Option<String>,
    pub layout: Layout,
    pub panes: BTreeMap<PaneId, Pane>,
    pub focused: Option<PaneId>,
    pub zoomed: bool,
}

/// Layout imports always start a fresh login shell, never a source command.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Pane {
    pub cwd: String,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum Layout {
    Pane(PaneId),
    Stack {
        panes: Vec<PaneId>,
        active: usize,
    },
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

impl Layout {
    fn validate(
        &self,
        depth: usize,
        panes: &BTreeMap<PaneId, Pane>,
        seen: &mut HashSet<PaneId>,
        selected: &mut HashSet<PaneId>,
    ) -> Result<()> {
        ensure!(depth <= MAX_DEPTH, "Import layout is too deeply nested");
        let mut visit = |id: PaneId| -> Result<()> {
            ensure!(
                panes.contains_key(&id),
                "Import layout references a missing terminal"
            );
            ensure!(seen.insert(id), "Import layout repeats a terminal");
            ensure!(seen.len() <= MAX_PANES, "Import has too many terminals");
            Ok(())
        };
        match self {
            Self::Pane(id) => {
                visit(*id)?;
                selected.insert(*id);
            }
            Self::Stack { panes, active } => {
                ensure!(
                    !panes.is_empty() && panes.len() <= MAX_PANES && *active < panes.len(),
                    "Invalid imported terminal stack"
                );
                for id in panes {
                    visit(*id)?;
                }
                selected.insert(panes[*active]);
            }
            Self::Split {
                direction: _,
                ratio,
                first,
                second,
            } => {
                ensure!(
                    ratio.is_finite() && *ratio > 0.0 && *ratio < 1.0,
                    "Invalid import split ratio"
                );
                first.validate(depth + 1, panes, seen, selected)?;
                second.validate(depth + 1, panes, seen, selected)?;
            }
        }
        Ok(())
    }

    pub fn first_pane(&self) -> PaneId {
        match self {
            Self::Pane(id) => *id,
            Self::Stack { panes, active } => panes[*active],
            Self::Split { first, .. } => first.first_pane(),
        }
    }
}

fn text(value: &str) -> Result<()> {
    ensure!(value.len() <= MAX_TEXT, "Import text is too long");
    Ok(())
}

fn directory(value: &str) -> Result<()> {
    text(value)?;
    ensure!(
        !value.contains('\0') && std::path::Path::new(value).is_absolute(),
        "Imported directory must be absolute"
    );
    Ok(())
}

impl ImportPlan {
    pub fn tabs(&self) -> impl Iterator<Item = &Tab> {
        self.projects
            .iter()
            .flat_map(|p| &p.threads)
            .flat_map(|t| &t.tabs)
    }

    pub fn pane_count(&self) -> usize {
        self.tabs().map(|tab| tab.panes.len()).sum()
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.projects.is_empty() && self.projects.len() <= MAX_PANES,
            "Invalid imported project count"
        );
        ensure!(
            self.active.project < self.projects.len()
                && self.active.thread < self.projects[self.active.project].threads.len(),
            "Invalid import selection"
        );
        let mut seen = HashSet::new();
        let mut tab_count = 0;
        let mut thread_count = 0;
        for project in &self.projects {
            text(&project.name)?;
            directory(&project.directory)?;
            ensure!(
                !project.threads.is_empty(),
                "Imported project has no threads"
            );
            thread_count += project.threads.len();
            ensure!(thread_count <= MAX_PANES, "Import has too many threads");
            for thread in &project.threads {
                text(&thread.name)?;
                ensure!(
                    !thread.tabs.is_empty() && thread.active_tab < thread.tabs.len(),
                    "Invalid imported tab selection"
                );
                tab_count += thread.tabs.len();
                ensure!(tab_count <= MAX_PANES, "Import has too many tabs");
                for tab in &thread.tabs {
                    if let Some(name) = &tab.name {
                        text(name)?;
                    }
                    let before = seen.len();
                    let mut selected = HashSet::new();
                    tab.layout
                        .validate(0, &tab.panes, &mut seen, &mut selected)?;
                    ensure!(
                        seen.len() - before == tab.panes.len(),
                        "Imported tab has terminals outside its layout"
                    );
                    if let Some(focused) = tab.focused {
                        ensure!(
                            selected.contains(&focused),
                            "Imported focus is not a visible terminal"
                        );
                    }
                    for pane in tab.panes.values() {
                        directory(&pane.cwd)?;
                        if let Some(title) = &pane.title {
                            text(title)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
