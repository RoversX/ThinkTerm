//! A ThinkTerm plugin that shows what a plugin can draw: a page -- a tab of
//! its own, or a pane beside a terminal -- with a tour of sections, each a
//! tool built by hand from rectangles, text, lines, areas, hits, scroll
//! areas, lists and fields. docs/thinkterm/plugins.md says how to install
//! it.
//!
//! - **Dashboard**: tiles, sparklines and a chart of made-up server numbers
//!   that a thread of the plugin's own moves once a second (`live`).
//! - **Components**: buttons, toggles, checkboxes, a segmented control,
//!   tabs, chips, a slider, a stepper, progress, cards, hover tints and
//!   cursors, corners, every colour ThinkTerm names, and its text.
//! - **Lists**: fifty thousand rows supplied as they come into view,
//!   filtered by a field and picked with the keys; a table wider than the
//!   page that scrolls sideways under its fixed first column; a long
//!   scroll area, and jumps within it.
//! - **Forms**: every kind of field, its icon, its clear button, a limit,
//!   where the keyboard is, and what was typed.
//! - **Motion**: drawn frame after frame by a ticker thread while it plays,
//!   with how many frames the plugin drew in the last second.
//! - **Board** and **Activity**: a kanban board whose cards move by click
//!   or key, kept in the plugin's data directory, and a year of activity
//!   as a heat map.
//! - **This page**: what ThinkTerm keeps of the page, where it was opened
//!   from, and buttons that type in and read that terminal.
//!
//! The page keeps the section it shows in what it is opened with
//! (`Frame::change_page`), and names its tab after it, so it comes back on
//! it after a restart and on another device. The panel in the sidebar has
//! a live summary and opens the page on any section (`Frame::open_page`),
//! and can show the dashboard in its extended view.

mod chrome;
mod live;
mod sections;
mod ui;

use anyhow::{bail, Context as _};
use live::{lock, Live, Metrics, SharedLive, SharedTicker, Ticker};
use sections::{activity, board, components, dashboard, forms, lists, motion, page};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use thinkterm_plugin_sdk::panel::{At, Click, Frame, Input, Item, Key, PageChange, RowsWanted};
use thinkterm_plugin_sdk::{Cx, Emitter, Plugin, View};
use ui::Look;

/// The parts of the tour, in the order the page lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    Dashboard,
    Components,
    Lists,
    Forms,
    Motion,
    Board,
    Activity,
    Page,
}

impl Section {
    pub const ALL: [Self; 8] = [
        Self::Dashboard,
        Self::Components,
        Self::Lists,
        Self::Forms,
        Self::Motion,
        Self::Board,
        Self::Activity,
        Self::Page,
    ];

    /// Its name in the page's `args`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Dashboard => "dashboard",
            Self::Components => "components",
            Self::Lists => "lists",
            Self::Forms => "forms",
            Self::Motion => "motion",
            Self::Board => "board",
            Self::Activity => "activity",
            Self::Page => "page",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Dashboard => "Dashboard",
            Self::Components => "Components",
            Self::Lists => "Lists",
            Self::Forms => "Forms",
            Self::Motion => "Motion",
            Self::Board => "Board",
            Self::Activity => "Activity",
            Self::Page => "This page",
        }
    }

    /// The icon the page's tab shows while it is on this section: one of
    /// those every client has.
    pub fn icon(self) -> &'static str {
        match self {
            Self::Dashboard => "gauge",
            Self::Components => "puzzle",
            Self::Lists => "database",
            Self::Forms => "bug",
            Self::Motion => "activity",
            Self::Board => "list-todo",
            Self::Activity => "calendar",
            Self::Page => "cpu",
        }
    }

    /// What it shows of a plugin's page, in a line.
    pub fn blurb(self) -> &'static str {
        match self {
            Self::Dashboard => {
                "Live numbers from a thread of the plugin's own, drawn again each second."
            }
            Self::Components => "Controls built from rectangles, text, lines and hits. No widgets.",
            Self::Lists => {
                "Rows supplied as they scroll into view, a wide table and a scroll area."
            }
            Self::Forms => "Fields the client draws and edits; the plugin hears what is typed.",
            Self::Motion => "Frames the plugin draws one after another while it plays.",
            Self::Board => "Cards that move by click or key, kept in the plugin's data directory.",
            Self::Activity => "A year as a heat map: every day a rectangle with a hit.",
            Self::Page => "What ThinkTerm keeps of this page, and the terminal it came from.",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|section| section.name() == name)
    }

    fn step(self, by: isize) -> Self {
        let at = Self::ALL.iter().position(|s| *s == self).unwrap_or(0) as isize;
        let count = Self::ALL.len() as isize;
        Self::ALL[(at + by).rem_euclid(count) as usize]
    }
}

/// What a page is opened with, for a section and, for Lists, its tab.
pub fn args_of(section: Section, tab: Option<lists::Tab>) -> Value {
    match (section, tab) {
        (Section::Lists, Some(tab)) => json!({"section": section.name(), "tab": tab.name()}),
        _ => json!({"section": section.name()}),
    }
}

/// The section, and the tab of Lists, a page's `args` name: the dashboard
/// for what names none, or none this plugin knows.
pub fn parse_args(args: &Value) -> (Section, Option<lists::Tab>) {
    let section = args["section"]
        .as_str()
        .and_then(Section::from_name)
        .unwrap_or(Section::Dashboard);
    let tab = args["tab"].as_str().and_then(lists::Tab::from_name);
    (section, tab)
}

/// What one view keeps: the section it shows, and each section's own.
pub struct ViewState {
    pub section: Section,
    /// On a narrow page, the list of sections shows in place of one.
    pub menu: bool,
    /// The section and tab the page last said it is opened with, and the
    /// change that said so, in every frame from then on.
    announced: Option<(Section, lists::Tab)>,
    change: Option<PageChange>,
    /// The page this view last asked for, by its seq, and its args.
    open: Option<(u32, Value)>,
    /// A panel asks for its extended view.
    pub extend: bool,
    pub dashboard: dashboard::State,
    pub components: components::State,
    pub lists: lists::State,
    pub forms: forms::State,
    pub motion: motion::State,
    pub board: board::Pick,
    pub activity: activity::State,
    pub page: page::State,
}

impl ViewState {
    fn new(view: &View) -> Self {
        let (section, tab) = match &view.page {
            Some(page) => parse_args(&page.args),
            None => (Section::Dashboard, None),
        };
        let mut lists = lists::State::default();
        if let Some(tab) = tab {
            lists.tab = tab;
        }
        Self {
            section,
            menu: false,
            announced: None,
            change: None,
            open: None,
            extend: false,
            dashboard: dashboard::State::default(),
            components: components::State::default(),
            lists,
            forms: forms::State::default(),
            motion: motion::State::default(),
            board: board::Pick::default(),
            activity: activity::State::default(),
            page: page::State::default(),
        }
    }

    /// Whether the live numbers move for this view.
    fn wants_live(&self, view: &View) -> bool {
        match (view.is_page(), view.extended()) {
            (true, _) => self.section == Section::Dashboard && !self.menu,
            // The panel shows a summary, and its extended view the
            // dashboard.
            (false, _) => true,
        }
    }

    /// How many frames a second this view wants drawn: 0 while its Motion
    /// does not play.
    fn wants_rate(&self, view: &View) -> u32 {
        let shows = view.is_page() && self.section == Section::Motion && !self.menu;
        match shows && self.motion.playing {
            true => self.motion.rate(),
            false => 0,
        }
    }
}

struct Showcase {
    views: HashMap<u64, ViewState>,
    /// What each view is, for the threads' sake: whether it wants the
    /// numbers moving, and how often it wants drawing.
    wants: HashMap<u64, (bool, u32)>,
    live: SharedLive,
    ticker: SharedTicker,
    board: board::Board,
    /// What came of the commands run in the terminals, for the views that
    /// asked, on their way back from the threads that ran them.
    ran: Arc<Mutex<Vec<(u64, page::Ran)>>>,
    /// Draws the views again from other threads; none in the tests.
    emitter: Option<Emitter>,
    /// Counts the pages asked for and the changes made to pages: each its
    /// own seq.
    seq: u32,
}

impl Showcase {
    fn new(emitter: Option<Emitter>, live: SharedLive, ticker: SharedTicker) -> Self {
        Self {
            views: HashMap::new(),
            wants: HashMap::new(),
            live,
            ticker,
            board: board::Board::load(thinkterm_plugin_sdk::data_dir()),
            ran: Arc::default(),
            emitter,
            seq: 0,
        }
    }

    fn next_seq(&mut self) -> u32 {
        self.seq = self.seq.wrapping_add(1).max(1);
        self.seq
    }

    /// Tells the threads what the views on show want: the numbers moving
    /// while one shows them, frames while one plays Motion.
    fn sync(&mut self, view: &View) {
        let wants = match self.views.get(&view.id) {
            Some(state) => (state.wants_live(view), state.wants_rate(view)),
            None => (false, 0),
        };
        match wants {
            (false, 0) => self.wants.remove(&view.id),
            wants => self.wants.insert(view.id, wants),
        };
        let wanted = self.wants.values().filter(|(live, _)| *live).count();
        let rate = self
            .wants
            .values()
            .map(|(_, rate)| *rate)
            .max()
            .unwrap_or(0);
        let (live, wake) = &*self.live;
        let mut live = lock(live);
        if live.wanted != wanted {
            live.wanted = wanted;
            wake.notify_all();
        }
        drop(live);
        let (ticker, wake) = &*self.ticker;
        let mut ticker = lock(ticker);
        if ticker.rate != rate {
            ticker.rate = rate;
            wake.notify_all();
        }
    }

    /// Runs `command` in the terminal `view` names, off the plugin's thread,
    /// and has the view drawn again with what came of it.
    fn run_in_terminal(&self, view: &View, what: page::Command) {
        let Some(cli) = view.cli() else {
            return;
        };
        let (ran, emitter, of) = (Arc::clone(&self.ran), self.emitter.clone(), view.id);
        std::thread::spawn(move || {
            let outcome = page::run(cli, what);
            let mut ran = lock(&ran);
            ran.retain(|(view, _)| *view != of);
            ran.push((of, outcome));
            drop(ran);
            if let Some(emitter) = emitter {
                emitter.redraw();
            }
        });
    }

    fn draw_page(&mut self, view: &View, frame: &mut Frame) {
        let look = Look::of(&view.env);
        let state = self.views.get_mut(&view.id).expect("made before drawing");
        // The narrow page's list of sections: one grown wide has the rail
        // in its place, and no list left open under the section.
        if chrome::wide(&look) {
            state.menu = false;
        }
        let content = chrome::draw(&mut frame.items, &look, state);
        if state.menu {
            chrome::menu(&mut frame.items, &look, content, state.section);
        } else {
            match state.section {
                Section::Dashboard => {
                    let live = lock(&self.live.0);
                    dashboard::draw(
                        &mut frame.items,
                        &look,
                        content,
                        &mut state.dashboard,
                        &live,
                        true,
                    );
                }
                Section::Components => {
                    components::draw(&mut frame.items, &look, content, &mut state.components)
                }
                Section::Lists => lists::draw(frame, &look, content, &mut state.lists),
                Section::Forms => forms::draw(frame, &look, content, &mut state.forms, view),
                Section::Motion => {
                    motion::draw(&mut frame.items, &look, content, &mut state.motion)
                }
                Section::Board => board::draw(frame, &look, content, &self.board, &mut state.board),
                Section::Activity => activity::draw(frame, &look, content, &mut state.activity),
                Section::Page => page::draw(&mut frame.items, &look, content, &state.page, view),
            }
        }
        // The page says which section it is on: its tab is named after it,
        // and it opens on it again -- after a restart, on another device.
        let now = (state.section, state.lists.tab);
        if state.announced != Some(now) {
            state.announced = Some(now);
            let tab = (state.section == Section::Lists).then_some(state.lists.tab);
            self.seq = self.seq.wrapping_add(1).max(1);
            state.change = Some(PageChange {
                seq: self.seq,
                title: Some(format!("Showcase \u{00b7} {}", state.section.title())),
                icon: Some(state.section.icon().to_string()),
                args: Some(args_of(state.section, tab)),
            });
        }
        if let Some(change) = &state.change {
            frame.change_page(change.clone());
        }
    }
}

impl Plugin for Showcase {
    fn call(&mut self, body: Value, _cx: &mut Cx) -> anyhow::Result<Value> {
        match body["op"].as_str() {
            Some("sections") => Ok(json!(Section::ALL.map(Section::name))),
            // An action (plugin.toml): ThinkTerm adds where the user asked
            // for it, and the greeting is typed in the terminal there.
            Some("type-greeting") => {
                let terminal = At::of(&body)
                    .and_then(|at| at.terminal)
                    .context("\"type-greeting\" is asked for at a terminal")?;
                let mut cli = thinkterm_plugin_sdk::cli(&terminal);
                let typed = cli
                    .args(["send-text", "--no-paste", page::GREETING])
                    .output()
                    .context("running thinkterm cli")?;
                if !typed.status.success() {
                    bail!("{}", String::from_utf8_lossy(&typed.stderr).trim());
                }
                Ok(json!(page::GREETING))
            }
            _ => bail!("unknown op; this plugin knows \"sections\" and \"type-greeting\""),
        }
    }

    fn draw(&mut self, view: &View, frame: &mut Frame) {
        self.views
            .entry(view.id)
            .or_insert_with(|| ViewState::new(view));
        // What a command run for this view came back with.
        let came: Vec<page::Ran> = {
            let mut ran = lock(&self.ran);
            let (mine, rest) = ran.drain(..).partition(|(of, _)| *of == view.id);
            *ran = rest;
            mine.into_iter().map(|(_, ran)| ran).collect()
        };
        if let Some(state) = self.views.get_mut(&view.id) {
            for ran in came {
                state.page.heard(ran);
            }
        }
        if view.is_page() {
            self.draw_page(view, frame);
        } else if view.extended() {
            let look = Look::of(&view.env);
            let state = self.views.get_mut(&view.id).expect("made above");
            let live = lock(&self.live.0);
            chrome::extended(&mut frame.items, &look, state, &live);
        } else {
            let look = Look::of(&view.env);
            let state = self.views.get_mut(&view.id).expect("made above");
            let live = lock(&self.live.0);
            chrome::panel(&mut frame.items, &look, state, &live, &self.board);
            frame.extend(state.extend && view.env.can_extend);
        }
        let state = &self.views[&view.id];
        if let Some((seq, args)) = &state.open {
            frame.open_page(*seq, args.clone());
        }
        frame.keys(&keys(state, view));
        self.sync(view);
    }

    fn rows(&mut self, view: &View, wanted: &RowsWanted) -> Vec<Vec<Item>> {
        let Some(state) = self.views.get_mut(&view.id) else {
            return Vec::new();
        };
        let look = Look::of(&view.env);
        lists::rows(&look, &mut state.lists, wanted)
    }

    fn input(&mut self, view: &View, input: Input, cx: &mut Cx) {
        self.views
            .entry(view.id)
            .or_insert_with(|| ViewState::new(view));
        match input {
            Input::Click(click) => self.click(view, click, cx),
            Input::Text(typed) if typed.id.starts_with("list:") => {
                let state = self.views.get_mut(&view.id).expect("made above");
                lists::typed(&mut state.lists, &typed, false);
            }
            Input::Submit(typed) if typed.id.starts_with("list:") => {
                let state = self.views.get_mut(&view.id).expect("made above");
                lists::typed(&mut state.lists, &typed, true);
            }
            Input::Text(typed) => {
                let state = self.views.get_mut(&view.id).expect("made above");
                forms::typed(&mut state.forms, &typed, false);
            }
            Input::Submit(typed) => {
                let state = self.views.get_mut(&view.id).expect("made above");
                forms::typed(&mut state.forms, &typed, true);
            }
            Input::Key(Key { key, .. }) => {
                let state = self.views.get_mut(&view.id).expect("made above");
                match state.section {
                    Section::Lists => lists::key(&mut state.lists, &key),
                    Section::Motion if key == " " => state.motion.playing = !state.motion.playing,
                    Section::Activity => activity::key(&mut state.activity, &key),
                    Section::Board if board::key(&mut self.board, &mut state.board, &key) => {
                        cx.redraw();
                    }
                    _ => {}
                }
            }
            // The extended view's close button: the panel stops asking.
            Input::Close => {
                if let Some(panel) = self.views.get_mut(&view.panel()) {
                    panel.extend = false;
                }
            }
            _ => {}
        }
    }

    fn closed(&mut self, view: &View) {
        self.views.remove(&view.id);
        lock(&self.ran).retain(|(of, _)| *of != view.id);
        self.sync(view);
    }
}

impl Showcase {
    fn click(&mut self, view: &View, click: Click, cx: &mut Cx) {
        let id = click.id.as_str();
        // Opening the page, from the panel or from the page itself.
        if let Some(name) = id.strip_prefix("open:") {
            let args = match Section::from_name(name) {
                Some(section) => args_of(section, None),
                None => Value::Null,
            };
            let seq = self.next_seq();
            if let Some(state) = self.views.get_mut(&view.id) {
                state.open = Some((seq, args));
            }
            return;
        }
        let state = self.views.get_mut(&view.id).expect("made before input");
        if let Some(name) = id.strip_prefix("nav:") {
            if let Some(section) = Section::from_name(name) {
                state.section = section;
            }
            state.menu = false;
            return;
        }
        match id {
            "menu" => state.menu = !state.menu,
            "prev" => {
                state.section = state.section.step(-1);
                state.menu = false;
            }
            "next" => {
                state.section = state.section.step(1);
                state.menu = false;
            }
            "extend" => state.extend = !state.extend,
            _ => {}
        }
        let Some((part, rest)) = id.split_once(':') else {
            return;
        };
        match part {
            "dash" => {
                if dashboard::click(&mut state.dashboard, rest, &self.live) {
                    cx.redraw();
                }
            }
            "comp" => components::click(&mut state.components, rest, &click),
            "list" => lists::click(&mut state.lists, rest),
            "form" => forms::click(&mut state.forms, rest),
            "mo" => motion::click(&mut state.motion, rest),
            "act" => activity::click(&mut state.activity, rest),
            "board" => {
                if board::click(&mut self.board, &mut state.board, rest) {
                    cx.redraw();
                }
            }
            "page" => {
                // Drawn disabled on a page opened from no terminal: a press
                // there starts nothing to wait for.
                if view.env.terminal.is_some() {
                    if let Some(command) = page::click(&mut state.page, rest) {
                        self.run_in_terminal(view, command);
                    }
                }
            }
            _ => {}
        }
    }
}

/// The keys a view takes while it has the keyboard.
fn keys(state: &ViewState, view: &View) -> Vec<&'static str> {
    if !view.is_page() || state.menu {
        return Vec::new();
    }
    match state.section {
        Section::Lists if state.lists.tab == lists::Tab::Events => {
            vec!["ArrowUp", "ArrowDown", "PageUp", "PageDown", "Home", "End"]
        }
        Section::Motion => vec![" "],
        Section::Board | Section::Activity => {
            vec!["ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight"]
        }
        _ => Vec::new(),
    }
}

fn main() -> std::io::Result<()> {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(7, |since| since.as_secs());
    let live: SharedLive = Arc::new((
        Mutex::new(Live {
            metrics: Metrics::new(seed),
            wanted: 0,
            paused: false,
        }),
        Condvar::new(),
    ));
    let ticker: SharedTicker = Arc::new((Mutex::new(Ticker { rate: 0 }), Condvar::new()));
    thinkterm_plugin_sdk::run_with(|emitter| {
        let threads = [
            ("metrics", {
                let (live, emitter) = (Arc::clone(&live), emitter.clone());
                Box::new(move || live::tick_metrics(live, emitter)) as Box<dyn FnOnce() + Send>
            }),
            ("motion", {
                let (ticker, emitter) = (Arc::clone(&ticker), emitter.clone());
                Box::new(move || live::tick_motion(ticker, emitter)) as Box<dyn FnOnce() + Send>
            }),
        ];
        for (name, work) in threads {
            if let Err(err) = std::thread::Builder::new().name(name.into()).spawn(work) {
                eprintln!("showcase: cannot start the {name} thread: {err}");
            }
        }
        Showcase::new(Some(emitter), live, ticker)
    })
}

#[cfg(test)]
mod tests;
