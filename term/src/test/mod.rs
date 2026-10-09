//! Various tests of the terminal model and escape sequence
//! processing routines.

use super::*;
mod c0;
use bitflags::bitflags;
mod c1;
mod csi;
mod graphics;
mod mouse;
#[cfg(feature = "use_serde")]
mod snapshot;
// mod selection; FIXME: port to render layer
use crate::color::ColorPalette;
use k9::assert_equal as assert_eq;
use std::sync::{Arc, Mutex};
use wezterm_escape_parser::csi::{Edit, EraseInDisplay, EraseInLine};
use wezterm_escape_parser::{OneBased, OperatingSystemCommand, CSI};
use wezterm_surface::{CursorShape, CursorVisibility, SequenceNo, SEQ_ZERO};

#[derive(Debug)]
struct LocalClip {
    clip: Mutex<Option<String>>,
}

impl LocalClip {
    fn new() -> Self {
        Self {
            clip: Mutex::new(None),
        }
    }
}

impl Clipboard for LocalClip {
    fn set_contents(
        &self,
        _selection: ClipboardSelection,
        clip: Option<String>,
    ) -> anyhow::Result<()> {
        *self.clip.lock().unwrap() = clip;
        Ok(())
    }
}

struct TestTerm {
    term: Terminal,
}

#[derive(Debug)]
struct TestTermConfig {
    scrollback: usize,
}
impl TerminalConfiguration for TestTermConfig {
    fn scrollback_size(&self) -> usize {
        self.scrollback
    }

    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }
}

impl TestTerm {
    fn new(height: usize, width: usize, scrollback: usize) -> Self {
        let _ = env_logger::Builder::new()
            .is_test(true)
            .filter_level(log::LevelFilter::Trace)
            .try_init();

        let mut term = Terminal::new(
            TerminalSize {
                rows: height,
                cols: width,
                pixel_width: width * 8,
                pixel_height: height * 16,
                dpi: 0,
            },
            Arc::new(TestTermConfig { scrollback }),
            "ThinkTerm",
            "O_o",
            Box::new(Vec::new()),
        );
        let clip: Arc<dyn Clipboard> = Arc::new(LocalClip::new());
        term.set_clipboard(&clip);

        let mut term = Self { term };

        term.set_auto_wrap(true);

        term
    }

    fn print<B: AsRef<[u8]>>(&mut self, bytes: B) {
        self.term.advance_bytes(bytes);
    }

    fn set_mode(&mut self, mode: &str, enable: bool) {
        self.print(CSI);
        self.print(mode);
        self.print(if enable { b"h" } else { b"l" });
    }

    fn set_auto_wrap(&mut self, enable: bool) {
        self.set_mode("?7", enable);
    }

    fn set_left_and_right_margins(&mut self, left: usize, right: usize) {
        self.print(CSI);
        self.print(format!("{};{}s", left + 1, right + 1));
    }

    fn set_scroll_region(&mut self, top: usize, bottom: usize) {
        self.print(CSI);
        self.print(format!("{};{}r", top + 1, bottom + 1));
    }

    fn delete_lines(&mut self, n: isize) {
        self.print(CSI);
        self.print(format!("{}M", n));
    }

    fn cup(&mut self, col: isize, row: isize) {
        self.print(CSI);
        self.print(format!("{};{}H", row + 1, col + 1));
    }

    fn hvp(&mut self, col: isize, row: isize) {
        self.print(CSI);
        self.print(format!("{};{}f", row + 1, col + 1));
    }

    fn erase_in_display(&mut self, erase: EraseInDisplay) {
        let csi = CSI::Edit(Edit::EraseInDisplay(erase));
        self.print(format!("{}", csi));
    }

    fn erase_in_line(&mut self, erase: EraseInLine) {
        let csi = CSI::Edit(Edit::EraseInLine(erase));
        self.print(format!("{}", csi));
    }

    fn hyperlink(&mut self, link: &Arc<Hyperlink>) {
        let osc = OperatingSystemCommand::SetHyperlink(Some(link.as_ref().clone()));
        self.print(format!("{}", osc));
    }

    fn hyperlink_off(&mut self) {
        self.print("\x1b]8;;\x1b\\");
    }

    fn soft_reset(&mut self) {
        self.print(CSI);
        self.print("!p");
    }

    fn assert_cursor_pos(&self, x: usize, y: i64, reason: Option<&str>, seqno: Option<SequenceNo>) {
        let cursor = self.cursor_pos();
        let expect = CursorPosition {
            x,
            y,
            shape: CursorShape::Default,
            visibility: CursorVisibility::Visible,
            seqno: seqno.unwrap_or_else(|| self.current_seqno()),
        };
        assert_eq!(
            cursor, expect,
            "actual cursor (left) didn't match expected cursor (right) reason={:?}",
            reason
        );
    }

    fn assert_dirty_lines(&self, seqno: SequenceNo, expected: &[usize], reason: Option<&str>) {
        let mut seqs = vec![];
        let mut dirty_indices = vec![];

        self.screen().for_each_phys_line(|i, line| {
            seqs.push(line.current_seqno());
            if line.changed_since(seqno) {
                dirty_indices.push(i);
            }
        });
        assert_eq!(
            &dirty_indices, &expected,
            "actual dirty lines (left) didn't match expected dirty \
             lines (right) reason={:?}. threshold seq: {} seqs: {:?}",
            reason, seqno, seqs
        );
    }
}

impl Deref for TestTerm {
    type Target = Terminal;

    fn deref(&self) -> &Terminal {
        &self.term
    }
}

impl DerefMut for TestTerm {
    fn deref_mut(&mut self) -> &mut Terminal {
        &mut self.term
    }
}

#[test]
fn agent_osc_evidence_tracks_only_real_emissions() {
    let mut term = TestTerm::new(4, 20, 0);

    // A fresh terminal has a displayable title but zero evidence: the
    // distinction is the whole point of the evidence channel.
    let fresh = term.agent_osc_evidence();
    assert_eq!(fresh.title, None);
    assert_eq!(fresh.progress, None);
    assert!(!term.get_title().is_empty());

    term.print("\x1b]0;\u{25d0} fix the tests\x07");
    assert_eq!(
        term.agent_osc_evidence().title.as_deref(),
        Some("\u{25d0} fix the tests")
    );

    // OSC 1 names only the icon and shells emit it freely: it must not
    // create evidence, and an empty one must not erase what an agent
    // said via OSC 0/2.
    term.print("\x1b]1;shell-icon\x07");
    assert_eq!(
        term.agent_osc_evidence().title.as_deref(),
        Some("\u{25d0} fix the tests")
    );
    term.print("\x1b]1;\x07");
    assert_eq!(
        term.agent_osc_evidence().title.as_deref(),
        Some("\u{25d0} fix the tests")
    );

    term.print("\x1b]9;4;3\x07");
    assert_eq!(term.agent_osc_evidence().progress.as_deref(), Some("4;3"));
    // Explicit clear is retained as "4;0" — distinct from never-reported.
    term.print("\x1b]9;4;0\x07");
    assert_eq!(term.agent_osc_evidence().progress.as_deref(), Some("4;0"));
    // A pause is retained pre-flattening even though the display path
    // folds it into Progress::None.
    term.print("\x1b]9;4;4\x07");
    assert_eq!(term.agent_osc_evidence().progress.as_deref(), Some("4;4"));

    // Clearing evidence leaves the display title alone.
    let shown = term.get_title().to_string();
    term.clear_agent_osc_state();
    let cleared = term.agent_osc_evidence();
    assert_eq!(cleared.title, None);
    assert_eq!(cleared.progress, None);
    assert_eq!(term.get_title(), shown);

    // Hard reset drops evidence too.
    term.print("\x1b]2;busy\x07\x1b]9;4;3\x07");
    assert!(term.agent_osc_evidence().title.is_some());
    term.print("\x1bc");
    assert_eq!(term.agent_osc_evidence(), Default::default());
}

/// OSC 7501: the query is answered, reports are kept, and each of the
/// protocol's ends for a record -- a new prompt, being seen, a hard reset
/// -- does what it should.
#[cfg(not(target_family = "wasm"))]
#[test]
fn program_status_reports_are_answered_kept_and_retired() {
    use crate::program_status::ProgramState;

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let sink = Sink::default();
    let mut term = Terminal::new(
        TerminalSize {
            rows: 4,
            cols: 20,
            pixel_width: 160,
            pixel_height: 64,
            dpi: 0,
        },
        Arc::new(TestTermConfig { scrollback: 0 }),
        "ThinkTerm",
        "O_o",
        Box::new(sink.clone()),
    );

    term.advance_bytes("\x1b]7501;?\x07");
    let reply = b"\x1b]7501;?\x1b\\";
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while sink.0.lock().unwrap().len() < reply.len() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(sink.0.lock().unwrap().as_slice(), reply);

    // "Running tests"
    term.advance_bytes(
        "\x1b]7501;state=working:app=claude-code:progress=60:msg=UnVubmluZyB0ZXN0cw\x1b\\\
         \x1b]7501;state=blocked:id=plan:kind=question\x1b\\\
         \x1b]7501;state=done:id=explore\x1b\\",
    );
    let snapshot = term.program_status(usize::MAX);
    let root = snapshot.root.unwrap();
    assert_eq!(root.state, ProgramState::Working);
    assert_eq!(root.progress, Some(60));
    assert_eq!(root.msg.as_deref(), Some("Running tests"));
    assert_eq!(snapshot.children.len(), 2);
    assert_eq!(snapshot.children[1].app.as_deref(), Some("claude-code"));

    // The shell's next prompt: only the unseen result is left.
    term.advance_bytes("\x1b]133;A\x07");
    let snapshot = term.program_status(usize::MAX);
    assert_eq!(snapshot.root, None);
    assert_eq!(snapshot.children.len(), 1);
    assert_eq!(snapshot.children[0].state, ProgramState::Done);
    assert!(snapshot.children[0].orphaned);

    assert!(term.program_status_seen());
    assert_eq!(term.program_status(usize::MAX), Default::default());

    term.advance_bytes("\x1b]7501;state=error\x1b\\");
    assert!(term.program_status(usize::MAX).root.is_some());
    term.advance_bytes("\x1bc");
    assert_eq!(term.program_status(usize::MAX), Default::default());
}

#[test]
fn application_palette_override_tracks_osc_set_query_and_reset() {
    let mut term = TestTerm::new(4, 8, 0);
    assert!(term.palette_override().is_none());

    // A query is observational and must not make the configured palette
    // authoritative on remote renderers.
    term.print("\x1b]10;?\x07");
    assert!(term.palette_override().is_none());

    term.print("\x1b]10;#ffffff\x07");
    let override_palette = term.palette_override().expect("OSC 10 override");
    assert_eq!(override_palette.foreground, (1.0, 1.0, 1.0, 1.0).into());

    term.print("\x1b]110\x07");
    assert!(term.palette_override().is_none());

    term.print("\x1b]10;#ffffff\x07");
    assert!(term.palette_override().is_some());
    term.print("\x1bc");
    assert!(term.palette_override().is_none());
}

/// Asserts that both line slices match according to the
/// selected flags.
fn assert_lines_equal(
    file: &str,
    line_no: u32,
    lines: &[Line],
    expect_lines: &[Line],
    compare: Compare,
) {
    let mut expect_iter = expect_lines.iter();

    println!("actual_lines:");
    for line in lines {
        println!("[{}]", line.as_str());
    }
    println!("expect_lines");
    for line in expect_lines {
        println!("[{}]", line.as_str());
    }

    for (idx, line) in lines.iter().enumerate() {
        let expect = match expect_iter.next() {
            Some(e) => e,
            None => break,
        };

        if compare.contains(Compare::ATTRS) {
            let line_attrs: Vec<_> = line.visible_cells().map(|c| c.attrs().clone()).collect();
            let expect_attrs: Vec<_> = expect.visible_cells().map(|c| c.attrs().clone()).collect();
            assert_eq!(
                expect_attrs,
                line_attrs,
                "{}:{}: line {} `{}` attrs didn't match (left=expected, right=actual)",
                file,
                line_no,
                idx,
                line.as_str()
            );
        }
        if compare.contains(Compare::TEXT) {
            let line_str = line.as_str();
            let expect_str = expect.as_str();
            assert_eq!(
                line_str,
                expect_str,
                "{}:{}: line {} text didn't match '{}' vs '{}'",
                file,
                line_no,
                idx,
                line_str.escape_default(),
                expect_str.escape_default()
            );
        }
    }

    assert_eq!(
        lines.len(),
        expect_lines.len(),
        "{}:{}: expectation has wrong number of lines",
        file,
        line_no
    );
}

bitflags! {
    struct Compare : u8{
        const TEXT = 1;
        const ATTRS = 2;
        const DIRTY = 4;
    }
}

fn print_all_lines(term: &Terminal) {
    let screen = term.screen();

    println!("whole screen contents are:");
    screen.for_each_phys_line(|_, line| {
        println!("[{}]", line.as_str());
    });
}

fn print_visible_lines(term: &Terminal) {
    let screen = term.screen();

    println!("screen contents are:");
    for line in screen.visible_lines().iter() {
        println!("[{}]", line.as_str());
    }
}

/// Asserts that the visible lines of the terminal have the
/// same character contents as the expected lines.
/// The other cell attributes are not compared; this is
/// a convenience for writing visually understandable tests.
fn assert_visible_contents(term: &Terminal, file: &str, line: u32, expect_lines: &[&str]) {
    print_visible_lines(&term);
    let screen = term.screen();

    let expect: Vec<Line> = expect_lines.iter().map(|s| (*s).into()).collect();

    assert_lines_equal(file, line, &screen.visible_lines(), &expect, Compare::TEXT);
}

fn assert_all_contents(term: &Terminal, file: &str, line: u32, expect_lines: &[&str]) {
    print_all_lines(&term);
    let screen = term.screen();

    let expect: Vec<Line> = expect_lines.iter().map(|s| (*s).into()).collect();

    assert_lines_equal(file, line, &screen.all_lines(), &expect, Compare::TEXT);
}

#[test]
fn test_semantic_1539() {
    use wezterm_escape_parser::osc::FinalTermSemanticPrompt;
    let mut term = TestTerm::new(5, 10, 0);
    term.print(format!(
        "{}prompt\r\nwoot",
        OperatingSystemCommand::FinalTermSemanticPrompt(
            FinalTermSemanticPrompt::MarkEndOfPromptAndStartOfInputUntilEndOfLine
        )
    ));

    assert_visible_contents(&term, file!(), line!(), &["prompt", "woot", "", "", ""]);

    k9::snapshot!(
        term.get_semantic_zones().unwrap(),
        "
[
    SemanticZone {
        start_y: 0,
        start_x: 0,
        end_y: 0,
        end_x: 5,
        semantic_type: Input,
    },
    SemanticZone {
        start_y: 1,
        start_x: 0,
        end_y: 1,
        end_x: 3,
        semantic_type: Output,
    },
]
"
    );
}

#[test]
fn test_semantic() {
    use wezterm_escape_parser::osc::FinalTermSemanticPrompt;
    let mut term = TestTerm::new(5, 10, 0);
    term.print("hello");
    term.print(format!(
        "{}",
        OperatingSystemCommand::FinalTermSemanticPrompt(FinalTermSemanticPrompt::FreshLine)
    ));
    term.print("there");

    assert_visible_contents(&term, file!(), line!(), &["hello", "there", "", "", ""]);

    term.cup(0, 2);
    term.print(format!(
        "{}",
        OperatingSystemCommand::FinalTermSemanticPrompt(FinalTermSemanticPrompt::FreshLine)
    ));
    term.print("three");
    assert_visible_contents(
        &term,
        file!(),
        line!(),
        &["hello", "there", "three", "", ""],
    );

    k9::snapshot!(
        term.get_semantic_zones().unwrap(),
        "
[
    SemanticZone {
        start_y: 0,
        start_x: 0,
        end_y: 2,
        end_x: 4,
        semantic_type: Output,
    },
]
"
    );

    term.print(format!(
        "{}",
        OperatingSystemCommand::FinalTermSemanticPrompt(
            FinalTermSemanticPrompt::FreshLineAndStartPrompt {
                aid: None,
                cl: None
            }
        )
    ));
    term.print("> ");
    term.print(format!(
        "{}",
        OperatingSystemCommand::FinalTermSemanticPrompt(
            FinalTermSemanticPrompt::MarkEndOfPromptAndStartOfInputUntilNextMarker
        )
    ));
    term.print("ls -l\r\n");
    term.print(format!(
        "{}",
        OperatingSystemCommand::FinalTermSemanticPrompt(
            FinalTermSemanticPrompt::MarkEndOfInputAndStartOfOutput { aid: None }
        )
    ));
    term.print("some file");

    let output = CellAttributes::default();
    let mut input = CellAttributes::default();
    input.set_semantic_type(SemanticType::Input);

    let mut prompt_line = Line::from_text("> ls -l", &output, SEQ_ZERO, None);
    for i in 0..2 {
        prompt_line.cells_mut()[i]
            .attrs_mut()
            .set_semantic_type(SemanticType::Prompt);
    }
    for i in 2..7 {
        prompt_line.cells_mut()[i]
            .attrs_mut()
            .set_semantic_type(SemanticType::Input);
    }

    k9::snapshot!(
        term.get_semantic_zones().unwrap(),
        "
[
    SemanticZone {
        start_y: 0,
        start_x: 0,
        end_y: 2,
        end_x: 4,
        semantic_type: Output,
    },
    SemanticZone {
        start_y: 3,
        start_x: 0,
        end_y: 3,
        end_x: 1,
        semantic_type: Prompt,
    },
    SemanticZone {
        start_y: 3,
        start_x: 2,
        end_y: 3,
        end_x: 6,
        semantic_type: Input,
    },
    SemanticZone {
        start_y: 4,
        start_x: 0,
        end_y: 4,
        end_x: 8,
        semantic_type: Output,
    },
]
"
    );

    assert_lines_equal(
        file!(),
        line!(),
        &term.screen().visible_lines(),
        &[
            Line::from_text("hello", &output, SEQ_ZERO, None),
            Line::from_text("there", &output, SEQ_ZERO, None),
            Line::from_text("three", &output, SEQ_ZERO, None),
            prompt_line,
            Line::from_text("some file", &output, SEQ_ZERO, None),
        ],
        Compare::TEXT | Compare::ATTRS,
    );
}

#[test]
fn issue_1161() {
    let mut term = TestTerm::new(1, 5, 0);
    term.print("x\u{3000}x");
    assert_visible_contents(
        &term,
        file!(),
        line!(),
        &[
            // U+3000 is ideographic space, a double-width space
            "x\u{3000}x",
        ],
    );
}

#[test]
fn basic_output() {
    let mut term = TestTerm::new(5, 10, 0);

    term.cup(1, 1);

    term.set_auto_wrap(false);
    term.print("hello, world!");
    assert_visible_contents(&term, file!(), line!(), &["", " hello, w!", "", "", ""]);

    term.set_auto_wrap(true);
    term.erase_in_display(EraseInDisplay::EraseToStartOfDisplay);
    term.cup(1, 1);
    term.print("hello, world!");
    assert_visible_contents(&term, file!(), line!(), &["", " hello, wo", "rld!", "", ""]);

    term.erase_in_display(EraseInDisplay::EraseToStartOfDisplay);
    assert_visible_contents(
        &term,
        file!(),
        line!(),
        &["", "          ", "     ", "", ""],
    );

    term.cup(0, 2);
    term.print("woot");
    term.cup(2, 2);
    term.erase_in_line(EraseInLine::EraseToEndOfLine);
    assert_visible_contents(&term, file!(), line!(), &["", "          ", "wo", "", ""]);

    term.erase_in_line(EraseInLine::EraseToStartOfLine);
    assert_visible_contents(&term, file!(), line!(), &["", "          ", "   ", "", ""]);
}

/// Ensure that we dirty lines as the cursor is moved around, otherwise
/// the renderer won't draw the cursor in the right place
#[test]
fn cursor_movement_damage() {
    let mut term = TestTerm::new(2, 3, 0);

    let seqno = term.current_seqno();
    term.print("fooo.");
    assert_visible_contents(&term, file!(), line!(), &["foo", "o."]);
    term.assert_cursor_pos(2, 1, None, None);
    term.assert_dirty_lines(seqno, &[0, 1], None);

    term.cup(0, 1);

    let seqno = term.current_seqno();
    term.print("\x08");
    term.assert_cursor_pos(0, 1, Some("BS doesn't change the line"), Some(seqno));
    // Since we didn't move, the line isn't dirty
    term.assert_dirty_lines(seqno, &[], None);

    let seqno = term.current_seqno();
    term.cup(0, 0);
    term.assert_dirty_lines(
        seqno,
        &[],
        Some("cursor movement no longer dirties old and new lines"),
    );
    term.assert_cursor_pos(0, 0, None, None);
}
const NUM_COLS: usize = 3;

#[test]
fn scroll_up_within_left_and_right_margins() {
    let ones = "1".repeat(NUM_COLS);
    let twos = "2".repeat(NUM_COLS);
    let threes = "3".repeat(NUM_COLS);
    let fours = "4".repeat(NUM_COLS + 2);
    let fives = "5".repeat(NUM_COLS);

    let mut term = TestTerm::new(5, NUM_COLS + 2, 0);

    term.print(&ones);
    term.print("\r\n");
    term.print(&twos);
    term.print("\r\n");
    term.print(&threes);
    term.print("\r\n");
    term.print(&fours);
    term.print("\r\n");
    term.print(&fives);

    assert_visible_contents(
        &term,
        file!(),
        line!(),
        &["111", "222", "333", "44444", "555"],
    );

    term.set_mode("?69", true); // allow left/right margins to be set
    term.set_left_and_right_margins(1, NUM_COLS + 1);
    term.set_scroll_region(2, 4);
    term.cup(1, 4);
    term.print("\n");
    assert_visible_contents(
        &term,
        file!(),
        line!(),
        &[
            "111",
            "222",
            &format!("3{}", "4".repeat(NUM_COLS + 1)),
            &format!("4{}", "5".repeat(NUM_COLS - 1)),
            &format!("5{}", " ".repeat(NUM_COLS - 1)),
        ],
    );
}

#[test]
fn scroll_down_within_left_and_right_margins() {
    let ones = "1".repeat(NUM_COLS);
    let twos = "2".repeat(NUM_COLS);
    let threes = "3".repeat(NUM_COLS);
    let fours = "4".repeat(NUM_COLS + 2);
    let fives = "5".repeat(NUM_COLS);

    let mut term = TestTerm::new(5, NUM_COLS + 2, 0);

    term.print(&ones);
    term.print("\r\n");
    term.print(&twos);
    term.print("\r\n");
    term.print(&threes);
    term.print("\r\n");
    term.print(&fours);
    term.print("\r\n");
    term.print(&fives);

    assert_visible_contents(
        &term,
        file!(),
        line!(),
        &["111", "222", "333", "44444", "555"],
    );

    term.set_mode("?69", true); // allow left/right margins to be set
    term.set_left_and_right_margins(1, NUM_COLS + 1);
    term.set_scroll_region(2, 5);
    term.cup(1, 2);

    // IL: Insert Line
    term.print(CSI);
    term.print("L");

    assert_visible_contents(
        &term,
        file!(),
        line!(),
        &[
            "111",
            "222",
            &format!("3{}", " ".repeat(NUM_COLS - 1)),
            &format!("4{}", "3".repeat(NUM_COLS - 1)),
            &format!("5{}", "4".repeat(NUM_COLS + 1)),
        ],
    );
}

/// Replicates a bug I initially found via:
/// $ vim
/// :help
/// PageDown
#[test]
fn test_delete_lines() {
    let mut term = TestTerm::new(5, 3, 0);

    let seqno = term.current_seqno();
    term.print("111\r\n222\r\n333\r\n444\r\n555");
    assert_visible_contents(
        &term,
        file!(),
        line!(),
        &["111", "222", "333", "444", "555"],
    );
    term.assert_dirty_lines(seqno, &[0, 1, 2, 3, 4], None);
    term.cup(0, 1);

    let seqno = term.current_seqno();
    term.assert_dirty_lines(seqno, &[], None);
    term.delete_lines(2);
    assert_visible_contents(&term, file!(), line!(), &["111", "444", "555", "", ""]);
    term.assert_dirty_lines(seqno, &[1, 2, 3, 4], None);

    term.cup(0, 3);
    term.print("aaa\r\nbbb");
    term.cup(0, 1);

    let seqno = term.current_seqno();
    assert_visible_contents(
        &term,
        file!(),
        line!(),
        &["111", "444", "555", "aaa", "bbb"],
    );

    // test with a scroll region smaller than the screen
    term.set_scroll_region(1, 3);
    term.cup(0, 1);
    print_all_lines(&term);
    term.delete_lines(2);

    assert_visible_contents(&term, file!(), line!(), &["111", "aaa", "", "", "bbb"]);
    term.assert_dirty_lines(seqno, &[1, 2, 3], None);

    // expand the scroll region to fill the screen
    term.set_scroll_region(0, 4);

    let seqno = term.current_seqno();
    print_all_lines(&term);
    term.delete_lines(1);

    assert_visible_contents(&term, file!(), line!(), &["aaa", "", "", "bbb", ""]);
    term.assert_dirty_lines(seqno, &[4], None);
}

/// Test DEC Special Graphics character set.
#[test]
fn test_dec_special_graphics() {
    let mut term = TestTerm::new(2, 50, 0);

    term.print("\u{1b}(0ABCabcdefghijklmnopqrstuvwxyzDEF\r\n\u{1b}(Bhello");
    assert_visible_contents(
        &term,
        file!(),
        line!(),
        &["ABC▒␉␌␍␊°±␤␋┘┐┌└┼⎺⎻─⎼⎽├┤┴┬│≤≥DEF", "hello"],
    );

    term = TestTerm::new(2, 50, 0);
    term.print("\u{1b})0\u{0e}SO-ABCabcdefghijklmnopqrstuvwxyzDEF\r\n\u{0f}SI-hello");
    assert_visible_contents(
        &term,
        file!(),
        line!(),
        &["SO-ABC▒␉␌␍␊°±␤␋┘┐┌└┼⎺⎻─⎼⎽├┤┴┬│≤≥DEF", "SI-hello"],
    );
}

/// Test double-width / double-height sequences.
#[test]
fn test_dec_double_width() {
    let mut term = TestTerm::new(4, 50, 0);

    term.print("\u{1b}#3line1\r\nline2\u{1b}#4\r\nli\u{1b}#6ne3\r\n\u{1b}#5line4");
    assert_visible_contents(
        &term,
        file!(),
        line!(),
        &["line1", "line2", "line3", "line4"],
    );

    let lines = term.screen().visible_lines();
    assert!(lines[0].is_double_height_top());
    assert!(lines[1].is_double_height_bottom());
    assert!(lines[2].is_double_width());
    assert!(lines[3].is_single_width());
}

/// This test skips over an edge case with cursor positioning,
/// while sizing down, but tries to trip over the same edge
/// case while sizing back up again
#[test]
fn test_resize_2162_by_2_then_up_1() {
    let num_lines = 4;
    let num_cols = 20;

    let mut term = TestTerm::new(num_lines, num_cols, 0);
    term.print("some long long text");
    assert_visible_contents(
        &term,
        file!(),
        line!(),
        &["some long long text", "", "", ""],
    );
    term.assert_cursor_pos(19, 0, None, Some(0));
    term.resize(TerminalSize {
        rows: num_lines,
        cols: num_cols - 2,
        pixel_width: 0,
        pixel_height: 0,
        dpi: 0,
    });
    assert_visible_contents(
        &term,
        file!(),
        line!(),
        &["some long long tex", "t", "", ""],
    );
    eprintln!("check cursor pos 2");
    term.assert_cursor_pos(1, 1, None, Some(6));
    term.resize(TerminalSize {
        rows: num_lines - 1,
        cols: num_cols,
        pixel_width: 0,
        pixel_height: 0,
        dpi: 0,
    });
    assert_visible_contents(&term, file!(), line!(), &["some long long text", "", ""]);
    eprintln!("check cursor pos 3");
    term.assert_cursor_pos(19, 0, None, Some(7));
    term.resize(TerminalSize {
        rows: num_lines,
        cols: num_cols,
        pixel_width: 0,
        pixel_height: 0,
        dpi: 0,
    });
    assert_visible_contents(
        &term,
        file!(),
        line!(),
        &["some long long text", "", "", ""],
    );
    eprintln!("check cursor pos 3");
    term.assert_cursor_pos(19, 0, None, Some(8));
}

/// The alt screen has no scrollback, so shrinking it must not scroll the
/// grid up to chase the cursor: full-screen apps redraw on SIGWINCH, and
/// the bottom-gravity slice briefly showed rows from the middle of the
/// old grid at the top of the pane (visible as flicker while dragging a
/// split). The alt screen is anchored to its top: the bottom is cropped
/// and the cursor clamped into the window.
#[test]
fn test_alt_screen_shrink_is_top_anchored() {
    let mut term = TestTerm::new(6, 10, 100);
    term.print("\x1b[?1049h");
    term.print("aaa\r\nbbb\r\nccc\r\nddd\r\neee\r\nfff");
    assert_visible_contents(
        &term,
        file!(),
        line!(),
        &["aaa", "bbb", "ccc", "ddd", "eee", "fff"],
    );
    term.resize(TerminalSize {
        rows: 4,
        cols: 10,
        pixel_width: 0,
        pixel_height: 0,
        dpi: 0,
    });
    assert_visible_contents(&term, file!(), line!(), &["aaa", "bbb", "ccc", "ddd"]);
    term.assert_cursor_pos(3, 3, None, None);
}

/// Rewrapping on a width change renumbers every physical row; the screen
/// must re-anchor stable_row_index_offset so the cursor row keeps its
/// stable index, or every saved StableRowIndex (GUI scroll viewports,
/// most visibly) silently points at different content after a split or
/// sidebar drag — the "terminal jumped to the top" reports.
#[test]
fn test_rewrap_preserves_cursor_stable_row() {
    let mut term = TestTerm::new(4, 20, 100);
    // Enough wrapped output to overflow the scrollback and accumulate a
    // real stable_row_index_offset first: the anchor is unsigned, so it
    // is exact only once history has scrolled past (the every-day case
    // for a long-lived pane).
    for i in 0..200 {
        term.print(format!("line {i} padding padding\r\n"));
    }
    let cursor = term.cursor_pos();
    let before = term.screen().visible_row_to_stable_row(cursor.y);
    term.resize(TerminalSize {
        rows: 4,
        cols: 10,
        pixel_width: 0,
        pixel_height: 0,
        dpi: 0,
    });
    let cursor = term.cursor_pos();
    let after = term.screen().visible_row_to_stable_row(cursor.y);
    assert_eq!(
        before, after,
        "cursor row must keep its stable index across a rewrap"
    );
}

/// This test skips over an edge case with cursor positioning,
/// so it passes even ahead of a fix for issue 2162.
#[test]
fn test_resize_2162_by_2() {
    let num_lines = 4;
    let num_cols = 20;

    let mut term = TestTerm::new(num_lines, num_cols, 0);
    term.print("some long long text");
    assert_visible_contents(
        &term,
        file!(),
        line!(),
        &["some long long text", "", "", ""],
    );
    term.assert_cursor_pos(19, 0, None, Some(0));
    term.resize(TerminalSize {
        rows: num_lines,
        cols: num_cols - 2,
        pixel_width: 0,
        pixel_height: 0,
        dpi: 0,
    });
    assert_visible_contents(
        &term,
        file!(),
        line!(),
        &["some long long tex", "t", "", ""],
    );
    eprintln!("check cursor pos 2");
    term.assert_cursor_pos(1, 1, None, Some(6));
    term.resize(TerminalSize {
        rows: num_lines,
        cols: num_cols,
        pixel_width: 0,
        pixel_height: 0,
        dpi: 0,
    });
    assert_visible_contents(
        &term,
        file!(),
        line!(),
        &["some long long text", "", "", ""],
    );
    eprintln!("check cursor pos 3");
    term.assert_cursor_pos(19, 0, None, Some(7));
}

/// This case tickles an edge case where the cursor ends
/// up drifting away from where the line wraps and ends up
/// in the wrong place
#[test]
fn test_resize_2162() {
    let num_lines = 4;
    let num_cols = 20;

    let mut term = TestTerm::new(num_lines, num_cols, 0);
    term.print("some long long text");
    assert_visible_contents(
        &term,
        file!(),
        line!(),
        &["some long long text", "", "", ""],
    );
    term.assert_cursor_pos(19, 0, None, Some(0));
    term.resize(TerminalSize {
        rows: num_lines,
        cols: num_cols - 1,
        pixel_width: 0,
        pixel_height: 0,
        dpi: 0,
    });
    assert_visible_contents(
        &term,
        file!(),
        line!(),
        &["some long long text", "", "", ""],
    );
    eprintln!("check cursor pos 2");
    term.assert_cursor_pos(19, 0, None, Some(6));
    term.resize(TerminalSize {
        rows: num_lines,
        cols: num_cols,
        pixel_width: 0,
        pixel_height: 0,
        dpi: 0,
    });
    assert_visible_contents(
        &term,
        file!(),
        line!(),
        &["some long long text", "", "", ""],
    );
    eprintln!("check cursor pos 3");
    term.assert_cursor_pos(19, 0, None, Some(7));
}

/// Test the behavior of wrapped lines when we resize the terminal
/// wider and then narrower.
#[test]
fn test_resize_wrap() {
    const LINES: usize = 8;
    let mut term = TestTerm::new(LINES, 4, 0);
    term.print("111\r\n2222aa\r\n333\r\n");
    assert_visible_contents(
        &term,
        file!(),
        line!(),
        &["111", "2222", "aa", "333", "", "", "", ""],
    );
    term.resize(TerminalSize {
        rows: LINES,
        cols: 5,
        pixel_width: 0,
        pixel_height: 0,
        dpi: 0,
    });
    assert_visible_contents(
        &term,
        file!(),
        line!(),
        &["111", "2222a", "a", "333", "", "", "", ""],
    );
    term.resize(TerminalSize {
        rows: LINES,
        cols: 6,
        pixel_width: 0,
        pixel_height: 0,
        dpi: 0,
    });
    assert_visible_contents(
        &term,
        file!(),
        line!(),
        &["111", "2222aa", "333", "", "", "", "", ""],
    );
    term.resize(TerminalSize {
        rows: LINES,
        cols: 7,
        pixel_width: 0,
        pixel_height: 0,
        dpi: 0,
    });
    assert_visible_contents(
        &term,
        file!(),
        line!(),
        &["111", "2222aa", "333", "", "", "", "", ""],
    );
    term.resize(TerminalSize {
        rows: LINES,
        cols: 8,
        ..Default::default()
    });
    assert_visible_contents(
        &term,
        file!(),
        line!(),
        &["111", "2222aa", "333", "", "", "", "", ""],
    );

    // Resize smaller again
    term.resize(TerminalSize {
        rows: LINES,
        cols: 7,
        ..Default::default()
    });
    assert_visible_contents(
        &term,
        file!(),
        line!(),
        &["111", "2222aa", "333", "", "", "", "", ""],
    );
    term.resize(TerminalSize {
        rows: LINES,
        cols: 6,
        ..Default::default()
    });
    assert_visible_contents(
        &term,
        file!(),
        line!(),
        &["111", "2222aa", "333", "", "", "", "", ""],
    );
    term.resize(TerminalSize {
        rows: LINES,
        cols: 5,
        ..Default::default()
    });
    assert_visible_contents(
        &term,
        file!(),
        line!(),
        &["111", "2222a", "a", "333", "", "", "", ""],
    );
    term.resize(TerminalSize {
        rows: LINES,
        cols: 4,
        ..Default::default()
    });
    assert_visible_contents(
        &term,
        file!(),
        line!(),
        &["111", "2222", "aa", "333", "", "", "", ""],
    );
}

#[test]
fn test_resize_wrap_issue_971() {
    const LINES: usize = 4;
    let mut term = TestTerm::new(LINES, 4, 0);
    term.print("====\r\nSS\r\n");
    assert_visible_contents(&term, file!(), line!(), &["====", "SS", "", ""]);
    term.resize(TerminalSize {
        rows: LINES,
        cols: 6,
        ..Default::default()
    });
    assert_visible_contents(&term, file!(), line!(), &["====", "SS", "", ""]);
}

#[test]
fn test_resize_wrap_sgc_issue_978() {
    const LINES: usize = 4;
    let mut term = TestTerm::new(LINES, 4, 0);
    term.print("\u{1b}(0qqqq\u{1b}(B\r\nSS\r\n");
    assert_visible_contents(&term, file!(), line!(), &["────", "SS", "", ""]);
    term.resize(TerminalSize {
        rows: LINES,
        cols: 6,
        ..Default::default()
    });
    assert_visible_contents(&term, file!(), line!(), &["────", "SS", "", ""]);
}

#[test]
fn test_resize_wrap_dectcm_issue_978() {
    const LINES: usize = 4;
    let mut term = TestTerm::new(LINES, 4, 0);
    term.print("\u{1b}[?25l====\u{1b}[?25h\r\nSS\r\n");
    assert_visible_contents(&term, file!(), line!(), &["====", "SS", "", ""]);
    term.resize(TerminalSize {
        rows: LINES,
        cols: 6,
        ..Default::default()
    });
    assert_visible_contents(&term, file!(), line!(), &["====", "SS", "", ""]);
}

#[test]
fn test_resize_wrap_escape_code_issue_978() {
    const LINES: usize = 4;
    let mut term = TestTerm::new(LINES, 4, 0);
    term.print("====\u{1b}[0m\r\nSS\r\n");
    assert_visible_contents(&term, file!(), line!(), &["====", "SS", "", ""]);
    term.resize(TerminalSize {
        rows: LINES,
        cols: 6,
        ..Default::default()
    });
    assert_visible_contents(&term, file!(), line!(), &["====", "SS", "", ""]);
}

#[test]
fn test_scrollup() {
    let mut term = TestTerm::new(2, 1, 4);
    term.print("1\n");
    assert_all_contents(&term, file!(), line!(), &["1", ""]);
    assert_eq!(term.screen().visible_row_to_stable_row(0), 0);

    term.print("2\n");
    assert_all_contents(&term, file!(), line!(), &["1", "2", ""]);
    assert_eq!(term.screen().visible_row_to_stable_row(0), 1);

    term.print("3\n");
    assert_all_contents(&term, file!(), line!(), &["1", "2", "3", ""]);
    assert_eq!(term.screen().visible_row_to_stable_row(0), 2);

    term.print("4\n");
    assert_all_contents(&term, file!(), line!(), &["1", "2", "3", "4", ""]);
    assert_eq!(term.screen().visible_row_to_stable_row(0), 3);

    term.print("5\n");
    assert_all_contents(&term, file!(), line!(), &["1", "2", "3", "4", "5", ""]);
    assert_eq!(term.screen().visible_row_to_stable_row(0), 4);

    term.print("6\n");
    assert_all_contents(&term, file!(), line!(), &["2", "3", "4", "5", "6", ""]);
    assert_eq!(term.screen().visible_row_to_stable_row(0), 5);

    term.print("7\n");
    assert_all_contents(&term, file!(), line!(), &["3", "4", "5", "6", "7", ""]);
    assert_eq!(term.screen().visible_row_to_stable_row(0), 6);

    term.print("8\n");
    assert_all_contents(&term, file!(), line!(), &["4", "5", "6", "7", "8", ""]);
    assert_eq!(term.screen().visible_row_to_stable_row(0), 7);
}

#[test]
fn test_ri() {
    let mut term = TestTerm::new(3, 1, 10);
    term.print("1\n\u{8d}\n");
    assert_all_contents(&term, file!(), line!(), &["1", "", ""]);
}

#[test]
fn test_scroll_margins() {
    let mut term = TestTerm::new(3, 1, 10);
    term.print("1\n2\n3\n4\n");
    assert_all_contents(&term, file!(), line!(), &["1", "2", "3", "4", ""]);

    let margins = CSI::Cursor(wezterm_escape_parser::csi::Cursor::SetTopAndBottomMargins {
        top: OneBased::new(1),
        bottom: OneBased::new(2),
    });
    term.print(format!("{}", margins));

    term.print("z\n");
    assert_all_contents(&term, file!(), line!(), &["1", "2", "z", "4", ""]);

    term.print("a\n");
    assert_all_contents(&term, file!(), line!(), &["1", "2", "z", "a", "", ""]);

    term.cup(0, 1);
    term.print("W\n");
    assert_all_contents(&term, file!(), line!(), &["1", "2", "z", "a", "W", "", ""]);
}

#[test]
fn test_emoji_with_modifier() {
    let waving_hand = "\u{1f44b}";
    let waving_hand_dark_tone = "\u{1f44b}\u{1f3ff}";

    let mut term = TestTerm::new(3, 5, 0);
    term.print(waving_hand);
    term.print("\r\n");
    term.print(waving_hand_dark_tone);

    assert_all_contents(
        &term,
        file!(),
        line!(),
        &[waving_hand, waving_hand_dark_tone, ""],
    );
}

#[test]
fn test_1573() {
    let sequence = "\u{1112}\u{1161}\u{11ab}";

    let mut term = TestTerm::new(2, 5, 0);
    term.print(sequence);
    term.print("\r\n");

    assert_all_contents(&term, file!(), line!(), &[sequence, ""]);

    use unicode_normalization::UnicodeNormalization;
    let recomposed: String = sequence.nfc().collect();
    assert_eq!(recomposed, "\u{d55c}");

    use finl_unicode::grapheme_clusters::Graphemes;
    let graphemes: Vec<_> = Graphemes::new(sequence).collect();
    assert_eq!(graphemes, vec![sequence]);
}

#[test]
fn test_region_scroll() {
    let mut term = TestTerm::new(5, 1, 10);
    term.print("1\n2\n3\n4\n5");

    // Test scroll region that doesn't start on first row, scrollback not used
    term.set_scroll_region(1, 2);
    term.cup(0, 2);
    let seqno = term.current_seqno();
    term.print("\na");
    assert_all_contents(&term, file!(), line!(), &["1", "3", "a", "4", "5"]);
    term.assert_dirty_lines(seqno, &[1, 2], None);
    assert_eq!(term.screen().visible_row_to_stable_row(0), 0);
    assert_eq!(term.screen().visible_row_to_stable_row(4), 4);

    // Scroll region starting on first row, but is smaller than screen (see #6099)
    //  Scrollback will be used, which means lines below the scroll region
    //  have their stable index invalidated, and so need to be marked dirty
    term.set_scroll_region(0, 1);
    term.cup(0, 1);
    let seqno = term.current_seqno();
    term.print("\nb");
    assert_all_contents(&term, file!(), line!(), &["1", "3", "b", "a", "4", "5"]);
    term.assert_dirty_lines(seqno, &[2, 3, 4, 5], None);
    assert_eq!(term.screen().visible_row_to_stable_row(0), 1);
    assert_eq!(term.screen().visible_row_to_stable_row(4), 5);

    // Test deletion of more lines than exist in scroll region
    term.cup(0, 1);
    let seqno = term.current_seqno();
    term.delete_lines(3);
    assert_all_contents(&term, file!(), line!(), &["1", "3", "", "a", "4", "5"]);
    term.assert_dirty_lines(seqno, &[2], None);
    assert_eq!(term.screen().visible_row_to_stable_row(0), 1);
    assert_eq!(term.screen().visible_row_to_stable_row(4), 5);

    // Return to normal, entire-screen scrolling, optimal number of lines marked dirty
    term.set_scroll_region(0, 4);
    term.cup(0, 4);
    let seqno = term.current_seqno();
    term.print("\nX");
    assert_all_contents(&term, file!(), line!(), &["1", "3", "", "a", "4", "5", "X"]);
    term.assert_dirty_lines(seqno, &[6], None);
    assert_eq!(term.screen().visible_row_to_stable_row(4), 6);
}

#[test]
fn test_alt_screen_region_scroll() {
    // Test that scrollback is never used, and lines below the scroll region
    //  aren't made dirty or invalid. Only the scroll region is marked dirty.
    let mut term = TestTerm::new(5, 1, 10);
    term.print("M\no\nn\nk\ne\ny");

    // Enter alternate-screen mode, saving current state
    term.set_mode("?1049", true);
    term.print("1\n2\n3\n4\n5");

    // Test scroll region that doesn't start on first row
    term.set_scroll_region(1, 2);
    term.cup(0, 2);
    let seqno = term.current_seqno();
    term.print("\na");
    assert_all_contents(&term, file!(), line!(), &["1", "3", "a", "4", "5"]);
    term.assert_dirty_lines(seqno, &[1, 2], None);
    assert_eq!(term.screen().visible_row_to_stable_row(4), 4);

    // Test scroll region that starts on first row, still no scrollback
    term.set_scroll_region(0, 1);
    term.cup(0, 1);
    let seqno = term.current_seqno();
    term.print("\nb");
    assert_all_contents(&term, file!(), line!(), &["3", "b", "a", "4", "5"]);
    term.assert_dirty_lines(seqno, &[0, 1], None);
    assert_eq!(term.screen().visible_row_to_stable_row(4), 4);

    // Return to normal, entire-screen scrolling
    //  Not optimal, the entire screen is marked dirty for every line scrolled
    term.set_scroll_region(0, 4);
    term.cup(0, 4);
    let seqno = term.current_seqno();
    term.print("\nX");
    assert_all_contents(&term, file!(), line!(), &["b", "a", "4", "5", "X"]);
    term.assert_dirty_lines(seqno, &[0, 1, 2, 3, 4], None);
    assert_eq!(term.screen().visible_row_to_stable_row(4), 4);

    // Leave alternate-mode and ensure screen is restored, with every
    // visible line marked dirty: physical rows 1..6, since "M" has
    // scrolled into history. (Rows 0..5 used to be stamped, which dirtied
    // the history line and left the bottom row on view clean.)
    let seqno = term.current_seqno();
    term.set_mode("?1049", false);
    assert_all_contents(&term, file!(), line!(), &["M", "o", "n", "k", "e", "y"]);
    term.assert_dirty_lines(seqno, &[1, 2, 3, 4, 5], None);
    assert_eq!(term.screen().visible_row_to_stable_row(0), 1);
}

#[test]
fn region_scrolls_mark_the_region_wherever_the_row_ring_wraps() {
    // Each pass rotates the row deque one more step, so the scroll region
    // lands on every position relative to where its storage wraps around.
    for alt in [false, true] {
        let mut term = TestTerm::new(5, 4, 3);
        if alt {
            term.set_mode("?1049", true);
        }
        for pass in 0..24 {
            term.set_scroll_region(0, 4);
            term.cup(0, 4);
            term.print(&format!("\n{}", pass));
            let top = term.screen().scrollback_rows() - 5;
            let region = [top + 1, top + 2, top + 3];

            term.set_scroll_region(1, 3);
            term.cup(0, 3);
            let seqno = term.current_seqno();
            term.print("\n");
            term.assert_dirty_lines(seqno, &region, Some("scroll up"));

            term.cup(0, 1);
            let seqno = term.current_seqno();
            term.print("\x1bM");
            term.assert_dirty_lines(seqno, &region, Some("scroll down"));
        }
    }
}

#[test]
fn skipped_region_marks_match_marking_every_scroll() {
    use crate::screen::ALWAYS_MARK_REGIONS;
    // Region scrolls in one batch skip rows that an earlier scroll of the
    // same region marked. Interleave everything else that moves, replaces
    // or adds rows, in batches of every size, and compare with marking on
    // every scroll.
    let lf = |n: usize| "x\r\n".repeat(n);
    let steps = [
        format!("\x1b[2;4r\x1b[4;1H{}", lf(9)),
        format!("\x1b[4;1H{}\x1b[2;1H\x1bM\x1bM{}", lf(3), lf(3)),
        format!("\x1b[3;1H\x1b[2L{}\x1b[3;1H\x1b[M{}", lf(4), lf(4)),
        format!("\x1b[2S{}\x1b[T{}", lf(2), lf(2)),
        format!("\x1b[r\x1b[6;1H{}\x1b[2;4r\x1b[4;1H{}", lf(12), lf(5)),
        format!("\x1b[?1049h\x1b[1;3r\x1b[3;1H{}\x1b[?1049l{}", lf(6), lf(6)),
        format!("\x1b[3J\x1b[2;5r\x1b[5;1H{}\x1b#8{}", lf(7), lf(7)),
        format!("\x1b[1;6r\x1b[6;1H{}\x1b[2;4r\x1b[4;1H{}", lf(5), lf(5)),
        format!(
            "\x1b[2;4r\x1b[4;1H\x1bD\x1bD\x1bE{}\x1b[4;1H\x1b[2J{}",
            lf(2),
            lf(3)
        ),
    ];
    let input: String = steps.concat();
    for chunk in [1usize, 5, 17, 64, 100_000] {
        let mut skipping = TestTerm::new(6, 4, 3);
        let mut marking = TestTerm::new(6, 4, 3);
        for (i, piece) in input.as_bytes().chunks(chunk).enumerate() {
            if i % 7 == 3 {
                // A resize between batches moves every row.
                let rows = 5 + i % 3;
                for term in [&mut skipping, &mut marking] {
                    term.resize(TerminalSize {
                        rows,
                        cols: 4,
                        pixel_width: 32,
                        pixel_height: rows * 16,
                        dpi: 0,
                    });
                }
            }
            ALWAYS_MARK_REGIONS.with(|always| always.set(false));
            skipping.print(piece);
            ALWAYS_MARK_REGIONS.with(|always| always.set(true));
            marking.print(piece);
            ALWAYS_MARK_REGIONS.with(|always| always.set(false));
            assert_eq!(
                skipping.screen().all_lines(),
                marking.screen().all_lines(),
                "chunk={chunk}, batch={i}"
            );
            assert_eq!(
                skipping.cursor_pos(),
                marking.cursor_pos(),
                "chunk={chunk}, batch={i}"
            );
        }
    }
}

#[test]
fn recycled_full_screen_rows_match_new_rows() {
    use crate::screen::NEVER_RECYCLE_FULL_SCREEN;
    // Full-screen scrolls reuse the discarded row; compare with allocating
    // a new one, on the primary screen (rows go into a full history) and the
    // alternate one, with short, long, wide and styled rows.
    let mut input = String::new();
    for i in 0..40 {
        match i % 5 {
            0 => input.push_str("y\r\n"),
            1 => input.push_str(&format!("{}\r\n", "long row ".repeat(3))),
            2 => input.push_str("界🙂e\u{301}\r\n"),
            3 => input.push_str("\x1b[44mstyled\x1b[0m\r\n"),
            _ => input.push_str("\r\n"),
        }
    }
    let input = format!("{input}\x1b[?1049h{input}\x1b[?1049l{input}");
    for chunk in [1usize, 9, 64, 100_000] {
        let mut recycling = TestTerm::new(5, 12, 7);
        let mut allocating = TestTerm::new(5, 12, 7);
        for (i, piece) in input.as_bytes().chunks(chunk).enumerate() {
            NEVER_RECYCLE_FULL_SCREEN.with(|never| never.set(false));
            recycling.print(piece);
            NEVER_RECYCLE_FULL_SCREEN.with(|never| never.set(true));
            allocating.print(piece);
            NEVER_RECYCLE_FULL_SCREEN.with(|never| never.set(false));
            assert_eq!(
                recycling.screen().all_lines(),
                allocating.screen().all_lines(),
                "chunk={chunk}, batch={i}"
            );
        }
    }
}

#[test]
fn test_region_scrollback_limit() {
    // Ensure scrollback is truncated properly, when it reaches the line limit
    let mut term = TestTerm::new(4, 1, 2);
    term.print("1\n2\n3\n4");
    term.set_scroll_region(0, 1);
    term.cup(0, 1);

    let seqno = term.current_seqno();
    term.print("A\nB\nC\nD");
    assert_all_contents(&term, file!(), line!(), &["A", "B", "C", "D", "3", "4"]);
    term.assert_dirty_lines(seqno, &[0, 1, 2, 3, 4, 5], None);
    assert_eq!(term.screen().visible_row_to_stable_row(4), 7);
}

#[test]
fn styled_alt_screen_scrolling_matches_retained_primary_rows() {
    for region_end in [1, 3] {
        let mut primary = TestTerm::new(4, 8, 10);
        let mut alternate = TestTerm::new(4, 8, 10);
        alternate.set_mode("?1049", true);
        for term in [&mut primary, &mut alternate] {
            term.print("outside\r\nrows\r\nstay\r\nhere");
            term.set_scroll_region(0, region_end);
            term.cup(0, 0);
        }
        // Recycle styled rows repeatedly, including wide characters, wrapping,
        // hyperlinks and erasure. Primary history still uses compression and
        // provides an independent reference for the visible alternate screen.
        for i in 0..24 {
            let input = format!(
                "\x1b[48;2;12;34;56m\x1b]8;;https://example.com\x1b\\中e\u{301}🙂{i}\x1b]8;;\x1b\\\r\n\x1b[31mcolored text\x1b[0m\r\n\x1b[44m\x1b[2K"
            );
            primary.print(&input);
            alternate.print(&input);
            let expected = primary.screen().visible_lines();
            let actual = alternate.screen().visible_lines();
            assert_eq!(actual.len(), expected.len());
            assert_lines_equal(
                file!(),
                line!(),
                &actual,
                &expected,
                Compare::TEXT | Compare::ATTRS,
            );
            for (a, b) in actual.iter().zip(&expected) {
                assert_eq!(a.last_cell_was_wrapped(), b.last_cell_was_wrapped());
            }
            assert_eq!(alternate.screen().all_lines().len(), 4);
        }
        assert!(primary.screen().all_lines().len() > 4);
    }
}

/// A configuration whose hot-path settings change between batches and which
/// counts every read of them. With a `key` the terminal may cache them.
#[derive(Debug, Default)]
struct SwitchingConfig {
    key: Mutex<Option<(usize, usize)>>,
    nfc: std::sync::atomic::AtomicBool,
    bidi: std::sync::atomic::AtomicBool,
    scrollback: std::sync::atomic::AtomicUsize,
    reads: std::sync::atomic::AtomicUsize,
}

impl TerminalConfiguration for SwitchingConfig {
    fn change_key(&self) -> Option<(usize, usize)> {
        *self.key.lock().unwrap()
    }
    fn scrollback_size(&self) -> usize {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.scrollback.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn normalize_output_to_unicode_nfc(&self) -> bool {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.nfc.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn bidi_mode(&self) -> crate::config::BidiMode {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::config::BidiMode {
            enabled: self.bidi.load(std::sync::atomic::Ordering::SeqCst),
            hint: wezterm_bidi::ParagraphDirectionHint::LeftToRight,
        }
    }
    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }
}

#[test]
fn cached_hot_settings_follow_every_config_change() {
    use std::sync::atomic::Ordering::SeqCst;
    let make = |cache: bool| {
        let config = Arc::new(SwitchingConfig::default());
        if cache {
            *config.key.lock().unwrap() = Some((0, 0));
        }
        let term = Terminal::new(
            TerminalSize {
                rows: 4,
                cols: 12,
                pixel_width: 96,
                pixel_height: 64,
                dpi: 0,
            },
            Arc::clone(&config) as Arc<dyn TerminalConfiguration>,
            "ThinkTerm",
            "O_o",
            Box::new(Vec::new()),
        );
        (config, term)
    };
    let (cached_config, mut cached) = make(true);
    let (plain_config, mut plain) = make(false);
    // Each step changes what the hot paths read: NFC for printed runs, bidi
    // for line feeds and erases, the scrollback size for history and REP.
    let steps = [
        (false, false, 3),
        (true, false, 3),
        (true, true, 1),
        (false, true, 5),
        (false, false, 0),
    ];
    let chunk = "e\u{301}x\r\nab\u{5d0}\u{5d1}\r\n\x1b[2Kz\x1b[20b\r\n".repeat(6);
    for (step, &(nfc, bidi, scrollback)) in steps.iter().enumerate() {
        for config in [&cached_config, &plain_config] {
            config.nfc.store(nfc, SeqCst);
            config.bidi.store(bidi, SeqCst);
            config.scrollback.store(scrollback, SeqCst);
        }
        *cached_config.key.lock().unwrap() = Some((step + 1, 0));
        cached.advance_bytes(&chunk);
        plain.advance_bytes(&chunk);
        assert_eq!(
            cached.screen().all_lines(),
            plain.screen().all_lines(),
            "step {step}"
        );
        assert_eq!(cached.cursor_pos(), plain.cursor_pos(), "step {step}");
    }
    let cached_reads = cached_config.reads.load(SeqCst);
    let plain_reads = plain_config.reads.load(SeqCst);
    assert!(
        cached_reads * 4 < plain_reads,
        "settings are reused while the key stays: {cached_reads} vs {plain_reads} reads"
    );
}

#[test]
fn perform_actions_in_place_matches_owned_batches() {
    use wezterm_escape_parser::parser::Parser;
    let input = format!(
        "plain ascii\r\n\x1b[1;31mred\x1b[0m wide 中文 e\u{301}\r\n{}\x1b[2;4r\x1b[4H\n\n\n\x1b[r\
         \x1b]0;title\x07\x1b[38:5:24;48:2:125:136:147mx\x1b[100b\x1b[m\x1b[10A\x1b[3E\x1b[2Kend",
        "a line long enough to wrap in a narrow terminal ".repeat(3)
    );
    for chunk_size in [1, 3, 7, 64, 4096] {
        let mut owned = TestTerm::new(5, 20, 10);
        let mut in_place = TestTerm::new(5, 20, 10);
        let mut owned_parser = Parser::new();
        let mut in_place_parser = Parser::new();
        // One vector for every batch, as the mux parser thread keeps it.
        let mut reused = Vec::new();
        for chunk in input.as_bytes().chunks(chunk_size) {
            owned.perform_actions(owned_parser.parse_as_vec(chunk));
            in_place_parser.parse(chunk, |action| reused.push(action));
            in_place.perform_actions_in_place(&mut reused);
            assert!(reused.is_empty(), "every action is applied and removed");
            // Line equality covers cells, attributes, bits and seqnos.
            assert_eq!(
                in_place.screen().all_lines(),
                owned.screen().all_lines(),
                "chunk_size={chunk_size}"
            );
            assert_eq!(in_place.cursor_pos(), owned.cursor_pos());
            assert_eq!(in_place.current_seqno(), owned.current_seqno());
            assert_eq!(in_place.get_title(), owned.get_title());
            assert_eq!(
                in_place.is_alt_screen_active(),
                owned.is_alt_screen_active()
            );
        }
        assert!(
            reused.capacity() > 0,
            "the allocation stays for the next batch"
        );
    }
}

#[test]
fn test_hyperlinks() {
    let mut term = TestTerm::new(3, 5, 0);
    let link = Arc::new(Hyperlink::new("http://example.com"));
    term.hyperlink(&link);
    term.print("hello");
    term.hyperlink_off();

    let mut linked = CellAttributes::default();
    linked.set_hyperlink(Some(Arc::clone(&link)));

    assert_lines_equal(
        file!(),
        line!(),
        &term.screen().visible_lines(),
        &[
            Line::from_text("hello", &linked, SEQ_ZERO, None),
            "".into(),
            "".into(),
        ],
        Compare::TEXT | Compare::ATTRS,
    );

    term.hyperlink(&link);
    term.print("he");
    // Resetting pen should not reset the link
    term.print("\x1b[m");
    term.print("y!!");

    assert_lines_equal(
        file!(),
        line!(),
        &term.screen().visible_lines(),
        &[
            Line::from_text_with_wrapped_last_col("hello", &linked, SEQ_ZERO),
            Line::from_text("hey!!", &linked, SEQ_ZERO, None),
            "".into(),
        ],
        Compare::TEXT | Compare::ATTRS,
    );

    let otherlink = Arc::new(Hyperlink::new_with_id("http://example.com/other", "w00t"));

    // Switching link and turning it off
    term.hyperlink(&otherlink);
    term.print("wo");
    // soft reset also disables hyperlink attribute
    term.soft_reset();
    term.print("00t");

    let mut partial_line = Line::from_text("wo00t", &CellAttributes::default(), SEQ_ZERO, None);
    partial_line.set_cell(
        0,
        Cell::new(
            'w',
            CellAttributes::default()
                .set_hyperlink(Some(Arc::clone(&otherlink)))
                .clone(),
        ),
        SEQ_ZERO,
    );
    partial_line.set_cell(
        1,
        Cell::new(
            'o',
            CellAttributes::default()
                .set_hyperlink(Some(Arc::clone(&otherlink)))
                .clone(),
        ),
        SEQ_ZERO,
    );

    assert_lines_equal(
        file!(),
        line!(),
        &term.screen().visible_lines(),
        &[
            Line::from_text_with_wrapped_last_col("hello", &linked, SEQ_ZERO),
            Line::from_text_with_wrapped_last_col("hey!!", &linked, SEQ_ZERO),
            partial_line,
        ],
        Compare::TEXT | Compare::ATTRS,
    );
}

/// What the embedder writes through the terminal's writer handle goes out
/// behind what the terminal itself wrote, never ahead: one pty, one order.
#[cfg(not(target_family = "wasm"))]
#[test]
fn the_writer_handle_keeps_the_terminals_order() {
    use std::io::Write;

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let sink = Sink::default();
    let mut term = Terminal::new(
        TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 640,
            pixel_height: 384,
            dpi: 0,
        },
        Arc::new(TestTermConfig { scrollback: 0 }),
        "ThinkTerm",
        "O_o",
        Box::new(sink.clone()),
    );
    let mut handle = term.writer_handle();
    let mut expected = Vec::new();
    for _ in 0..50 {
        // A paste the terminal applies, then bytes the embedder encoded.
        term.send_paste("p").unwrap();
        handle.write_all(b"h").unwrap();
        expected.extend_from_slice(b"ph");
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while sink.0.lock().unwrap().len() < expected.len() && std::time::Instant::now() < deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(sink.0.lock().unwrap().clone(), expected);
}

#[test]
fn oversized_agent_updates_clear_retained_identity_but_normal_updates_survive() {
    let mut term = TestTerm::new(3, 20, 10);
    let normal = "v1;agent=soul;state=working;session=test";
    for value in [
        normal.to_string(),
        format!("v1;agent={}", "x".repeat(129)),
        normal.into(),
        format!("v1;session={}", "x".repeat(513)),
    ] {
        let encoded = graphics::base64_of(value.as_bytes());
        term.print(format!(
            "\x1b]1337;SetUserVar=THINKTERM_AGENT={encoded}\x07"
        ));
        let expected = if crate::agent_contract::agent_contract_within_budget(&value) {
            value.as_str()
        } else {
            ""
        };
        assert_eq!(
            term.user_vars().get("THINKTERM_AGENT").map(String::as_str),
            Some(expected)
        );
    }
}
