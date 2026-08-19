use super::*;

/// In this issue, the `CSI 2 P` sequence incorrectly removed two
/// cells from the line, leaving them effectively blank, when those
/// two cells should have been erased to the current background
/// color as set by `CSI 40 m`
#[test]
fn test_789() {
    let mut term = TestTerm::new(1, 8, 0);
    term.print("\x1b[40m\x1b[Kfoo\x1b[2P");

    k9::snapshot!(
        term.screen().visible_lines(),
        r#"
[
    Line {
        cells: V(
            VecStorage {
                cells: [
                    Cell {
                        text: "f",
                        width: 1,
                        attrs: CellAttributes {
                            attributes: 0,
                            intensity: Normal,
                            underline: None,
                            blink: None,
                            italic: false,
                            reverse: false,
                            strikethrough: false,
                            invisible: false,
                            wrapped: false,
                            overline: false,
                            semantic_type: Output,
                            foreground: Default,
                            background: PaletteIndex(
                                0,
                            ),
                            fat: None,
                        },
                    },
                    Cell {
                        text: "o",
                        width: 1,
                        attrs: CellAttributes {
                            attributes: 0,
                            intensity: Normal,
                            underline: None,
                            blink: None,
                            italic: false,
                            reverse: false,
                            strikethrough: false,
                            invisible: false,
                            wrapped: false,
                            overline: false,
                            semantic_type: Output,
                            foreground: Default,
                            background: PaletteIndex(
                                0,
                            ),
                            fat: None,
                        },
                    },
                    Cell {
                        text: "o",
                        width: 1,
                        attrs: CellAttributes {
                            attributes: 0,
                            intensity: Normal,
                            underline: None,
                            blink: None,
                            italic: false,
                            reverse: false,
                            strikethrough: false,
                            invisible: false,
                            wrapped: false,
                            overline: false,
                            semantic_type: Output,
                            foreground: Default,
                            background: PaletteIndex(
                                0,
                            ),
                            fat: None,
                        },
                    },
                    Cell {
                        text: " ",
                        width: 1,
                        attrs: CellAttributes {
                            attributes: 0,
                            intensity: Normal,
                            underline: None,
                            blink: None,
                            italic: false,
                            reverse: false,
                            strikethrough: false,
                            invisible: false,
                            wrapped: false,
                            overline: false,
                            semantic_type: Output,
                            foreground: Default,
                            background: PaletteIndex(
                                0,
                            ),
                            fat: None,
                        },
                    },
                    Cell {
                        text: " ",
                        width: 1,
                        attrs: CellAttributes {
                            attributes: 0,
                            intensity: Normal,
                            underline: None,
                            blink: None,
                            italic: false,
                            reverse: false,
                            strikethrough: false,
                            invisible: false,
                            wrapped: false,
                            overline: false,
                            semantic_type: Output,
                            foreground: Default,
                            background: PaletteIndex(
                                0,
                            ),
                            fat: None,
                        },
                    },
                    Cell {
                        text: " ",
                        width: 1,
                        attrs: CellAttributes {
                            attributes: 0,
                            intensity: Normal,
                            underline: None,
                            blink: None,
                            italic: false,
                            reverse: false,
                            strikethrough: false,
                            invisible: false,
                            wrapped: false,
                            overline: false,
                            semantic_type: Output,
                            foreground: Default,
                            background: PaletteIndex(
                                0,
                            ),
                            fat: None,
                        },
                    },
                    Cell {
                        text: " ",
                        width: 1,
                        attrs: CellAttributes {
                            attributes: 0,
                            intensity: Normal,
                            underline: None,
                            blink: None,
                            italic: false,
                            reverse: false,
                            strikethrough: false,
                            invisible: false,
                            wrapped: false,
                            overline: false,
                            semantic_type: Output,
                            foreground: Default,
                            background: PaletteIndex(
                                0,
                            ),
                            fat: None,
                        },
                    },
                    Cell {
                        text: " ",
                        width: 1,
                        attrs: CellAttributes {
                            attributes: 0,
                            intensity: Normal,
                            underline: None,
                            blink: None,
                            italic: false,
                            reverse: false,
                            strikethrough: false,
                            invisible: false,
                            wrapped: false,
                            overline: false,
                            semantic_type: Output,
                            foreground: Default,
                            background: PaletteIndex(
                                0,
                            ),
                            fat: None,
                        },
                    },
                ],
            },
        ),
        zones: [],
        seqno: 5,
        bits: LineBits(
            0x0,
        ),
        appdata: Mutex {
            data: None,
            poisoned: false,
            ..
        },
    },
]
"#
    );
}

#[test]
fn test_vpa() {
    let mut term = TestTerm::new(3, 4, 0);
    term.assert_cursor_pos(0, 0, None, Some(0));
    term.print("a\r\nb\r\nc");
    term.assert_cursor_pos(1, 2, None, None);
    term.print("\x1b[d");
    term.assert_cursor_pos(1, 0, None, None);
    term.print("\r\n\r\n");
    term.assert_cursor_pos(0, 2, None, None);

    // escapes are 1-based, so check that we're handling that
    // when we parse them!
    term.print("\x1b[2d");
    term.assert_cursor_pos(0, 1, None, None);
    term.print("\x1b[-2d");
    term.assert_cursor_pos(0, 1, None, Some(term.current_seqno() - 1));
}

#[test]
fn test_rep() {
    let mut term = TestTerm::new(3, 4, 0);
    term.print("h");
    term.cup(1, 0);
    term.print("\x1b[2ba");
    assert_visible_contents(&term, file!(), line!(), &["hhha", "", ""]);
}

#[test]
fn test_irm() {
    let mut term = TestTerm::new(3, 8, 0);
    term.print("foo");
    term.cup(0, 0);
    term.print("\x1b[4hBAR");
    assert_visible_contents(&term, file!(), line!(), &["BARfoo", "", ""]);
}

#[test]
fn test_ich() {
    let mut term = TestTerm::new(3, 4, 0);
    term.print("hey!wat?");
    term.cup(1, 0);
    term.print("\x1b[2@");
    assert_visible_contents(&term, file!(), line!(), &["h  e", "wat?", ""]);
    // check how we handle overflowing the width
    term.print("\x1b[12@");
    assert_visible_contents(&term, file!(), line!(), &["h   ", "wat?", ""]);
    term.print("\x1b[-12@");
    assert_visible_contents(&term, file!(), line!(), &["h   ", "wat?", ""]);
}

#[test]
fn test_ech() {
    let mut term = TestTerm::new(3, 4, 0);
    term.print("hey!wat?");
    term.cup(1, 0);
    term.print("\x1b[2X");
    assert_visible_contents(&term, file!(), line!(), &["h  !", "wat?", ""]);
    // check how we handle overflowing the width
    term.print("\x1b[12X");
    assert_visible_contents(&term, file!(), line!(), &["h   ", "wat?", ""]);
    term.print("\x1b[-12X");
    assert_visible_contents(&term, file!(), line!(), &["h   ", "wat?", ""]);
}

#[test]
fn test_dch() {
    let mut term = TestTerm::new(1, 12, 0);
    term.print("hello world");
    term.cup(1, 0);
    term.print("\x1b[P");
    assert_visible_contents(&term, file!(), line!(), &["hllo world"]);

    term.cup(4, 0);
    term.print("\x1b[2P");
    assert_visible_contents(&term, file!(), line!(), &["hlloorld"]);

    term.print("\x1b[-2P");
    assert_visible_contents(&term, file!(), line!(), &["hlloorld"]);
}

#[test]
fn test_cup() {
    let mut term = TestTerm::new(3, 4, 0);
    term.cup(1, 1);
    term.assert_cursor_pos(1, 1, None, None);
    term.cup(-1, -1);
    term.assert_cursor_pos(0, 0, None, None);
    term.cup(2, 2);
    term.assert_cursor_pos(2, 2, None, None);
    term.cup(-1, -1);
    term.assert_cursor_pos(0, 0, None, None);
    term.cup(500, 500);
    term.assert_cursor_pos(4, 2, None, None);
}

#[test]
fn test_hvp() {
    let mut term = TestTerm::new(3, 4, 0);
    term.hvp(1, 1);
    term.assert_cursor_pos(1, 1, None, None);
    term.hvp(-1, -1);
    term.assert_cursor_pos(0, 0, None, None);
    term.hvp(2, 2);
    term.assert_cursor_pos(2, 2, None, None);
    term.hvp(-1, -1);
    term.assert_cursor_pos(0, 0, None, None);
    term.hvp(500, 500);
    term.assert_cursor_pos(4, 2, None, None);
}

#[test]
fn test_dl() {
    let mut term = TestTerm::new(3, 1, 0);
    term.print("a\r\nb\r\nc");
    term.cup(0, 1);
    let seqno = term.current_seqno();
    term.delete_lines(1);
    assert_visible_contents(&term, file!(), line!(), &["a", "c", ""]);
    term.assert_cursor_pos(0, 1, None, Some(seqno));
    term.cup(0, 0);
    term.delete_lines(2);
    assert_visible_contents(&term, file!(), line!(), &["", "", ""]);
    term.print("1\r\n2\r\n3");
    term.cup(0, 1);
    term.delete_lines(-2);
    assert_visible_contents(&term, file!(), line!(), &["1", "2", "3"]);
}

#[test]
fn test_cha() {
    let mut term = TestTerm::new(3, 4, 0);
    term.cup(1, 1);
    term.assert_cursor_pos(1, 1, None, None);

    term.print("\x1b[G");
    term.assert_cursor_pos(0, 1, None, None);

    term.print("\x1b[2G");
    term.assert_cursor_pos(1, 1, None, None);

    term.print("\x1b[0G");
    term.assert_cursor_pos(0, 1, None, None);

    let seqno = term.current_seqno();
    term.print("\x1b[-1G");
    term.assert_cursor_pos(0, 1, None, Some(seqno));

    term.print("\x1b[100G");
    term.assert_cursor_pos(4, 1, None, None);
}

#[test]
fn test_ed() {
    let mut term = TestTerm::new(3, 3, 0);
    term.print("abc\r\ndef\r\nghi");
    term.cup(1, 2);
    term.print("\x1b[J");
    assert_visible_contents(&term, file!(), line!(), &["abc", "def", "g"]);

    // Set background color to blue
    term.print("\x1b[44m");
    // Clear whole screen
    term.print("\x1b[2J");

    // Check that the background color paints all of the cells;
    // this is also known as BCE - Background Color Erase.
    let attr = CellAttributes::default()
        .set_background(color::AnsiColor::Navy)
        .clone();
    let mut line: Line = "   ".into();
    line.fill_range(0..3, &Cell::new(' ', attr.clone()), SEQ_ZERO);
    assert_lines_equal(
        file!(),
        line!(),
        &term.screen().visible_lines(),
        &[line.clone(), line.clone(), line],
        Compare::TEXT | Compare::ATTRS,
    );
}

#[test]
fn test_ed_erase_scrollback() {
    let mut term = TestTerm::new(3, 3, 3);
    term.print("abc\r\ndef\r\nghi\r\n111\r\n222\r\na\x1b[3J");
    assert_all_contents(&term, file!(), line!(), &["111", "222", "a"]);
    term.print("b");
    assert_all_contents(&term, file!(), line!(), &["111", "222", "ab"]);
}

/// The largest value a CSI parameter can carry: anything above it is rejected
/// by the parser, so this is what a hostile program actually gets to send.
const HUGE_PARAM: u32 = u32::MAX;

/// Asserts that two terminals fed different parameters ended up in the same
/// state. The left-hand terminal is always given a parameter small enough to
/// run the handler's loop for real, so it stands in for the unclamped
/// behaviour; the right-hand one is given HUGE_PARAM and goes through the
/// clamp.
fn assert_same_state(saturating: &TestTerm, huge: &TestTerm, file: &str, line: u32) {
    assert_lines_equal(
        file,
        line,
        &huge.screen().all_lines(),
        &saturating.screen().all_lines(),
        Compare::TEXT | Compare::ATTRS,
    );
    let (huge_pos, saturating_pos) = (huge.cursor_pos(), saturating.cursor_pos());
    assert!(
        huge_pos.x == saturating_pos.x && huge_pos.y == saturating_pos.y,
        "{}:{}: cursor differs: huge={:?} saturating={:?}",
        file,
        line,
        (huge_pos.x, huge_pos.y),
        (saturating_pos.x, saturating_pos.y)
    );
}

/// ICH past the right margin has nothing left to push, so a huge count must
/// land in the same place as one that just reaches the margin.
#[test]
fn test_ich_huge_param() {
    let run = |n: u32| {
        let mut term = TestTerm::new(3, 4, 0);
        term.print("abcd");
        term.cup(1, 0);
        term.print(format!("\x1b[{n}@"));
        term
    };
    // 4 columns, cursor at column 1: three inserts blank the rest of the row.
    assert_same_state(&run(3), &run(HUGE_PARAM), file!(), line!());
}

/// DCH stops at the right margin. On a 32-bit target the unclamped `x + n`
/// overflowed before min() saw it, turning this into a no-op.
#[test]
fn test_dch_huge_param() {
    let run = |n: u32| {
        let mut term = TestTerm::new(3, 4, 0);
        term.print("abcd");
        term.cup(1, 0);
        term.print(format!("\x1b[{n}P"));
        term
    };
    assert_same_state(&run(3), &run(HUGE_PARAM), file!(), line!());
}

/// ECH stops at the last column; same 32-bit overflow as DCH.
#[test]
fn test_ech_huge_param() {
    let run = |n: u32| {
        let mut term = TestTerm::new(3, 4, 0);
        term.print("abcd");
        term.cup(1, 0);
        term.print(format!("\x1b[{n}X"));
        term
    };
    assert_same_state(&run(3), &run(HUGE_PARAM), file!(), line!());
}

/// CUF stops at the last column; same 32-bit overflow as DCH.
#[test]
fn test_cuf_huge_param() {
    let run = |n: u32| {
        let mut term = TestTerm::new(3, 4, 0);
        term.cup(1, 0);
        term.print(format!("\x1b[{n}C"));
        term
    };
    assert_same_state(&run(2), &run(HUGE_PARAM), file!(), line!());
}

/// Tabs stick at the right margin, so more of them than there are columns
/// cannot move the cursor any further.
#[test]
fn test_cht_huge_param() {
    let run = |n: u32| {
        let mut term = TestTerm::new(3, 32, 0);
        term.print(format!("\x1b[{n}I"));
        term
    };
    assert_same_state(&run(32), &run(HUGE_PARAM), file!(), line!());
}

/// Backwards tabs stick at column zero.
#[test]
fn test_cbt_huge_param() {
    let run = |n: u32| {
        let mut term = TestTerm::new(3, 32, 0);
        term.cup(31, 0);
        term.print(format!("\x1b[{n}Z"));
        term
    };
    assert_same_state(&run(32), &run(HUGE_PARAM), file!(), line!());
}

/// REP keeps changing the screen as it wraps and scrolls, so its clamp is the
/// one that has to preserve more than "nothing further happens": past a full
/// screen and scrollback the picture is fixed and only the cursor column moves,
/// cycling with the margin width.
///
/// The reference here is 15 on a 4x3 screen with no scrollback. That is above
/// the saturation point (4 * 3 = 12) yet congruent to u32::MAX modulo 4, so the
/// clamp leaves it alone and the loop really does run fifteen times -- which is
/// what the unclamped code would settle on for any larger count.
#[test]
fn test_rep_huge_param() {
    let run = |n: u32| {
        let mut term = TestTerm::new(3, 4, 0);
        term.print("x");
        term.cup(0, 0);
        term.print(format!("\x1b[{n}b"));
        term
    };
    assert!(
        HUGE_PARAM % 4 == 15 % 4,
        "reference count must stay congruent to HUGE_PARAM"
    );
    assert_same_state(&run(15), &run(HUGE_PARAM), file!(), line!());
}

/// The reason the clamps exist. Unclamped, these four sequences are roughly
/// three minutes of work between them in a release build and far more here.
/// The bound is loose enough not to be flaky on a busy machine and still two
/// orders of magnitude below the unclamped cost.
#[test]
fn test_huge_params_are_bounded_work() {
    let start = std::time::Instant::now();
    for seq in [
        format!("\x1b[{HUGE_PARAM}@"),
        format!("\x1b[{HUGE_PARAM}b"),
        format!("\x1b[{HUGE_PARAM}I"),
        format!("\x1b[{HUGE_PARAM}Z"),
    ] {
        let mut term = TestTerm::new(24, 80, 3500);
        term.print("filler text");
        term.print(seq);
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "huge CSI parameters took {:?}; a clamp is missing",
        elapsed
    );
}
