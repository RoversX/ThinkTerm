//! A pane that holds a plugin's page in place of a terminal.
//!
//! The mux keeps a page as it keeps a terminal -- in a tab, in a stack,
//! beside others, in the saved layout -- and knows nothing of what the page
//! shows: no program runs here, nothing is read or parsed, and no line is
//! ever kept. Each client that has the pane on show asks the plugin, on the
//! machine the pane is on, for what to draw there. So a page costs the mux
//! what its description weighs, and no thread.

use crate::domain::{DomainId, LocalDomain};
use crate::pane::{
    CachePolicy, CloseReason, ForEachPaneLogicalLine, LogicalLine, Pane, PaneId, WithPaneLines,
};
use crate::renderable::*;
use crate::{Mux, MuxNotification};
use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};
use rangeset::RangeSet;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use termwiz::surface::{CursorVisibility, Line, SequenceNo, SEQ_ZERO};
use thinkterm_proto::{PageEdit, PageSpec};
use url::Url;
use wezterm_term::color::ColorPalette;
use wezterm_term::{KeyCode, KeyModifiers, MouseEvent, StableRowIndex, TerminalSize};

/// How often a page that changes tells the mux so, at most: a plugin that
/// changes its page with each frame it draws has that told to every client,
/// and the layout kept, no more often than this.
const CHANGE_TOLD_EVERY: Duration = Duration::from_millis(250);

pub struct PagePane {
    pane_id: PaneId,
    domain_id: DomainId,
    /// What its plugin changes of it is changed here.
    page: Mutex<PageSpec>,
    /// When the mux was last told the page changed, and whether a change
    /// made since waits to be told.
    told: Mutex<(Option<Instant>, bool)>,
    /// The room the pane has, in the cells a terminal there would have: it
    /// is sized with the terminals beside it, and with the others of its
    /// stack.
    size: Mutex<TerminalSize>,
    dead: AtomicBool,
    /// What is written to it is turned away.
    writer: Mutex<NoTerminal>,
}

/// Where what is written to a page goes: nowhere, and whoever wrote it is
/// told, so that text meant for a terminal is not taken for delivered.
pub struct NoTerminal(pub String);

impl std::io::Write for NoTerminal {
    fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, self.0.clone()))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl PagePane {
    /// The pane of `page`, of `domain`: the one its tab's terminals are of,
    /// so that what is opened beside it or moved onto it finds the domain
    /// it expects.
    pub fn new(pane_id: PaneId, domain_id: DomainId, size: TerminalSize, page: PageSpec) -> Self {
        let why = page.no_terminal(pane_id);
        Self {
            pane_id,
            domain_id,
            page: Mutex::new(page),
            told: Mutex::new((None, false)),
            size: Mutex::new(size),
            dead: AtomicBool::new(false),
            writer: Mutex::new(NoTerminal(why)),
        }
    }

    fn no_terminal(&self) -> anyhow::Error {
        anyhow::anyhow!("{}", self.writer.lock().0)
    }

    /// Tells the mux the page changed: at once, unless it was told less
    /// than [`CHANGE_TOLD_EVERY`] ago -- then once that is over, of all that
    /// changed meanwhile.
    fn tell_changed(&self) {
        let Some(mux) = Mux::try_get() else {
            return;
        };
        let now = Instant::now();
        let mut told = self.told.lock();
        let (at, waiting) = &mut *told;
        if *waiting {
            return;
        }
        let due = at.map_or(now, |at| at + CHANGE_TOLD_EVERY);
        if due <= now {
            *at = Some(now);
            drop(told);
            return mux.notify(MuxNotification::PageChanged(self.pane_id));
        }
        *waiting = true;
        let pane_id = self.pane_id;
        promise::spawn::spawn_into_main_thread(async move {
            smol::Timer::after(due - now).await;
            let Some(mux) = Mux::try_get() else {
                return;
            };
            let Some(pane) = mux.get_pane(pane_id) else {
                return;
            };
            if let Some(page) = pane.downcast_ref::<PagePane>() {
                *page.told.lock() = (Some(Instant::now()), false);
            }
            mux.notify(MuxNotification::PageChanged(pane_id));
        })
        .detach();
    }
}

/// `page` as `mux` opens it in a pane of its own, beside pane `beside`:
/// the pane in focus, or the one whose place or side it takes. One opened
/// anew is given an id and the place the user was -- `beside`'s terminal
/// and its directory, or for a page beside another page, where that one
/// was opened from. One that has an id already -- restored, handed to this
/// server, opened here for another mux that holds no pane -- keeps all it
/// has, unless a page here has that id: then it is given another.
pub fn opened(mux: &Mux, mut page: PageSpec, beside: Option<PaneId>) -> PageSpec {
    let taken = |id: &str| {
        mux.iter_panes()
            .iter()
            .any(|pane| pane.page().is_some_and(|other| other.id == id))
    };
    if !page.id.is_empty() {
        if taken(&page.id) {
            page.id = new_page_id();
        }
        return page;
    }
    page.id = new_page_id();
    page.cwd = String::new();
    page.terminal = None;
    let Some(beside) = beside.and_then(|pane_id| mux.get_pane(pane_id)) else {
        return page;
    };
    match beside.page() {
        Some(other) => {
            page.cwd = other.cwd;
            page.terminal = other.terminal;
        }
        None => {
            page.terminal = Some(beside.pane_id() as u64);
            if runs_here(mux, &beside) {
                // Read as it stands: a terminal busy printing is not waited on.
                page.cwd = beside
                    .working_dir_without_waiting(CachePolicy::AllowStale)
                    .and_then(|url| path_of(&url))
                    .unwrap_or_default();
            }
        }
    }
    page
}

/// Whether the programs of pane `pane` run on this machine, where the
/// plugin of a page kept here does, so that its directory names one the
/// plugin can read: not inside WSL or a container, nor on a host its
/// domain reaches over SSH.
fn runs_here(mux: &Mux, pane: &Arc<dyn Pane>) -> bool {
    mux.get_domain(pane.domain_id()).is_some_and(|domain| {
        domain
            .downcast_ref::<LocalDomain>()
            .is_some_and(LocalDomain::is_plain_local)
    })
}

/// A page's id, new: sixteen hex digits, drawn at random so that no two
/// pages any mux opened share one -- and a page restored is not taken for
/// one that came after it.
fn new_page_id() -> String {
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        // No randomness to be had: the time and a count will tell this
        // mux's pages apart.
        static OPENED: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos() as u64);
        bytes = (nanos ^ OPENED.fetch_add(1, Ordering::Relaxed).rotate_left(48)).to_be_bytes();
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The path a terminal's directory names, when it names one: `file:` and
/// the path, as the programs in it report it.
fn path_of(url: &url::Url) -> Option<String> {
    if url.scheme() != "file" {
        return None;
    }
    let path = percent_encoding::percent_decode_str(url.path())
        .decode_utf8()
        .ok()?;
    // `/C:/Users/...` on Windows.
    let path = match path.strip_prefix('/') {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => rest,
        _ => &path,
    };
    (!path.is_empty()).then(|| path.to_string())
}

impl Pane for PagePane {
    fn pane_id(&self) -> PaneId {
        self.pane_id
    }

    fn page(&self) -> Option<PageSpec> {
        Some(self.page.lock().clone())
    }

    fn change_page(&self, edit: PageEdit) -> anyhow::Result<()> {
        {
            let mut page = self.page.lock();
            let edited = page.edited(&edit).map_err(|why| anyhow::anyhow!("{why}"))?;
            if edited == *page {
                return Ok(());
            }
            *page = edited;
        }
        self.tell_changed();
        Ok(())
    }

    fn is_page(&self) -> bool {
        true
    }

    fn get_cursor_position(&self) -> StableCursorPosition {
        StableCursorPosition {
            visibility: CursorVisibility::Hidden,
            ..Default::default()
        }
    }

    fn get_current_seqno(&self) -> SequenceNo {
        SEQ_ZERO
    }

    fn get_changed_since(
        &self,
        _lines: Range<StableRowIndex>,
        _seqno: SequenceNo,
    ) -> RangeSet<StableRowIndex> {
        RangeSet::new()
    }

    /// The rows asked for that a terminal this size would have on its
    /// screen, blank: made for whoever asks and kept by nobody.
    fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
        let size = *self.size.lock();
        let rows = size.rows as StableRowIndex;
        let first = lines.start.clamp(0, rows);
        let end = lines.end.clamp(first, rows);
        let blank = (first..end)
            .map(|_| Line::with_width(size.cols, SEQ_ZERO))
            .collect();
        (first, blank)
    }

    fn with_lines_mut(&self, lines: Range<StableRowIndex>, with_lines: &mut dyn WithPaneLines) {
        let (first, mut blank) = self.get_lines(lines);
        let mut refs: Vec<&mut Line> = blank.iter_mut().collect();
        with_lines.with_lines_mut(first, &mut refs);
    }

    fn for_each_logical_line_in_stable_range_mut(
        &self,
        lines: Range<StableRowIndex>,
        for_line: &mut dyn ForEachPaneLogicalLine,
    ) {
        crate::pane::impl_for_each_logical_line_via_get_logical_lines(self, lines, for_line)
    }

    fn get_logical_lines(&self, lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
        crate::pane::impl_get_logical_lines_via_get_lines(self, lines)
    }

    fn get_dimensions(&self) -> RenderableDimensions {
        let size = *self.size.lock();
        RenderableDimensions {
            cols: size.cols,
            viewport_rows: size.rows,
            scrollback_rows: size.rows,
            physical_top: 0,
            scrollback_top: 0,
            dpi: size.dpi,
            pixel_width: size.pixel_width,
            pixel_height: size.pixel_height,
            reverse_video: false,
        }
    }

    /// What whoever opened the page called it, or its plugin's id: a
    /// client that knows the plugin shows its name.
    fn get_title(&self) -> String {
        self.page.lock().name().to_string()
    }

    fn send_paste(&self, _text: &str) -> anyhow::Result<()> {
        Err(self.no_terminal())
    }

    fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
        Ok(None)
    }

    fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
        MutexGuard::map(self.writer.lock(), |writer| {
            let writer: &mut dyn std::io::Write = writer;
            writer
        })
    }

    fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
        *self.size.lock() = size;
        Ok(())
    }

    fn key_down(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
        Err(self.no_terminal())
    }

    fn key_up(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
        Ok(())
    }

    fn mouse_event(&self, _event: MouseEvent) -> anyhow::Result<()> {
        Ok(())
    }

    fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
    }

    fn kill(&self) {
        self.dead.store(true, Ordering::Release);
    }

    fn palette(&self) -> ColorPalette {
        ColorPalette::default()
    }

    fn domain_id(&self) -> DomainId {
        self.domain_id
    }

    /// Nothing is lost with a page: what it shows is the plugin's.
    fn can_close_without_prompting(&self, _reason: CloseReason) -> bool {
        true
    }

    fn is_mouse_grabbed(&self) -> bool {
        false
    }

    fn is_alt_screen_active(&self) -> bool {
        false
    }

    fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<Url> {
        None
    }

    // No agent and no program is looked for in a page: the tables these
    // read are filled from what runs in a terminal.
    fn agent_status(&self) -> Option<thinkterm_proto::AgentStatus> {
        None
    }

    fn agent_status_summary(&self) -> Option<crate::agent_status::AgentStatusSummary> {
        None
    }

    fn foreground_program(&self) -> Option<thinkterm_proto::ForegroundProgram> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Domain;

    fn page() -> PagePane {
        let size = TerminalSize {
            rows: 4,
            cols: 10,
            ..Default::default()
        };
        let spec = PageSpec {
            plugin: "board".into(),
            args: "{\"board\":\"release\"}".into(),
            title: "Board".into(),
            ..Default::default()
        };
        PagePane::new(7, 3, size, spec)
    }

    #[test]
    fn a_page_says_what_it_is_and_holds_no_terminal() {
        let pane = page();
        assert_eq!(pane.page().unwrap().plugin, "board");
        assert_eq!(pane.get_title(), "Board");
        assert_eq!(pane.domain_id(), 3);
        assert!(pane.reader().unwrap().is_none(), "nothing to read: no thread");
        // What is meant for a terminal is turned away, and says so.
        let why = pane.send_paste("ls\n").unwrap_err().to_string();
        assert_eq!(why, "pane 7 is a page of board, not a terminal");
        assert!(pane.writer().write_all(b"ls\n").is_err());
        assert!(pane.key_down(KeyCode::Enter, KeyModifiers::NONE).is_err());
        let untitled = PagePane::new(8, 3, TerminalSize::default(), PageSpec {
            plugin: "board".into(),
            ..Default::default()
        });
        assert_eq!(untitled.get_title(), "board");
        assert_eq!(pane.get_cursor_position().visibility, CursorVisibility::Hidden);
        assert!(pane.can_close_without_prompting(CloseReason::Pane));
        assert!(!pane.is_dead());
        pane.kill();
        assert!(pane.is_dead());
    }

    #[test]
    fn its_plugin_changes_its_title_and_what_it_opens_with_but_not_whose_it_is() {
        let pane = page();
        let edit = PageEdit {
            title: Some("Release board".into()),
            args: Some("{\"board\":\"release\",\"card\":12}".into()),
            ..Default::default()
        };
        pane.change_page(edit).unwrap();
        let page = pane.page().unwrap();
        assert_eq!(pane.get_title(), "Release board");
        assert_eq!(page.args, "{\"board\":\"release\",\"card\":12}");
        assert_eq!(page.plugin, "board");
        let too_long = PageEdit {
            title: Some("t".repeat(1000)),
            ..Default::default()
        };
        assert!(pane.change_page(too_long).is_err());
        assert_eq!(pane.get_title(), "Release board", "a change refused is no change");
    }

    /// A terminal, as far as a page opened beside it asks: a page that says
    /// it holds none, in a directory.
    struct Terminal(PagePane, &'static str);

    impl Pane for Terminal {
        fn pane_id(&self) -> PaneId {
            self.0.pane_id()
        }
        fn get_cursor_position(&self) -> StableCursorPosition {
            self.0.get_cursor_position()
        }
        fn get_current_seqno(&self) -> SequenceNo {
            self.0.get_current_seqno()
        }
        fn get_changed_since(
            &self,
            lines: Range<StableRowIndex>,
            seqno: SequenceNo,
        ) -> RangeSet<StableRowIndex> {
            self.0.get_changed_since(lines, seqno)
        }
        fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
            self.0.get_lines(lines)
        }
        fn with_lines_mut(&self, lines: Range<StableRowIndex>, with: &mut dyn WithPaneLines) {
            self.0.with_lines_mut(lines, with)
        }
        fn for_each_logical_line_in_stable_range_mut(
            &self,
            lines: Range<StableRowIndex>,
            for_line: &mut dyn ForEachPaneLogicalLine,
        ) {
            self.0.for_each_logical_line_in_stable_range_mut(lines, for_line)
        }
        fn get_logical_lines(&self, lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
            self.0.get_logical_lines(lines)
        }
        fn get_dimensions(&self) -> RenderableDimensions {
            self.0.get_dimensions()
        }
        fn get_title(&self) -> String {
            "zsh".into()
        }
        fn send_paste(&self, text: &str) -> anyhow::Result<()> {
            self.0.send_paste(text)
        }
        fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
            Ok(None)
        }
        fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
            self.0.writer()
        }
        fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
            self.0.resize(size)
        }
        fn key_down(&self, key: KeyCode, mods: KeyModifiers) -> anyhow::Result<()> {
            self.0.key_down(key, mods)
        }
        fn key_up(&self, key: KeyCode, mods: KeyModifiers) -> anyhow::Result<()> {
            self.0.key_up(key, mods)
        }
        fn mouse_event(&self, event: MouseEvent) -> anyhow::Result<()> {
            self.0.mouse_event(event)
        }
        fn is_dead(&self) -> bool {
            false
        }
        fn palette(&self) -> ColorPalette {
            ColorPalette::default()
        }
        fn domain_id(&self) -> DomainId {
            self.0.domain_id()
        }
        fn is_mouse_grabbed(&self) -> bool {
            false
        }
        fn is_alt_screen_active(&self) -> bool {
            false
        }
        fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<Url> {
            Url::parse(self.1).ok()
        }
    }

    #[test]
    fn a_page_opened_beside_a_terminal_is_opened_from_it_and_its_directory() {
        let mux = Mux::new(None);
        let local: Arc<dyn Domain> = Arc::new(LocalDomain::new("local").unwrap());
        mux.add_domain(&local);
        let terminal = PagePane::new(5, local.domain_id(), TerminalSize::default(), PageSpec::default());
        let terminal: Arc<dyn Pane> =
            Arc::new(Terminal(terminal, "file:///home/user/my%20project"));
        mux.add_pane(&terminal).unwrap();
        let fresh = PageSpec {
            plugin: "board".into(),
            ..Default::default()
        };
        let page = opened(&mux, fresh.clone(), Some(5));
        assert_eq!(page.terminal, Some(5));
        assert_eq!(page.cwd, "/home/user/my project");
        // A terminal whose programs run elsewhere: its directory names
        // nothing here, where the page's plugin runs.
        let serial = config::SerialDomain {
            name: "serial".into(),
            ..Default::default()
        };
        let elsewhere: Arc<dyn Domain> = Arc::new(LocalDomain::new_serial_domain(serial).unwrap());
        mux.add_domain(&elsewhere);
        let terminal = PagePane::new(6, elsewhere.domain_id(), TerminalSize::default(), PageSpec::default());
        let terminal: Arc<dyn Pane> = Arc::new(Terminal(terminal, "file:///home/user/project"));
        mux.add_pane(&terminal).unwrap();
        let page = opened(&mux, fresh.clone(), Some(6));
        assert_eq!((page.cwd.as_str(), page.terminal), ("", Some(6)));
        // Opened from nowhere, or beside a pane gone: from no terminal.
        assert_eq!(opened(&mux, fresh.clone(), None).terminal, None);
        assert_eq!(opened(&mux, fresh, Some(9)).cwd, "");
    }

    #[test]
    fn a_page_opened_anew_is_given_an_id_and_one_opened_again_keeps_its_own() {
        let mux = Mux::new(None);
        let fresh = PageSpec {
            plugin: "board".into(),
            cwd: "/somewhere/else".into(),
            terminal: Some(99),
            ..Default::default()
        };
        let first = opened(&mux, fresh.clone(), None);
        assert_eq!(first.id.len(), 16);
        // Where it was opened from is the mux's to say.
        assert_eq!((first.cwd.as_str(), first.terminal), ("", None));
        let held = PageSpec {
            cwd: "/home/user/project".into(),
            terminal: Some(4),
            ..first.clone()
        };
        let pane: Arc<dyn Pane> = Arc::new(PagePane::new(7, 3, TerminalSize::default(), held));
        mux.add_pane(&pane).unwrap();
        // Beside a page: where that one was opened from.
        let beside = opened(&mux, fresh.clone(), Some(7));
        assert_ne!(beside.id, first.id);
        assert_eq!((beside.cwd.as_str(), beside.terminal), ("/home/user/project", Some(4)));
        // Restored, or handed over: all it has stays...
        let kept = PageSpec {
            id: "0f3a9c2e5b7d1846".into(),
            cwd: "/srv".into(),
            ..fresh.clone()
        };
        assert_eq!(opened(&mux, kept.clone(), Some(7)), kept);
        // ...unless a page here has its id.
        let again = opened(&mux, first.clone(), None);
        assert_ne!(again.id, first.id);
        assert_eq!(again.args, first.args);
    }

    #[test]
    fn a_page_id_is_sixteen_hex_digits_and_new_each_time() {
        let (one, two) = (new_page_id(), new_page_id());
        assert_eq!(one.len(), 16);
        assert!(one.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        assert_ne!(one, two);
        let page = PageSpec {
            plugin: "board".into(),
            id: one,
            ..Default::default()
        };
        assert_eq!(page.refusal(), None, "a page's id as the page may have it");
    }

    #[test]
    fn a_terminals_directory_is_the_path_its_url_names() {
        let url = |text: &str| url::Url::parse(text).unwrap();
        assert_eq!(path_of(&url("file:///home/user/a%20b")), Some("/home/user/a b".into()));
        assert_eq!(path_of(&url("file://devbox/home/user")), Some("/home/user".into()));
        assert_eq!(path_of(&url("file:///C:/Users/user")), Some("C:/Users/user".into()));
        assert_eq!(path_of(&url("https://example.com/x")), None);
    }

    #[test]
    fn it_has_a_terminals_room_and_blank_rows_for_whoever_asks() {
        let pane = page();
        let (first, rows) = pane.get_lines(1..3);
        assert_eq!((first, rows.len()), (1, 2));
        assert_eq!(rows[0].len(), 10);
        let (first, rows) = pane.get_lines(2..99);
        assert_eq!((first, rows.len()), (2, 2), "held to its screen");
        assert!(pane.get_lines(-5..0).1.is_empty());
        pane.resize(TerminalSize {
            rows: 2,
            cols: 6,
            ..Default::default()
        })
        .unwrap();
        let dims = pane.get_dimensions();
        assert_eq!((dims.cols, dims.viewport_rows), (6, 2));
        assert_eq!(pane.get_lines(0..9).1.len(), 2);
    }
}
