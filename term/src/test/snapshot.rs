//! A terminal's snapshot restores into a fresh terminal as the same
//! terminal: same screens, same scrollback, same pictures, same modes, and
//! the same behaviour for whatever the program sends next.

use crate::color::ColorPalette;
use crate::terminalstate::{TerminalSnapshot, SNAPSHOT_VERSION};
use crate::{Terminal, TerminalConfiguration, TerminalSize};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug)]
struct SnapshotConfig {
    scrollback: usize,
}

impl TerminalConfiguration for SnapshotConfig {
    fn scrollback_size(&self) -> usize {
        self.scrollback
    }

    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }

    fn enable_kitty_graphics(&self) -> bool {
        true
    }

    fn enable_kitty_keyboard(&self) -> bool {
        true
    }
}

/// 80x24 cells of 8x16 pixels, so pictures have a geometry to land in.
fn terminal(scrollback: usize) -> Terminal {
    Terminal::new(
        TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 640,
            pixel_height: 384,
            dpi: 96,
        },
        Arc::new(SnapshotConfig { scrollback }),
        "ThinkTerm",
        "O_o",
        Box::new(Vec::new()),
    )
}

/// A 2x2 all-zero RGBA picture.
const PIXELS_2X2: &str = "AAAAAAAAAAAAAAAAAAAAAA==";
/// Half of one, for a chunked transfer.
const HALF_2X2: &str = "AAAAAAAAAAA=";
/// A 2x2 red sixel.
const SIXEL_2X2: &str = "\x1bPq\"1;1;2;2#0;2;100;0;0#0~~$-\x1b\\";

/// Touches every field the snapshot carries, on both screens.
fn everything(term: &mut Terminal) {
    // Text with attributes and a hyperlink.
    term.advance_bytes("\x1b[31mred \x1b[1mbold\x1b[0m plain ");
    term.advance_bytes("\x1b]8;;http://example.com/\x1b\\link\x1b]8;;\x1b\\\r\n");
    // A line that wraps.
    term.advance_bytes("x".repeat(100));
    term.advance_bytes("\r\n");
    // More lines than rows: scrollback.
    for i in 0..40 {
        term.advance_bytes(format!("line {i}\r\n"));
    }
    // Pictures: a kitty image placed twice (once by number), and a sixel.
    term.advance_bytes(format!(
        "\x1b[3;1H\x1b_Ga=t,i=1,f=32,s=2,v=2;{PIXELS_2X2}\x1b\\\x1b_Ga=p,i=1,p=1\x1b\\"
    ));
    term.advance_bytes("\x1b[3;10H\x1b_Ga=p,i=1,p=2,z=3\x1b\\");
    term.advance_bytes(format!(
        "\x1b[5;1H\x1b_Ga=T,I=7,f=32,s=2,v=2;{PIXELS_2X2}\x1b\\"
    ));
    term.advance_bytes(format!("\x1b[7;1H{SIXEL_2X2}"));
    // A saved cursor, custom tab stops, margins.
    term.advance_bytes("\x1b[5;10H\x1b7\x1b[2;3H");
    term.advance_bytes("\x1b[3g\x1b[1;7H\x1bH\x1b[1;13H\x1bH\x1b[9;2H");
    term.advance_bytes("\x1b[?69h\x1b[5;70s\x1b[3;20r");
    // Every mode.
    term.advance_bytes("\x1b[4h\x1b[?6h\x1b[?5h\x1b[?2004h\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h");
    term.advance_bytes("\x1b[?1004h\x1b[?1h\x1b[?45h\x1b[20h\x1b=\x1b[?80h\x1b[?8452h");
    term.advance_bytes("\x1b[?1070h\x1b[>4;2m\x1b)0\x0e\x1b[>1u\x1b[?9001h");
    term.advance_bytes("\x1b]1337;UnicodeVersion=push before\x07\x1b]1337;UnicodeVersion=14\x07");
    // What the program says about itself.
    term.advance_bytes("\x1b]0;My Title\x07\x1b]1;icon\x07\x1b]7;file://localhost/tmp/work\x07");
    term.advance_bytes("\x1b]1337;SetUserVar=foo=YmFy\x07\x1b]4;1;rgb:ff/00/00\x07\x1b]9;4;1;50\x07");
    term.focus_changed(false);
    // The alternate screen, with its own saved cursor, content and kitty
    // keyboard stack (the primary keeps the one pushed above).
    term.advance_bytes("\x1b[?1049h\x1b[3;4Halt text\x1b7\x1b[10;1H\x1b[>3u");
}

/// Whether the cell at `x`,`y` of the visible screen carries a picture; a
/// cell never written to has nothing to ask.
fn has_image(term: &mut Terminal, x: usize, y: i64) -> bool {
    term.screen_mut()
        .get_cell(x, y)
        .is_some_and(|cell| cell.attrs().images().is_some())
}

fn restored(snapshot: TerminalSnapshot, scrollback: usize) -> Terminal {
    let mut term = terminal(scrollback);
    term.restore(snapshot).expect("restore");
    term
}

#[test]
fn a_restored_terminal_is_the_same_terminal() {
    let mut original = terminal(100);
    everything(&mut original);
    let snapshot = original.snapshot();
    assert_eq!(snapshot.version, SNAPSHOT_VERSION);
    assert!(snapshot.alt_screen_is_active);
    assert!(!snapshot.kitty.transmission_in_progress);

    let mut copy = terminal(100);
    copy.restore_with_kitty_placements(original.snapshot(), original.snapshot_kitty_graphics().placements).unwrap();
    assert_eq!(copy.snapshot(), snapshot, "a snapshot of the restore");
    for alt in [false, true] {
        assert_eq!(
            copy.screen_for_alt(alt).all_lines(),
            original.screen_for_alt(alt).all_lines(),
            "lines of the {} screen",
            if alt { "alternate" } else { "primary" }
        );
    }
    assert_eq!(copy.cursor_pos(), original.cursor_pos());
    assert_eq!(copy.get_title(), original.get_title());
    assert_eq!(copy.get_current_dir(), original.get_current_dir());
    assert_eq!(copy.user_vars(), original.user_vars());
    assert_eq!(copy.get_keyboard_encoding(), original.get_keyboard_encoding());
    assert_eq!(copy.get_size(), original.get_size());

    // The same bytes from here on leave both terminals the same: the
    // modes took effect, the kitty ids resolve, the margins hold.
    for term in [&mut original, &mut copy] {
        term.advance_bytes("\x1b[?1049l");
        term.advance_bytes("\x1b_Ga=d,d=i,i=1\x1b\\");
        term.advance_bytes("\x1b[4;5Hin the margins\r\n\r\n\r\n");
        term.advance_bytes("\x1b8restored\t|");
        term.advance_bytes("\x1b]1337;UnicodeVersion=pop before\x07");
    }
    assert_eq!(copy.snapshot(), original.snapshot(), "after more output");
    assert_eq!(
        copy.screen().all_lines(),
        original.screen().all_lines(),
        "primary lines after more output"
    );
}

/// The script is only worth its round trip if it moved every field off
/// its default; a wrong escape sequence would otherwise leave both sides
/// at the default and the comparison none the wiser.
#[test]
fn the_script_reaches_every_field() {
    use crate::terminalstate::{CharSet, MouseEncoding, SnapshotKeyboardEncoding};
    use crate::Progress;

    let mut original = terminal(100);
    everything(&mut original);
    let snapshot = original.snapshot();

    let modes = &snapshot.modes;
    assert!(snapshot.cursor.insert);
    assert!(modes.dec_origin_mode);
    assert!(modes.reverse_video_mode);
    assert!(modes.bracketed_paste);
    assert!(modes.any_event_mouse);
    assert!(modes.button_event_mouse);
    assert!(modes.mouse_tracking);
    assert_eq!(modes.mouse_encoding, MouseEncoding::SGR);
    assert!(modes.focus_tracking);
    assert!(modes.application_cursor_keys);
    assert!(modes.reverse_wraparound_mode);
    assert!(modes.newline_mode);
    assert!(modes.application_keypad);
    assert!(modes.sixel_display_mode);
    assert!(modes.sixel_scrolls_right);
    assert!(modes.use_private_color_registers_for_each_graphic);
    assert_eq!(modes.modify_other_keys, Some(2));
    assert_eq!(modes.g1_charset, CharSet::DecLineDrawing);
    assert!(modes.shift_out);
    assert_eq!(
        modes.keyboard_encoding,
        SnapshotKeyboardEncoding::Win32,
        "win32-input-mode lives in the terminal, the kitty pushes on the screens"
    );
    assert_eq!(
        snapshot.alt_screen.keyboard_stack,
        [SnapshotKeyboardEncoding::Kitty(3)]
    );
    assert_eq!(modes.unicode_version.version, 14);
    assert_eq!(modes.unicode_version_stack.len(), 1);
    assert_eq!(modes.top_and_bottom_margins, 2..20);
    assert_eq!(modes.left_and_right_margins, 4..70);
    assert!(modes.left_and_right_margin_mode);
    assert!(!modes.focused);
    assert_ne!(
        modes.tabs,
        terminal(100).snapshot().modes.tabs,
        "the tab stops were changed"
    );

    let identity = &snapshot.identity;
    assert_eq!(identity.title, "My Title");
    assert_eq!(identity.icon_title.as_deref(), Some("icon"));
    assert_eq!(
        identity.current_dir.as_deref(),
        Some("file:///tmp/work")
    );
    assert_eq!(identity.user_vars.get("foo").map(String::as_str), Some("bar"));
    assert!(identity.palette.is_some(), "OSC 4 made a palette override");
    assert_eq!(identity.progress, Progress::Percentage(50));
    assert!(identity.agent_osc_title.is_some());
    assert!(identity.agent_osc_progress.is_some());

    assert!(snapshot.screen.saved_cursor.is_some());
    assert!(snapshot.alt_screen.saved_cursor.is_some());
    assert!(snapshot.screen.lines.len() > 24, "scrollback");
    assert_eq!(snapshot.images.len(), 2, "the kitty pixels and the sixel");
    assert!(!snapshot.screen.image_cells.is_empty());
    assert_eq!(snapshot.kitty.number_to_id.get(&7).copied(), Some(2));
    assert_eq!(snapshot.kitty.placements.len(), 3);
    assert_eq!(
        snapshot.screen.keyboard_stack,
        [SnapshotKeyboardEncoding::Kitty(1)],
        "the primary screen keeps its own stack"
    );
}

#[test]
fn a_snapshot_survives_its_bytes() {
    let mut original = terminal(100);
    everything(&mut original);
    let snapshot = original.snapshot();

    let mut bytes = Vec::new();
    let mut encode = varbincode::Serializer::new(&mut bytes);
    snapshot.serialize(&mut encode).expect("serialize");
    let mut reader = &bytes[..];
    let mut decode = varbincode::Deserializer::new(&mut reader);
    let back = TerminalSnapshot::deserialize(&mut decode).expect("deserialize");
    assert_eq!(back, snapshot);

    let copy = restored(back, 100);
    assert_eq!(copy.snapshot(), snapshot);
}

#[test]
fn a_smaller_scrollback_keeps_the_newest_lines() {
    let mut original = terminal(100);
    everything(&mut original);
    original.advance_bytes("\x1b[?1049l");
    let snapshot = original.snapshot();
    let carried = snapshot.screen.lines.len();
    let offset = snapshot.screen.stable_row_index_offset;
    assert!(carried > 24 + 4, "the script scrolled {carried} lines");

    let mut copy = terminal(4);
    copy.restore_with_kitty_placements(snapshot, original.snapshot_kitty_graphics().placements).unwrap();
    let screen = copy.screen();
    assert_eq!(screen.lines().len(), 24 + 4, "visible rows plus the allowance");
    assert_eq!(
        screen.stable_row_index_offset(),
        offset + (carried - 28),
        "the dropped lines still count in the stable offset"
    );
    assert_eq!(copy.cursor_pos(), original.cursor_pos());
    assert_eq!(
        screen.visible_lines(),
        original.screen().visible_lines(),
        "the visible rows are untouched"
    );
    assert_eq!(
        screen.visible_row_to_stable_row(0),
        original.screen().visible_row_to_stable_row(0),
        "a stable row index names the same line on both sides"
    );
}

#[test]
fn a_picture_travels_once_however_many_cells_and_ids_share_it() {
    let mut original = terminal(24);
    original.advance_bytes(format!(
        "\x1b_Ga=t,i=1,f=32,s=2,v=2;{PIXELS_2X2}\x1b\\\x1b_Ga=p,i=1,p=1\x1b\\"
    ));
    original.advance_bytes("\x1b[3;1H\x1b_Ga=p,i=1,p=2\x1b\\");
    original.advance_bytes(format!(
        "\x1b[5;1H\x1b_Ga=t,i=2,f=32,s=2,v=2;{PIXELS_2X2}\x1b\\\x1b_Ga=p,i=2\x1b\\"
    ));
    let snapshot = original.snapshot();
    assert_eq!(snapshot.images.len(), 1, "same pixels, one picture");
    assert_eq!(snapshot.kitty.id_to_hash.len(), 2, "under two ids");
    assert!(
        snapshot.screen.image_cells.len() >= 3,
        "placed three times: {} cells",
        snapshot.screen.image_cells.len()
    );

    let mut copy = restored(snapshot, 24);
    let stored = copy.kitty_image_data_for_id(1).expect("id 1 restored");
    let same_as_id_2 = copy.kitty_image_data_for_id(2).expect("id 2 restored");
    assert!(Arc::ptr_eq(&stored, &same_as_id_2), "both ids hold the one Arc");
    let cell = copy
        .screen_mut()
        .get_cell(0, 0)
        .expect("cell 0,0")
        .clone();
    let on_screen = cell.attrs().images().expect("a picture at 0,0");
    assert!(
        Arc::ptr_eq(on_screen[0].image_data(), &stored),
        "the cell shares the Arc with the kitty store"
    );
    assert_eq!(on_screen[0].image_id(), Some(1));
    assert_eq!(on_screen[0].placement_id(), Some(1));

    // A delete by id after the restore finds the placements it names.
    copy.advance_bytes("\x1b_Ga=d,d=I,i=1\x1b\\");
    assert!(!has_image(&mut copy, 0, 0), "placement 1 is gone");
    assert!(!has_image(&mut copy, 0, 2), "placement 2 is gone");
    assert!(has_image(&mut copy, 0, 4), "the picture under id 2 stays");
}

#[test]
fn a_transfer_in_flight_is_flagged_and_not_resumed() {
    let mut original = terminal(24);
    original.advance_bytes(format!(
        "\x1b_Ga=t,i=9,f=32,s=2,v=2,m=1;{HALF_2X2}\x1b\\"
    ));
    let snapshot = original.snapshot();
    assert!(snapshot.kitty.transmission_in_progress);
    assert!(snapshot.images.is_empty());

    let mut copy = restored(snapshot, 24);
    copy.advance_bytes(format!("\x1b_Gm=0;{HALF_2X2}\x1b\\"));
    copy.advance_bytes("\x1b_Ga=p,i=9\x1b\\");
    assert!(
        !has_image(&mut copy, 0, 0),
        "the half-transferred picture never appears"
    );
    assert!(copy.kitty_image_data_for_id(9).is_none());
}

#[test]
fn a_snapshot_from_another_version_is_refused() {
    let mut original = terminal(24);
    original.advance_bytes("hello");
    let mut snapshot = original.snapshot();
    snapshot.version += 1;
    let err = terminal(24).restore(snapshot).expect_err("refused");
    let message = format!("{err:#}");
    assert!(
        message.contains(&format!("version {}", SNAPSHOT_VERSION + 1))
            && message.contains(&format!("version {}", SNAPSHOT_VERSION)),
        "{message}"
    );
}

#[test]
fn a_snapshot_needs_a_terminal_of_its_size() {
    let mut original = terminal(24);
    original.advance_bytes("hello");
    let snapshot = original.snapshot();
    let mut small = Terminal::new(
        TerminalSize {
            rows: 10,
            cols: 40,
            pixel_width: 320,
            pixel_height: 160,
            dpi: 96,
        },
        Arc::new(SnapshotConfig { scrollback: 24 }),
        "ThinkTerm",
        "O_o",
        Box::new(Vec::new()),
    );
    let err = small.restore(snapshot).expect_err("refused");
    assert!(format!("{err:#}").contains("80x24"), "{err:#}");
}

#[test]
fn restore_applies_agent_budgets_without_changing_other_user_variables() {
    for contract in [
        format!("v1;agent={}", "x".repeat(129)),
        format!("v1;session={}", "s".repeat(513)),
        "x".repeat(1024 * 1024),
    ] {
        let mut snapshot = terminal(10).snapshot();
        snapshot
            .identity
            .user_vars
            .insert("THINKTERM_AGENT".into(), contract);
        snapshot
            .identity
            .user_vars
            .insert("ordinary".into(), "kept".into());
        let restored = restored(snapshot, 10);
        let cleared = restored.user_vars().get("THINKTERM_AGENT").unwrap();
        assert!(cleared.is_empty());
        assert_eq!(cleared.capacity(), 0);
        assert_eq!(restored.user_vars().get("ordinary").unwrap(), "kept");
    }
    let mut snapshot = terminal(10).snapshot();
    let normal = "v1;agent=claude;state=idle;session=abc";
    snapshot
        .identity
        .user_vars
        .insert("THINKTERM_AGENT".into(), normal.into());
    assert_eq!(
        restored(snapshot, 10)
            .user_vars()
            .get("THINKTERM_AGENT")
            .unwrap(),
        normal
    );
}
