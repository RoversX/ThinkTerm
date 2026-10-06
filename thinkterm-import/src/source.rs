use crate::{ImportMode, ImportPlan, ImportRequest, PaneId, Preview, Session, MAX_PANES};
use anyhow::{ensure, Context, Result};
use std::collections::HashSet;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Debug, Clone)]
pub struct ImportContext {
    pub home: PathBuf,
    pub config_home: Option<PathBuf>,
    pub executable: PathBuf,
}

#[derive(Debug, Clone, Copy)]
pub struct SourceInfo {
    pub id: &'static str,
    pub name: &'static str,
    pub icon: &'static str,
}

/// Adapters normalize source modes and history to ANSI. The owner parses it
/// into a temporary terminal with a sink writer before attaching the PTY.
#[derive(Debug, Clone)]
pub struct TerminalSeed {
    pub child_pid: u32,
    pub rows: u16,
    pub cols: u16,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
    pub title: Option<String>,
    pub ansi: String,
}

pub struct LiveTerminal {
    pub pane_id: PaneId,
    pub seed: TerminalSeed,
    pub pty: OwnedFd,
}

/// The adapter keeps its source paused until the owner has prepared every
/// terminal. Dropping an uncommitted transfer must leave the source owning it.
pub trait Handoff: Send {
    fn commit(&mut self) -> Result<()>;
    fn finish(self: Box<Self>);
}

pub struct PreparedImport {
    pub plan: ImportPlan,
    pub terminals: Vec<LiveTerminal>,
    pub handoff: Option<Box<dyn Handoff>>,
}

impl PreparedImport {
    pub fn validate(&self) -> Result<()> {
        self.plan.validate()?;
        if self.plan.mode == ImportMode::Layout {
            ensure!(
                self.terminals.is_empty() && self.handoff.is_none(),
                "Layout import contains live resources"
            );
            for pane in self.plan.tabs().flat_map(|tab| tab.panes.values()) {
                ensure!(
                    std::path::Path::new(&pane.cwd).is_dir(),
                    "An imported working directory is unavailable"
                );
            }
            return Ok(());
        }
        ensure!(self.handoff.is_some(), "Live import has no handoff");
        ensure!(
            self.terminals.len() <= MAX_PANES,
            "Import has too many live terminals"
        );
        let expected: HashSet<_> = self
            .plan
            .tabs()
            .flat_map(|tab| tab.panes.keys().copied())
            .collect();
        let mut seen = HashSet::new();
        let mut pids = HashSet::new();
        let mut cells = 0u64;
        for terminal in &self.terminals {
            ensure!(
                seen.insert(terminal.pane_id) && expected.contains(&terminal.pane_id),
                "Unexpected imported terminal"
            );
            let seed = &terminal.seed;
            ensure!(
                seed.child_pid > 1
                    && seed.child_pid <= i32::MAX as u32
                    && pids.insert(seed.child_pid),
                "Invalid imported process id"
            );
            ensure!(
                seed.rows > 0
                    && seed.cols > 0
                    && seed.rows <= 4096
                    && seed.cols <= 4096
                    && u32::from(seed.rows) * u32::from(seed.cols) <= 1_000_000,
                "Invalid imported terminal size"
            );
            cells += u64::from(seed.rows) * u64::from(seed.cols);
            ensure!(
                cells <= 16_000_000,
                "Import exceeds the terminal cell limit"
            );
            ensure!(
                seed.ansi.len() <= 16 * 1024
                    && seed.title.as_ref().is_none_or(|title| title.len() <= 4096),
                "Imported terminal state is too large"
            );
        }
        ensure!(
            seen == expected,
            "Some imported panes have no live terminal"
        );
        Ok(())
    }
}

pub trait ImportSource: Send + Sync {
    fn info(&self) -> SourceInfo;
    fn discover(&self, context: &ImportContext) -> Result<Vec<Session>>;
    fn preview(&self, context: &ImportContext, session: &str) -> Result<Preview>;
    fn prepare(&self, context: &ImportContext, request: &ImportRequest) -> Result<PreparedImport>;
    fn notes(&self, _mode: Option<ImportMode>) -> Vec<&'static str> {
        vec![]
    }
    fn result_notes(&self) -> Vec<&'static str> {
        vec![]
    }
    fn error_key(&self, _message: &str) -> Option<&'static str> {
        None
    }
    fn run_helper(&self) -> Option<Result<()>> {
        None
    }
}

/// Composition roots register built-ins before starting the GUI or server.
/// The mux knows this registry, never the crates implementing its sources.
pub struct Registry {
    sources: Vec<&'static dyn ImportSource>,
}

impl Registry {
    pub const fn new() -> Self {
        Self {
            sources: Vec::new(),
        }
    }

    pub fn register(&mut self, source: &'static dyn ImportSource) -> Result<()> {
        let info = source.info();
        ensure!(crate::valid_source_id(info.id), "Invalid import source");
        ensure!(self.sources.len() < 32, "Too many import sources");
        ensure!(
            !self.sources.iter().any(|s| s.info().id == info.id),
            "Import source is already registered"
        );
        self.sources.push(source);
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&'static dyn ImportSource> {
        self.sources.iter().copied().find(|s| s.info().id == id)
    }
}

static SOURCES: Mutex<Registry> = Mutex::new(Registry::new());

pub fn register(source: &'static dyn ImportSource) -> Result<()> {
    SOURCES.lock().unwrap().register(source)
}

pub fn sources() -> Vec<SourceInfo> {
    SOURCES
        .lock()
        .unwrap()
        .sources
        .iter()
        .map(|s| s.info())
        .collect()
}

pub fn source(id: &str) -> Result<&'static dyn ImportSource> {
    SOURCES
        .lock()
        .unwrap()
        .get(id)
        .context("Import source is not registered")
}

pub fn prepare(context: &ImportContext, request: &ImportRequest) -> Result<PreparedImport> {
    request.validate()?;
    let prepared = source(&request.source)?.prepare(context, request)?;
    ensure!(
        prepared.plan.mode == request.mode,
        "Import mode changed after preview"
    );
    prepared.validate()?;
    Ok(prepared)
}

pub fn maybe_run_helper() -> Option<Result<()>> {
    for info in sources() {
        if let Some(result) = source(info.id).ok()?.run_helper() {
            return Some(result);
        }
    }
    None
}
