//! Regression tests for the image attachment path shared by the Kitty
//! graphics, sixel and iTerm protocols.
//!
//! Everything here funnels through `assign_image_to_cells`, which used to
//! divide by zero whenever a pane had no usable pixel geometry or a placement
//! selected an empty source rectangle. Both are reachable from a hostile or
//! merely unlucky byte stream, and a panic there kills the pane's parser
//! thread and freezes the pane.

use super::*;
use crate::color::ColorPalette;
use std::assert_eq;
use std::io::Write;
use std::sync::{Arc, Mutex};

#[derive(Debug)]
struct GraphicsConfig {
    kitty: bool,
    image_budget: usize,
}

impl TerminalConfiguration for GraphicsConfig {
    fn scrollback_size(&self) -> usize {
        24
    }

    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }

    fn enable_kitty_graphics(&self) -> bool {
        self.kitty
    }

    fn kitty_image_memory_budget(&self) -> usize {
        self.image_budget
    }
}

fn term(pixel_width: usize, pixel_height: usize, kitty: bool) -> Terminal {
    term_with_budget(
        pixel_width,
        pixel_height,
        kitty,
        crate::config::DEFAULT_KITTY_IMAGE_MEMORY_BUDGET,
    )
}

fn term_with_budget(
    pixel_width: usize,
    pixel_height: usize,
    kitty: bool,
    image_budget: usize,
) -> Terminal {
    Terminal::new(
        TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width,
            pixel_height,
            dpi: 96,
        },
        Arc::new(GraphicsConfig {
            kitty,
            image_budget,
        }),
        "ThinkTerm",
        "O_o",
        Box::new(Vec::new()),
    )
}

/// A 2x2 all-zero RGBA image, transmitted and stored as image id 1.
const XMIT_2X2: &str = "\x1b_Ga=t,i=1,f=32,s=2,v=2;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\";

/// A 2x2 sixel. The sixel path always passes `columns: None, rows: None`, so it
/// takes the dividing branch that a Kitty placement can avoid with `c=`/`r=`.
const SIXEL_2X2: &str = "\x1bPq\"1;1;2;2#0;2;100;0;0#0~~$-\x1b\\";

/// Captures what the terminal writes back to the application.
#[derive(Clone, Default)]
struct Tap(Arc<Mutex<Vec<u8>>>);

impl Write for Tap {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Tap {
    /// `TerminalState` hands replies to a `BufWriter<ThreadedWriter>`, so they
    /// arrive on a worker thread rather than during `advance_bytes`. Wait for
    /// the requested occurrence of `needle` instead of guessing at a delay.
    /// Replies are written in order, so everything before that barrier has
    /// arrived too. An earlier drain's barrier cannot satisfy a later one.
    fn read_until(&self, needle: &str, count: usize) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let buf = String::from_utf8_lossy(&self.0.lock().unwrap()).to_string();
            if buf.matches(needle).count() >= count {
                return buf;
            }
            assert!(std::time::Instant::now() < deadline, "terminal reply barrier timed out");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

/// The image id of the sentinel query `drain` sends. Queries never allocate
/// ids, so it cannot collide with anything a test transmits.
const SENTINEL: &str = "987654321";

/// Returns everything the terminal has replied so far, deterministically:
/// sends a query that always earns a reply, waits for that reply, and strips
/// it. Replies arrive in write order, so a reply that "should not exist" is
/// proven absent rather than merely slow, and no test waits longer than the
/// actual write latency.
///
/// The sentinel is a Query, which aborts a chunked transfer in progress —
/// only drain once the transfer under test has completed or been abandoned.
fn drain(term: &mut Terminal, tap: &Tap) -> String {
    let reply = format!("\x1b_Gi={SENTINEL};OK\x1b\\");
    let count = String::from_utf8_lossy(&tap.0.lock().unwrap()).matches(&reply).count() + 1;
    term.advance_bytes(format!(
        "\x1b_Ga=q,i={SENTINEL},f=32,s=1,v=1;AAAAAA==\x1b\\"
    ));
    tap.read_until(&reply, count).replace(&reply, "")
}

fn term_with_tap(pixel_width: usize, pixel_height: usize) -> (Terminal, Tap) {
    let tap = Tap::default();
    let term = Terminal::new(
        TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width,
            pixel_height,
            dpi: 96,
        },
        Arc::new(GraphicsConfig {
            kitty: true,
            image_budget: crate::config::DEFAULT_KITTY_IMAGE_MEMORY_BUDGET,
        }),
        "ThinkTerm",
        "O_o",
        Box::new(tap.clone()),
    );
    (term, tap)
}

fn kitty(pixel_width: usize, pixel_height: usize, place: &str) {
    let mut term = term(pixel_width, pixel_height, true);
    term.advance_bytes(XMIT_2X2);
    term.advance_bytes(place);
}

#[test]
fn kitty_placement_survives_degenerate_pixel_geometry() {
    // tmux panes, thinkterm-tui panes and `mux.spawn_window{width, height}` all
    // reach the terminal with no pixel size at all.
    kitty(0, 384, "\x1b_Ga=p,i=1\x1b\\");
    kitty(640, 0, "\x1b_Ga=p,i=1\x1b\\");
    kitty(0, 0, "\x1b_Ga=p,i=1\x1b\\");
    // Fewer pixels than columns leaves cells zero pixels wide.
    kitty(40, 384, "\x1b_Ga=p,i=1\x1b\\");
    // Specifying c=/r= takes the other branch, which used to reach a NaN
    // instead of a division by zero.
    kitty(0, 0, "\x1b_Ga=p,i=1,c=1,r=1\x1b\\");
}

#[test]
fn kitty_placement_survives_empty_source_rect() {
    // Zero extent, with and without cell scaling.
    kitty(640, 384, "\x1b_Ga=p,i=1,w=0,c=1\x1b\\");
    kitty(640, 384, "\x1b_Ga=p,i=1,h=0,r=1\x1b\\");
    kitty(640, 384, "\x1b_Ga=p,i=1,w=0\x1b\\");
    kitty(640, 384, "\x1b_Ga=p,i=1,h=0\x1b\\");
    // Origin at or past the far edge of a 2x2 image.
    kitty(640, 384, "\x1b_Ga=p,i=1,x=2,c=1\x1b\\");
    kitty(640, 384, "\x1b_Ga=p,i=1,y=2,r=1\x1b\\");
    kitty(640, 384, "\x1b_Ga=p,i=1,x=99,y=99\x1b\\");
}

#[test]
fn sixel_survives_degenerate_pixel_geometry() {
    // The sixel path is not gated by enable_kitty_graphics, so turning Kitty
    // graphics off is not a mitigation for this class of crash.
    for (w, h) in [(0, 384), (640, 0), (0, 0), (40, 384)] {
        let mut term = term(w, h, false);
        term.advance_bytes(SIXEL_2X2);
    }
}

#[test]
fn kitty_placement_still_works_on_a_normal_pane() {
    let mut term = term(640, 384, true);
    term.advance_bytes(XMIT_2X2);
    term.advance_bytes("\x1b_Ga=p,i=1\x1b\\");

    let line = term.screen_mut().line_mut(0);
    let cell = line.get_cell(0).expect("cell 0,0 should exist");
    let images = cell
        .attrs()
        .images()
        .expect("a placement should attach an image to the cell it covers");
    assert!(!images.is_empty());
}

#[test]
fn kitty_image_only_rows_survive_resizing_a_blank_bottom_margin() {
    for alternate in [false, true] {
        for compressed in [false, true] {
            for columns in [90, 100] {
                let mut terminal = term(1000, 550, true);
                let size = TerminalSize {
                    rows: 25,
                    cols: 100,
                    pixel_width: 1000,
                    pixel_height: 550,
                    dpi: 96,
                };
                terminal.resize(size);
                if alternate {
                    terminal.advance_bytes("\x1b[?1049h");
                }
                terminal.advance_bytes(XMIT_2X2);
                terminal.advance_bytes("\x1b[3;5H\x1b_Ga=p,i=1,p=1,c=8,r=4,C=1,q=2\x1b\\");
                if compressed {
                    for row in 2..6 {
                        terminal.screen_mut().line_mut(row).compress_for_scrollback();
                    }
                }
                let image_cells = |terminal: &Terminal| {
                    terminal
                        .screen()
                        .all_lines()
                        .iter()
                        .flat_map(|line| line.visible_cells())
                        .filter(|cell| cell.attrs().has_images())
                        .count()
                };
                assert_eq!(image_cells(&terminal), 32);
                terminal.resize(TerminalSize {
                    rows: 22,
                    cols: columns,
                    pixel_width: columns * 10,
                    pixel_height: 484,
                    ..size
                });
                assert_eq!(image_cells(&terminal), 32, "all four image rows survive");
                terminal.resize(size);
                assert_eq!(image_cells(&terminal), 32, "growing preserves the image");
            }
        }
    }
}

#[test]
fn sixel_still_works_on_a_normal_pane() {
    let mut term = term(640, 384, false);
    term.advance_bytes(SIXEL_2X2);

    let line = term.screen_mut().line_mut(0);
    let cell = line.get_cell(0).expect("cell 0,0 should exist");
    assert!(
        cell.attrs().images().is_some(),
        "a sixel should attach an image to the cell it covers"
    );
}

/// Half of a 2x2 RGBA image, base64 encoded: eight zero bytes.
const HALF_2X2: &str = "AAAAAAAAAAA=";

#[test]
fn an_unpadded_kitty_image_is_decoded_and_placed() {
    use wezterm_cell::image::ImageDataType;

    let (mut term, tap) = term_with_tap(640, 384);
    let pixels = [
        255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
    ];
    let encoded = base64_of(&pixels);
    term.advance_bytes(format!(
        "\x1b_Ga=T,i=9,f=32,s=2,v=2,c=1,r=1;{}\x1b\\",
        encoded.trim_end_matches('=')
    ));
    assert_eq!(drain(&mut term, &tap), "\x1b_Gi=9;OK\x1b\\");
    let image = term.kitty_image_data_for_id(9).expect("decoded image");
    match &*image.data() {
        ImageDataType::Rgba8 {
            data,
            width,
            height,
            ..
        } => {
            assert_eq!((*width, *height), (2, 2));
            assert_eq!(data.as_slice(), pixels.as_slice());
        }
        other => panic!("unexpected image data {:?}", other),
    }
    assert!(term
        .screen_mut()
        .line_mut(0)
        .get_cell(0)
        .unwrap()
        .attrs()
        .images()
        .unwrap()
        .iter()
        .any(|im| im.image_id() == Some(9)));
    assert_eq!(term.kitty_used_memory(), pixels.len());
    drop(image);
    term.advance_bytes("\x1b_Ga=d,d=I,i=9\x1b\\");
    assert!(term.kitty_image_data_for_id(9).is_none());
    assert_eq!(term.kitty_used_memory(), 0);
}

#[test]
fn benchmark_sized_kitty_images_decode_with_optional_padding() {
    use wezterm_cell::image::ImageDataType;

    let (mut term, tap) = term_with_tap(640, 384);
    let pixels: Vec<u8> = (0..4 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let padded = base64_of(&pixels);
    // kitten __benchmark__ sends a 1024x1024 RGBA image in 128 KiB
    // Base64 chunks, with no '=' in the final chunk. Check the old padded
    // form too, including deleting the stored image after each transfer.
    for encoded in [padded.as_str(), padded.trim_end_matches('=')] {
        let mut chunks = encoded.as_bytes().chunks(128 * 1024).peekable();
        let mut first = true;
        while let Some(chunk) = chunks.next() {
            let more = u8::from(chunks.peek().is_some());
            let header = if first {
                first = false;
                format!("\x1b_Ga=t,i=12345,f=32,s=1024,v=1024,m={more};")
            } else {
                format!("\x1b_Gm={more};")
            };
            term.advance_bytes(header);
            // Also split PTY reads inside the APC payload.
            for bytes in chunk.chunks(8191) {
                term.advance_bytes(bytes);
            }
            term.advance_bytes("\x1b\\");
        }
        assert_eq!(drain(&mut term, &tap), "\x1b_Gi=12345;OK\x1b\\");
        // Tap retains replies, including the sentinel. Start the next
        // transfer with an empty buffer so drain waits for its own reply.
        tap.0.lock().unwrap().clear();
        let image = term.kitty_image_data_for_id(12345).expect("complete image");
        match &*image.data() {
            ImageDataType::Rgba8 {
                data,
                width,
                height,
                ..
            } => {
                assert_eq!((*width, *height), (1024, 1024));
                assert_eq!(data, &pixels);
            }
            other => panic!("unexpected image data {:?}", other),
        }
        assert_eq!(term.kitty_used_memory(), pixels.len());
        drop(image);
        term.advance_bytes("\x1b_Ga=d,d=I,i=12345\x1b\\");
        assert!(term.kitty_image_data_for_id(12345).is_none());
        assert_eq!(term.kitty_used_memory(), 0);
    }
}

#[test]
fn a_malformed_kitty_tail_does_not_poison_the_next_image() {
    let (mut term, tap) = term_with_tap(640, 384);
    term.advance_bytes("\x1b_Ga=t,i=9,f=32,s=2,v=2,m=1;AAAAAAAAAAAAAAAA\x1b\\");
    // Four more bytes need either no padding or two '=' characters.
    term.advance_bytes("\x1b_Gm=0;AAAAAA=\x1b\\");
    assert!(term.kitty_image_data_for_id(9).is_none());
    assert_eq!(term.kitty_used_memory(), 0);
    term.advance_bytes("\x1b_Ga=t,i=10,f=32,s=2,v=2;AAAAAAAAAAAAAAAAAAAAAA\x1b\\");
    let reply = drain(&mut term, &tap);
    assert!(reply.contains("i=10;OK"), "{:?}", reply);
    assert!(!reply.contains("i=9;OK"), "{:?}", reply);
    assert!(term.kitty_image_data_for_id(10).is_some());
    assert_eq!(term.kitty_used_memory(), 16);
}

#[test]
fn a_chunked_transfer_reassembles() {
    let (mut term, _tap) = term_with_tap(640, 384);
    term.advance_bytes(format!("\x1b_Ga=t,i=9,f=32,s=2,v=2,m=1;{HALF_2X2}\x1b\\"));
    term.advance_bytes(format!("\x1b_Gm=0;{HALF_2X2}\x1b\\"));
    term.advance_bytes("\x1b_Ga=p,i=9\x1b\\");

    let line = term.screen_mut().line_mut(0);
    let cell = line.get_cell(0).expect("cell 0,0 should exist");
    assert!(
        cell.attrs().images().is_some(),
        "the two chunks should have reassembled into a placeable image"
    );
}

#[test]
fn animation_controls_leave_legacy_images_and_transfers_unchanged() {
    let (mut term, tap) = term_with_tap(640, 384);
    term.advance_bytes(XMIT_2X2);
    term.advance_bytes("\x1b_Ga=f,i=1,f=32,s=2,v=2,z=70;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");
    term.advance_bytes("\x1b_Ga=p,i=1\x1b\\");
    let before = animation_at_origin(&mut term).unwrap();
    let image = term.kitty_image_data_for_id(1).unwrap();
    let generation = image.generation();
    let cursor = term.cursor_pos();
    term.advance_bytes(format!("\x1b_Ga=t,i=9,f=32,s=2,v=2,m=1;{HALF_2X2}\x1b\\"));
    for control in ["c=2", "r=2,z=-1", "s=1", "s=2,v=1", "s=3,v=3"] {
        term.advance_bytes(format!("\x1b_Ga=a,i=1,{control}\x1b\\"));
    }
    term.advance_bytes(format!("\x1b_Gm=0;{HALF_2X2}\x1b\\"));
    assert_eq!(animation_at_origin(&mut term).unwrap(), before);
    assert_eq!(image.generation(), generation);
    assert_eq!(term.cursor_pos(), cursor);
    assert!(term.kitty_image_data_for_id(9).is_some());
    let reply = drain(&mut term, &tap);
    assert!(!reply.contains("EINVAL"), "{reply}");
    assert!(reply.contains("i=9;OK"), "{reply}");
}

#[test]
fn frame_selections_are_validated_and_released_with_the_image() {
    let mut term = term(640, 384, true);
    term.advance_bytes(XMIT_2X2);
    term.advance_bytes("\x1b_Ga=f,i=1,f=32,s=2,v=2;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");
    term.advance_bytes("\x1b_Ga=a,i=1,c=2\x1b\\");
    let (revision, selected) = term.kitty_frame_selections(None).unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].image_id, 1);
    assert_eq!(selected[0].animation.frame, 1);
    for command in ["i=1,c=2", "i=1,c=99", "i=9,c=2", "i=1,c=0"] {
        term.advance_bytes(format!("\x1b_Ga=a,{command}\x1b\\"));
        assert!(term.kitty_frame_selections(Some(revision)).is_none());
    }
    term.advance_bytes("\x1b_Ga=a,i=1,c=1\x1b\\");
    assert_eq!(term.kitty_frame_selections(Some(revision)).unwrap().1[0].animation.frame, 0);
    term.advance_bytes("\x1b_Ga=a,i=1,c=2\x1b\\");
    let before_delete = term.kitty_frame_selections(None).unwrap().0;
    term.advance_bytes("\x1b_Ga=d,d=I,i=1\x1b\\");
    assert!(term.kitty_frame_selections(Some(before_delete)).unwrap().1.is_empty());
    term.advance_bytes(XMIT_2X2);
    let root = term.kitty_frame_selections(None).unwrap().1;
    assert_eq!(root.len(), 1);
    assert_eq!(root[0].animation.frame, 0);
    assert_eq!(root[0].animation.frame_ends, [0]);
    assert!(term.snapshot_kitty_playback().selections.is_empty());
}

#[test]
fn frame_selection_resolves_image_numbers_and_reset_releases_it() {
    let mut term = term(640, 384, true);
    term.advance_bytes("\x1b_Ga=t,I=12,f=32,s=2,v=2;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");
    term.advance_bytes("\x1b_Ga=f,I=12,f=32,s=2,v=2;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");
    term.advance_bytes("\x1b_Ga=a,I=12,c=2\x1b\\");
    assert_eq!(term.kitty_frame_selections(None).unwrap().1[0].animation.frame, 1);
    term.advance_bytes("\x1bc");
    assert!(term.kitty_frame_selections(None).unwrap().1.is_empty());
}

#[test]
fn kitty_number_resolves_previous_image_after_delete_and_failed_transmissions() {
    let (mut terminal, tap) = term_with_tap(640, 384);
    for _ in 0..3 {
        terminal.advance_bytes("\x1b_Ga=t,I=12,f=32,s=2,v=2;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");
    }
    assert_eq!(terminal.snapshot_kitty_graphics().image_numbers, [(12, 1), (12, 2), (12, 3)]);
    // Replacing and editing an older id must not reorder numbered images.
    terminal.advance_bytes(XMIT_2X2);
    terminal.advance_bytes("\x1b_Ga=f,i=2,f=32,s=2,v=2;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");
    for failed in [
        "a=t,I=12,f=32,s=2,v=2;AA==",
        "a=t,I=12,f=100;AAAAAA==",
        "a=t,I=12,f=32,s=2,v=2,o=z;AAAAAA==",
        "a=t,I=12,f=32,s=2,v=2;!",
        "a=f,I=99,f=32,s=2,v=2;AAAAAAAAAAAAAAAAAAAAAA==",
    ] {
        terminal.advance_bytes(format!("\x1b_G{failed}\x1b\\"));
        assert_eq!(terminal.snapshot_kitty_graphics().image_numbers, [(12, 1), (12, 2), (12, 3)]);
        assert_eq!(terminal.kitty_image_stats().0, 3);
    }
    for id in [3, 2, 1] {
        terminal.advance_bytes("\x1b[1;1H\x1b_Ga=p,I=12,p=7,c=2,r=2,C=1\x1b\\");
        assert_eq!(kitty_placement_keys(&terminal, false), [(id, Some(7))].into());
        if id == 2 {
            terminal.advance_bytes("\x1b_Ga=a,I=12,c=2\x1b\\");
            assert_eq!(terminal.kitty_frame_selections(None).unwrap().1.into_iter()
                .find(|entry| entry.image_id == 2).unwrap().animation.frame, 1);
            terminal.advance_bytes("\x1b_Ga=d,d=f,I=12,r=2\x1b\\");
            assert_eq!(terminal.kitty_image_data_for_id(2).unwrap().len(), 16);
        }
        terminal.advance_bytes("\x1b_Ga=d,d=N,I=12\x1b\\");
        assert!(terminal.kitty_image_data_for_id(id).is_none());
    }
    assert!(terminal.snapshot_kitty_graphics().image_numbers.is_empty());
    terminal.advance_bytes("\x1b_Ga=t,I=99,f=32,s=2,v=2;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");
    assert_eq!(terminal.snapshot_kitty_graphics().image_numbers, [(99, 4)]);
    let reply = drain(&mut terminal, &tap);
    assert!(reply.contains("I=99;ENOENT"));
    assert!(reply.contains("I=99,i=4;OK"));
    terminal.advance_bytes("\x1bc");
    assert!(terminal.snapshot_kitty_graphics().image_numbers.is_empty());
}

#[test]
fn kitty_number_history_is_released_with_evicted_images() {
    let mut terminal = term_with_budget(640, 384, true, 32);
    for value in 1..20u8 {
        let pixels = base64_of(&[value; 16]);
        terminal.advance_bytes(format!("\x1b_Ga=t,I=12,f=32,s=2,v=2;{pixels}\x1b\\"));
        let history = terminal.snapshot_kitty_graphics().image_numbers;
        assert!(history.len() <= 2);
        assert_eq!(history.last(), Some(&(12, u32::from(value))));
        assert!(history.iter().all(|(_, id)| terminal.kitty_image_data_for_id(*id).is_some()));
    }
    terminal.advance_bytes("\x1b_Ga=d,d=N,I=12\x1b\\");
    assert_eq!(terminal.snapshot_kitty_graphics().image_numbers, [(12, 18)]);
    terminal.advance_bytes("\x1b_Ga=d,d=R,x=1,y=99\x1b\\");
    assert!(terminal.snapshot_kitty_graphics().image_numbers.is_empty());
    assert_eq!(terminal.kitty_used_memory(), 0);
}

#[test]
#[cfg(feature = "use_serde")]
fn kitty_number_history_survives_restore_and_validates_before_replacing() {
    let mut original = term(640, 384, true);
    for _ in 0..3 {
        original.advance_bytes("\x1b_Ga=t,I=12,f=32,s=2,v=2;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");
    }
    let history = original.snapshot_kitty_graphics().image_numbers;
    let mut restored = term(640, 384, true);
    restored.restore(original.snapshot()).unwrap();
    assert_eq!(restored.snapshot_kitty_graphics().image_numbers, [(12, 3)]);
    for invalid in [vec![], vec![(12, 1)], vec![(12, 3), (12, 3)],
        vec![(13, 1), (12, 3)], vec![(12, 3), (12, 4)]] {
        assert!(restored.restore_kitty_numbers(invalid).is_err());
        assert_eq!(restored.snapshot_kitty_graphics().image_numbers, [(12, 3)]);
    }
    restored.restore_kitty_numbers(history.clone()).unwrap();
    assert_eq!(restored.snapshot_kitty_graphics().image_numbers, history);
    assert_eq!(restored.snapshot(), original.snapshot());
    for id in [3, 2, 1] {
        restored.advance_bytes("\x1b_Ga=d,d=N,I=12\x1b\\");
        assert!(restored.kitty_image_data_for_id(id).is_none());
        assert_eq!(restored.kitty_image_stats().0, id as usize - 1);
    }
}

#[test]
fn animation_pixel_edits_publish_versions_without_changing_playback() {
    let mut term = term(640, 384, true);
    term.advance_bytes(XMIT_2X2);
    term.advance_bytes(FRAME_2X2);
    for command in [
        "\x1b_Ga=f,i=1,f=32,s=2,v=2,r=1;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\",
        "\x1b_Ga=c,i=1,r=2,c=1,w=1,h=1\x1b\\",
    ] {
        let (revision, before) = term.kitty_frame_selections(None).unwrap();
        term.advance_bytes(command);
        let (_, after) = term.kitty_frame_selections(Some(revision)).expect("pixel edit publishes metadata");
        assert_eq!(after[0].data_hash, before[0].data_hash);
        assert!(after[0].data_generation > before[0].data_generation);
        assert_eq!(after[0].animation, before[0].animation);
    }
}

#[test]
fn static_pixel_edits_publish_versions_and_release_them_with_the_image() {
    let mut term = term(640, 384, true);
    let initial = term.kitty_frame_selections(None).unwrap().0;
    term.advance_bytes(XMIT_2X2);
    let (revision, before) = term.kitty_frame_selections(Some(initial)).unwrap();
    term.advance_bytes("\x1b_Ga=f,i=1,f=32,s=2,v=2,r=1;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");
    let (edited, after) = term.kitty_frame_selections(Some(revision)).unwrap();
    assert!(after[0].data_generation > before[0].data_generation);
    assert_eq!(after[0].animation, before[0].animation);
    assert!(term.snapshot_kitty_playback().selections.is_empty());
    term.advance_bytes("\x1b_Ga=d,d=I,i=1\x1b\\");
    assert!(term.kitty_frame_selections(Some(edited)).unwrap().1.is_empty());
}

#[test]
fn web_animation_gaps_and_controls_do_not_rewrite_legacy_frame_durations() {
    use crate::kitty_animation::Playback;
    use wezterm_cell::image::ImageDataType;
    let mut term = term(640, 384, true);
    term.advance_bytes(XMIT_2X2);
    term.advance_bytes("\x1b_Ga=f,i=1,f=32,s=2,v=2,z=70;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");
    term.advance_bytes("\x1b_Ga=f,i=1,f=32,s=2,v=2,z=-1;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");
    term.advance_bytes("\x1b_Ga=a,i=1,r=1,z=30,s=2,v=3\x1b\\");
    let state = term.kitty_frame_selections(None).unwrap().1.remove(0).animation;
    assert_eq!(state.frame_ends, [30, 100, 100]);
    assert_eq!(state.mode, Playback::Loading);
    assert_eq!(state.max_loops, 2);
    term.advance_bytes("\x1b_Ga=f,i=1,f=32,s=2,v=2,r=2;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");
    assert_eq!(term.kitty_frame_selections(None).unwrap().1[0].animation.frame_ends, state.frame_ends);
    let image = term.kitty_image_data_for_id(1).unwrap();
    let data = image.data();
    let ImageDataType::AnimRgba8 { durations, .. } = &*data else { panic!("not animated"); };
    assert_eq!(durations.iter().map(|gap| gap.as_millis()).collect::<Vec<_>>(), [0, 70, 40]);
}

#[test]
#[cfg(feature = "use_serde")]
fn playback_companion_restores_selection_and_rejects_mismatched_images_atomically() {
    let mut original = term(640, 384, true);
    original.advance_bytes(XMIT_2X2);
    original.advance_bytes("\x1b_Ga=f,i=1,f=32,s=2,v=2,z=70;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");
    original.advance_bytes("\x1b_Ga=a,i=1,c=2,r=1,z=30\x1b\\");
    let playback = original.snapshot_kitty_playback();
    let mut copy = term(640, 384, true);
    copy.restore(original.snapshot()).unwrap();
    copy.restore_kitty_playback(playback.clone()).unwrap();
    assert_eq!(copy.snapshot(), original.snapshot());
    let (revision, selections) = copy.kitty_frame_selections(None).unwrap();
    assert_eq!(selections[0].animation.frame, 1);
    assert_eq!(selections[0].animation.frame_ends, [30, 100]);
    for kind in 0..4 {
        let mut invalid = playback.clone();
        match kind {
            0 => invalid.selections[0].image_id = 999,
            1 => invalid.selections[0].data_hash[0] ^= 1,
            2 => invalid.selections[0].animation.frame_ends.pop().map(|_| ()).unwrap(),
            _ => invalid.selections.push(invalid.selections[0].clone()),
        }
        assert!(copy.restore_kitty_playback(invalid).is_err());
        assert!(copy.kitty_frame_selections(Some(revision)).is_none());
    }
}

#[test]
fn idle_sweep_keeps_a_chunked_transfer_that_is_still_arriving() {
    let (mut term, tap) = term_with_tap(640, 384);
    term.advance_bytes(format!("\x1b_Ga=t,i=9,f=32,s=2,v=2,m=1;{HALF_2X2}\x1b\\"));
    assert_eq!(term.idle_image_tick(2), 0);
    assert_eq!(term.idle_image_tick(2), 0);
    // Even an empty continuation is activity; only the chunk count changes.
    for _ in 0..3 {
        term.advance_bytes("\x1b_Gm=1;\x1b\\");
        assert_eq!(term.idle_image_tick(2), 0);
        assert_eq!(term.idle_image_tick(2), 0);
    }
    term.advance_bytes(format!("\x1b_Gm=0;{HALF_2X2}\x1b\\"));
    // The newest completed image must survive idle sweeps until placement.
    for _ in 0..4 {
        assert_eq!(term.idle_image_tick(2), 0);
    }
    term.advance_bytes("\x1b_Ga=p,i=9\x1b\\");
    assert!(term
        .screen_mut()
        .line_mut(0)
        .get_cell(0)
        .unwrap()
        .attrs()
        .images()
        .is_some());
    let reply = drain(&mut term, &tap);
    assert!(reply.contains("i=9;OK"), "{:?}", reply);
    assert!(!reply.contains(";E"), "{:?}", reply);
}

#[test]
fn idle_sweep_discards_late_chunks_and_accepts_the_next_image() {
    for tail_ends in [false, true] {
        let (mut term, tap) = term_with_tap(640, 384);
        term.advance_bytes(format!("\x1b_Ga=t,i=9,f=32,s=2,v=2,m=1;{HALF_2X2}\x1b\\"));
        assert_eq!(term.idle_image_tick(2), 0);
        assert_eq!(term.idle_image_tick(2), 0);
        assert!(term.idle_image_tick(2) > 0);
        term.advance_bytes(format!("\x1b_Gm=1;{HALF_2X2}\x1b\\"));
        // A late continuation must not start accumulating again.
        for _ in 0..3 {
            assert_eq!(term.idle_image_tick(2), 0);
        }
        if tail_ends {
            term.advance_bytes(format!("\x1b_Gm=0;{HALF_2X2}\x1b\\"));
        }
        // A fresh opening must work even when the old producer never sends m=0.
        term.advance_bytes(XMIT_2X2);
        term.advance_bytes("\x1b_Ga=p,i=1\x1b\\");
        assert!(term
            .screen_mut()
            .line_mut(0)
            .get_cell(0)
            .unwrap()
            .attrs()
            .images()
            .is_some());
        let reply = drain(&mut term, &tap);
        assert!(reply.contains("i=1;OK"), "{:?}", reply);
        assert!(
            !reply.contains(";E"),
            "late chunks must be silent: {:?}",
            reply
        );
    }
}

#[test]
fn an_unterminated_chunked_transfer_is_abandoned() {
    let (mut term, tap) = term_with_tap(640, 384);
    // The opening fragment carries the identifying keys; continuation
    // fragments carry only m=.
    term.advance_bytes("\x1b_Ga=t,i=7,f=32,s=2,v=2,m=1;AAAAAAAAAAA=\x1b\\");
    for _ in 0..70_000 {
        term.advance_bytes("\x1b_Gm=1;AAAAAAAAAAA=\x1b\\");
    }

    let reply = drain(&mut term, &tap);
    assert!(
        reply.contains("EFBIG"),
        "an over-long transfer should be refused, got {:?}",
        reply
    );
    assert!(
        reply.matches("EFBIG").count() == 1,
        "the client should be told once, not once per chunk, got {:?}",
        reply
    );
    assert!(
        reply.contains("i=7"),
        "the reply should name the image from the opening fragment, got {:?}",
        reply
    );
}

#[test]
fn a_transfer_after_an_abandoned_one_still_works() {
    let (mut term, _tap) = term_with_tap(640, 384);
    term.advance_bytes("\x1b_Ga=t,i=7,f=32,s=2,v=2,m=1;AAAAAAAAAAA=\x1b\\");
    for _ in 0..70_000 {
        term.advance_bytes("\x1b_Gm=1;AAAAAAAAAAA=\x1b\\");
    }
    // Ending the abandoned transfer should clear the latch.
    term.advance_bytes("\x1b_Gm=0;AAAAAAAAAAA=\x1b\\");

    term.advance_bytes(format!("\x1b_Ga=t,i=11,f=32,s=2,v=2,m=1;{HALF_2X2}\x1b\\"));
    term.advance_bytes(format!("\x1b_Gm=0;{HALF_2X2}\x1b\\"));
    term.advance_bytes("\x1b_Ga=p,i=11\x1b\\");

    let line = term.screen_mut().line_mut(0);
    let cell = line.get_cell(0).expect("cell 0,0 should exist");
    assert!(
        cell.attrs().images().is_some(),
        "a transfer following an abandoned one should succeed"
    );
}

/// Parks exactly `MAX_ACCUM_CHUNKS` fragments, so that the *next* fragment is
/// the first one over the limit. Anything more would trip the cap early and
/// stop the test from saying anything about which fragment was refused.
fn fill_accumulator_to_the_brim(term: &mut Terminal, opening: &str) {
    term.advance_bytes(opening);
    for _ in 0..crate::terminalstate::kitty::MAX_ACCUM_CHUNKS - 1 {
        term.advance_bytes("\x1b_Gm=1;AAAAAAAAAAA=\x1b\\");
    }
}

#[test]
fn the_closing_fragment_is_charged_against_the_cap() {
    let (mut term, tap) = term_with_tap(640, 384);
    fill_accumulator_to_the_brim(
        &mut term,
        "\x1b_Ga=t,i=13,f=32,s=2,v=2,m=1;AAAAAAAAAAA=\x1b\\",
    );

    // Nothing has been refused yet, so this closing fragment is the first one
    // over the limit. Letting m=0 through unmeasured would park the whole
    // budget and then accept another payload on top of it.
    term.advance_bytes("\x1b_Gm=0;AAAAAAAAAAA=\x1b\\");

    let reply = drain(&mut term, &tap);
    assert!(
        reply.contains("EFBIG"),
        "the closing fragment should be charged and refused too, got {:?}",
        reply
    );
    assert!(
        reply.contains("i=13"),
        "the refusal should name the image from the opening fragment, got {:?}",
        reply
    );
}

#[test]
fn a_rejected_transfer_respects_the_opening_fragments_verbosity() {
    let (mut term, tap) = term_with_tap(640, 384);
    // q=2 asks for silence. Continuation fragments carry only m=, which reads
    // as the default q=0, so the rejection must not consult them.
    term.advance_bytes("\x1b_Ga=t,i=17,q=2,f=32,s=2,v=2,m=1;AAAAAAAAAAA=\x1b\\");
    for _ in 0..70_000 {
        term.advance_bytes("\x1b_Gm=1;AAAAAAAAAAA=\x1b\\");
    }

    let reply = drain(&mut term, &tap);
    assert!(
        reply.is_empty(),
        "a transfer that asked for silence should get it, got {:?}",
        reply
    );
}

#[test]
fn a_rejected_unchunked_transfer_does_not_swallow_the_next_one() {
    let (mut term, _tap) = term_with_tap(640, 384);
    fill_accumulator_to_the_brim(
        &mut term,
        "\x1b_Ga=t,i=19,f=32,s=2,v=2,m=1;AAAAAAAAAAA=\x1b\\",
    );
    // Refused as a closing fragment, which ends the transfer; latching on the
    // way out would eat the start of the next one.
    term.advance_bytes("\x1b_Gm=0;AAAAAAAAAAA=\x1b\\");

    term.advance_bytes(XMIT_2X2);
    term.advance_bytes("\x1b_Ga=p,i=1\x1b\\");

    let line = term.screen_mut().line_mut(0);
    let cell = line.get_cell(0).expect("cell 0,0 should exist");
    assert!(
        cell.attrs().images().is_some(),
        "the transfer after a rejected closing fragment should still work"
    );
}

#[test]
fn anonymous_transfers_do_not_inflate_the_memory_accounting() {
    // Every transmission without i= or I= lands on image id 0 and replaces
    // whatever was there. If the replaced image's bytes are not credited back,
    // used_memory climbs forever, and once it passes the prune budget every
    // unplaced image gets evicted on every transfer.
    let mut term = term(640, 384, true);
    let anonymous = "\x1b_Ga=t,f=32,s=2,v=2;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\";

    term.advance_bytes(anonymous);
    let after_one = term.kitty_used_memory();
    assert!(after_one > 0, "the first transfer should be counted");

    for _ in 0..50 {
        term.advance_bytes(anonymous);
    }

    assert!(
        term.kitty_used_memory() == after_one,
        "51 anonymous transfers hold one image, so the accounting should still \
         read one image; got {} where one image is {}",
        term.kitty_used_memory(),
        after_one
    );
}

#[test]
fn a_stored_image_survives_later_anonymous_transfers() {
    // The functional consequence of the accounting bug: transmit-now,
    // place-later stops working once the drift crosses the prune budget.
    let mut term = term(640, 384, true);
    term.advance_bytes(XMIT_2X2);
    for _ in 0..50 {
        term.advance_bytes("\x1b_Ga=t,f=32,s=2,v=2;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");
    }
    term.advance_bytes("\x1b_Ga=p,i=1\x1b\\");

    let line = term.screen_mut().line_mut(0);
    let cell = line.get_cell(0).expect("cell 0,0 should exist");
    assert!(
        cell.attrs().images().is_some(),
        "an image stored with a= t should still be placeable later"
    );
}

#[test]
fn a_virtual_placement_draws_nothing_and_leaves_the_cursor_alone() {
    // U=1 says "the image is ready, I will print U+10EEEE placeholder cells to
    // say where it goes". Rendering those is not implemented, so the placement
    // must draw nothing. Treating U=1 as an ordinary placement paints a stray
    // image at the cursor and moves the cursor, and the placeholder cells the
    // application prints next then render as tofu on top of it.
    let mut term = term(640, 384, true);
    term.advance_bytes(XMIT_2X2);

    let (before_x, before_y) = (term.cursor_pos().x, term.cursor_pos().y);
    term.advance_bytes("\x1b_Ga=p,i=1,U=1,c=20,r=10\x1b\\");
    let (after_x, after_y) = (term.cursor_pos().x, term.cursor_pos().y);

    assert!(
        (before_x, before_y) == (after_x, after_y),
        "a virtual placement must not move the cursor: {:?} -> {:?}",
        (before_x, before_y),
        (after_x, after_y)
    );

    // Nothing was painted, so the line may hold no cells at all — which is the
    // strongest form of "drew nothing".
    let line = term.screen_mut().line_mut(0);
    let attached = line
        .get_cell(0)
        .and_then(|cell| cell.attrs().images())
        .is_some();
    assert!(
        !attached,
        "a virtual placement must not attach an image to any cell"
    );
}

#[test]
fn virtual_grids_publish_replace_and_delete_without_attaching_desktop_cells() {
    use crate::kitty_virtual::VirtualPlacement;
    let mut term = term(640, 384, true);
    term.advance_bytes(XMIT_2X2);
    let (before, _) = term.kitty_frame_selections(None).unwrap();
    term.advance_bytes("\x1b_Ga=p,i=1,U=1,p=3,c=20,r=10\x1b\\");
    let (revision, selections) = term.kitty_frame_selections(Some(before)).unwrap();
    assert_eq!(selections[0].virtual_placements, [VirtualPlacement {
        placement_id: 3, columns: 20, rows: 10,
    }]);
    term.advance_bytes("\x1b_Ga=p,i=1,U=1,p=3,c=20,r=10\x1b\\");
    assert!(term.kitty_frame_selections(Some(revision)).is_none());
    term.advance_bytes("\x1b_Ga=p,i=1,U=1,p=3,c=7,r=4\x1b\\");
    let (revision, selections) = term.kitty_frame_selections(Some(revision)).unwrap();
    assert_eq!(selections[0].virtual_placements[0].columns, 7);
    term.advance_bytes("\x1b_Ga=d,d=a\x1b\\");
    assert!(term.kitty_frame_selections(Some(revision)).is_none());
    term.advance_bytes("\x1b_Ga=d,d=i,i=1,p=3\x1b\\");
    let (_, selections) = term.kitty_frame_selections(Some(revision)).unwrap();
    assert!(selections[0].virtual_placements.is_empty());
    assert!(term.kitty_image_data_for_id(1).is_some());
    assert_eq!((term.cursor_pos().x, term.cursor_pos().y), (0, 0));
    assert!(!term.screen_mut().line_mut(0).has_images());
}

#[test]
fn virtual_grids_cannot_survive_replaced_or_deleted_image_data() {
    let mut term = term(640, 384, true);
    term.advance_bytes(XMIT_2X2);
    term.advance_bytes("\x1b_Ga=p,i=1,U=1,c=20,r=10\x1b\\");
    term.advance_bytes(XMIT_2X2);
    assert!(term.kitty_frame_selections(None).unwrap().1[0].virtual_placements.is_empty());
    term.advance_bytes("\x1b_Ga=p,i=1,U=1,c=20,r=10\x1b\\");
    term.advance_bytes("\x1b_Ga=d,d=I,i=1\x1b\\");
    assert!(term.kitty_frame_selections(None).unwrap().1.is_empty());
    term.advance_bytes("\x1b_Ga=p,i=1,U=1,c=20,r=10\x1b\\");
    assert!(term.kitty_frame_selections(None).unwrap().1.is_empty());
}

#[test]
#[cfg(feature = "use_serde")]
fn virtual_companion_restores_grids_and_rejects_invalid_state_atomically() {
    use crate::kitty_virtual::{VirtualImage, VirtualPlacement, MAX_VIRTUAL_PLACEMENTS};
    let mut original = term(640, 384, true);
    original.advance_bytes(XMIT_2X2);
    original.advance_bytes("\x1b_Ga=p,i=1,U=1,p=3,c=20,r=10\x1b\\");
    original.advance_bytes("\x1b_Ga=p,i=1,U=1,p=5,c=7,r=4\x1b\\");
    original.advance_bytes("\x1b[38;5;1m\u{10eeee}\u{305}\u{305}\x1b[0m");
    let grids = original.snapshot_kitty_virtual();
    let snapshot = original.snapshot();
    let mut copy = term(640, 384, true);
    copy.restore(original.snapshot()).unwrap();
    assert!(copy.snapshot_kitty_virtual().is_empty());
    copy.restore_kitty_virtual(grids.clone()).unwrap();
    assert_eq!(copy.snapshot(), snapshot);
    assert_eq!(copy.snapshot_kitty_virtual(), grids);
    let (revision, _) = copy.kitty_frame_selections(None).unwrap();
    for kind in 0..7 {
        let mut invalid = grids.clone();
        match kind {
            0 => invalid[0].image_id = 999,
            1 => invalid[0].data_hash[0] ^= 1,
            2 => invalid[0].placements.clear(),
            3 => { let duplicate = invalid[0].placements[0]; invalid[0].placements.push(duplicate); },
            4 => invalid[0].placements.reverse(),
            5 => invalid.push(invalid[0].clone()),
            _ => invalid[0].placements = (0..=MAX_VIRTUAL_PLACEMENTS as u32)
                .map(|placement_id| VirtualPlacement { placement_id, columns: 1, rows: 1 }).collect(),
        }
        assert!(copy.restore_kitty_virtual(invalid).is_err());
        assert!(copy.kitty_frame_selections(Some(revision)).is_none());
        assert_eq!(copy.snapshot_kitty_virtual(), grids);
        assert_eq!(copy.snapshot(), snapshot);
    }
    let oversized: Vec<VirtualImage> = std::iter::repeat(grids[0].clone())
        .take(MAX_VIRTUAL_PLACEMENTS + 1).collect();
    assert!(copy.restore_kitty_virtual(oversized).is_err());
    assert!(copy.kitty_frame_selections(Some(revision)).is_none());
    copy.restore_kitty_virtual(Vec::new()).unwrap();
    assert!(copy.snapshot_kitty_virtual().is_empty());
    assert_eq!(copy.snapshot(), snapshot);
}

#[test]
fn an_ordinary_placement_is_unaffected_by_the_virtual_check() {
    let mut term = term(640, 384, true);
    term.advance_bytes(XMIT_2X2);
    term.advance_bytes("\x1b_Ga=p,i=1,U=0\x1b\\");

    let line = term.screen_mut().line_mut(0);
    let cell = line.get_cell(0).expect("cell 0,0 should exist");
    assert!(
        cell.attrs().images().is_some(),
        "U=0 is an ordinary placement and must still paint"
    );
}

/// Sends `seq` to a fresh terminal and returns whatever it wrote back.
fn reply_to(seq: &str) -> String {
    let (mut term, tap) = term_with_tap(640, 384);
    term.advance_bytes(seq);
    drain(&mut term, &tap)
}

#[test]
fn a_query_without_an_image_id_gets_no_reply() {
    // With neither i= nor I= there are no keys to put in the reply, and the
    // old `ESC _ G OK ESC \` was not parseable by this crate's own parser.
    assert!(
        reply_to("\x1b_Ga=q,f=32,s=1,v=1;AAAAAA==\x1b\\").is_empty(),
        "a query that identified no image should not be answered"
    );
}

#[test]
fn a_query_honours_its_verbosity() {
    let loud = reply_to("\x1b_Ga=q,i=31,f=32,s=1,v=1;AAAAAA==\x1b\\");
    assert!(
        loud.contains("i=31") && loud.contains("OK"),
        "q= absent means answer, got {:?}",
        loud
    );

    assert!(
        reply_to("\x1b_Ga=q,i=31,q=2,f=32,s=1,v=1;AAAAAA==\x1b\\").is_empty(),
        "q=2 asks for silence, including for queries"
    );
}

#[test]
fn a_transmit_identified_by_image_id_is_acknowledged() {
    let reply = reply_to("\x1b_Ga=t,i=99,f=32,s=2,v=2;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");
    assert!(
        reply.contains("i=99") && reply.contains("OK"),
        "a transfer identified by i= should be acknowledged, got {:?}",
        reply
    );
}

#[test]
fn an_unidentified_transmit_is_not_acknowledged() {
    assert!(
        reply_to("\x1b_Ga=t,f=32,s=2,v=2;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\").is_empty(),
        "a transfer that named no image has nothing to acknowledge"
    );
}

/// Returns the frame count and per-frame gaps of the image attached at 0,0.
fn animation_at_origin(term: &mut Terminal) -> Option<(usize, Vec<std::time::Duration>)> {
    use wezterm_cell::image::ImageDataType;

    let line = term.screen_mut().line_mut(0);
    let images = line.get_cell(0)?.attrs().images()?;
    let data = images.first()?.image_data().data();
    match &*data {
        ImageDataType::AnimRgba8 {
            frames, durations, ..
        } => Some((frames.len(), durations.clone())),
        _ => None,
    }
}

#[test]
fn a_chunked_animation_frame_reaches_the_image() {
    // Raw RGBA frames are far too big to fit one escape sequence, so every
    // real animation arrives chunked. The frame path used to hand the first
    // fragment straight to the decoder, which rejected it as a short payload,
    // while the fragments after it were misread as a fresh image transfer.
    let mut term = term(640, 384, true);
    term.advance_bytes(XMIT_2X2);
    term.advance_bytes(format!(
        "\x1b_Ga=f,i=1,f=32,s=2,v=2,z=100,m=1;{HALF_2X2}\x1b\\"
    ));
    term.advance_bytes(format!("\x1b_Gm=0;{HALF_2X2}\x1b\\"));
    term.advance_bytes("\x1b_Ga=p,i=1\x1b\\");

    let (frames, durations) =
        animation_at_origin(&mut term).expect("the image should have become an animation");
    assert!(
        frames == 2,
        "the base image plus one transmitted frame is two frames, got {}",
        frames
    );
    // And the gap rides on `z=`, not `Z=`: reading the wrong case left every
    // frame on the 40ms default, pinning every animation to 25fps.
    assert!(
        durations[1] == std::time::Duration::from_millis(100),
        "the frame should keep the gap it asked for, got {:?}",
        durations[1]
    );
}

#[test]
fn an_unchunked_animation_frame_still_works() {
    let mut term = term(640, 384, true);
    term.advance_bytes(XMIT_2X2);
    term.advance_bytes("\x1b_Ga=f,i=1,f=32,s=2,v=2,z=70;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");
    term.advance_bytes("\x1b_Ga=p,i=1\x1b\\");

    let (frames, durations) =
        animation_at_origin(&mut term).expect("a single-sequence frame should still land");
    assert!(frames == 2, "expected two frames, got {}", frames);
    assert!(durations[1] == std::time::Duration::from_millis(70));
}

#[test]
fn an_interleaved_delete_aborts_the_transfer_and_still_executes() {
    // The spec forbids interleaving other commands into a chunked transfer.
    // The failure being prevented: the parked fragments used to wait around
    // and quietly absorb the next transmission — one image corrupted, the
    // other never created, and the client left waiting for an ack.
    let (mut term, tap) = term_with_tap(640, 384);
    term.advance_bytes(XMIT_2X2);
    term.advance_bytes("\x1b_Ga=p,i=1\x1b\\");

    term.advance_bytes(format!("\x1b_Ga=t,i=31,f=32,s=2,v=2,m=1;{HALF_2X2}\x1b\\"));
    term.advance_bytes("\x1b_Ga=d,d=i,i=1\x1b\\");

    let reply = drain(&mut term, &tap);
    assert!(
        reply.contains("EINVAL") && reply.contains("i=31"),
        "the interrupted transfer's opener should be told, got {:?}",
        reply
    );
    assert!(
        reply.matches("EINVAL").count() == 1,
        "told once, got {:?}",
        reply
    );
    assert!(
        term.screen_mut()
            .line_mut(0)
            .get_cell(0)
            .and_then(|c| c.attrs().images())
            .is_none(),
        "the interrupting delete should still execute"
    );

    // And nothing is left parked to corrupt the next transfer.
    term.advance_bytes("\x1b_Ga=t,i=32,f=32,s=2,v=2;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");
    term.advance_bytes("\x1b[H");
    term.advance_bytes("\x1b_Ga=p,i=32\x1b\\");
    assert!(
        term.screen_mut()
            .line_mut(0)
            .get_cell(0)
            .and_then(|c| c.attrs().images())
            .is_some(),
        "transfers after the aborted one should work"
    );
}

#[test]
fn an_interleaved_placement_aborts_the_transfer_and_still_places() {
    let (mut term, tap) = term_with_tap(640, 384);
    term.advance_bytes(XMIT_2X2);
    term.advance_bytes(format!("\x1b_Ga=t,i=41,f=32,s=2,v=2,m=1;{HALF_2X2}\x1b\\"));
    term.advance_bytes("\x1b_Ga=p,i=1\x1b\\");

    let reply = drain(&mut term, &tap);
    assert!(
        reply.contains("EINVAL") && reply.contains("i=41"),
        "the interrupted transfer's opener should be told, got {:?}",
        reply
    );
    assert!(
        term.screen_mut()
            .line_mut(0)
            .get_cell(0)
            .and_then(|c| c.attrs().images())
            .is_some(),
        "the interrupting placement should still paint"
    );
}

#[test]
fn a_fresh_keyed_transfer_clears_the_overflow_latch() {
    let (mut term, tap) = term_with_tap(640, 384);
    fill_accumulator_to_the_brim(
        &mut term,
        "\x1b_Ga=t,i=51,f=32,s=2,v=2,m=1;AAAAAAAAAAA=\x1b\\",
    );
    // The fragment over the cap still promises more data, so the latch arms.
    term.advance_bytes("\x1b_Gm=1;AAAAAAAAAAA=\x1b\\");

    // The producer dies here: no m=0 ever ends the abandoned transfer. The
    // next program's opening fragment carries identifying keys, which no
    // true continuation fragment does, so it must be let through rather
    // than swallowed — swallowing it eats a whole image and leaves that
    // client waiting for an acknowledgement forever.
    term.advance_bytes("\x1b_Ga=t,i=52,f=32,s=2,v=2;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");
    term.advance_bytes("\x1b_Ga=p,i=52\x1b\\");

    let reply = drain(&mut term, &tap);
    assert!(
        reply.matches("EFBIG").count() == 1 && reply.contains("i=51"),
        "the abandoned transfer should be reported once, got {:?}",
        reply
    );
    assert!(
        reply.contains("i=52") && reply.contains("OK"),
        "the fresh transfer should be acknowledged, got {:?}",
        reply
    );
    assert!(
        term.screen_mut()
            .line_mut(0)
            .get_cell(0)
            .and_then(|c| c.attrs().images())
            .is_some(),
        "the fresh transfer should be placeable"
    );
}

#[test]
fn image_id_space_exhaustion_is_enospc_not_a_panic() {
    let (mut term, tap) = term_with_tap(640, 384);
    // Parks the id counter at the ceiling; the ack is expected.
    term.advance_bytes("\x1b_Ga=t,i=4294967295,f=32,s=2,v=2;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");
    // I= asks the terminal to allocate the next id, which does not exist.
    // The old `+ 1` panicked the parser thread here in debug builds and
    // wrapped onto the anonymous id 0 slot in release builds.
    term.advance_bytes("\x1b_Ga=t,I=1,f=32,s=2,v=2;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");

    let reply = drain(&mut term, &tap);
    assert!(
        reply.contains("ENOSPC") && reply.contains("I=1"),
        "id exhaustion should be reported to the client, got {:?}",
        reply
    );

    // Explicitly chosen ids keep working.
    term.advance_bytes("\x1b_Ga=t,i=5,f=32,s=2,v=2;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");
    term.advance_bytes("\x1b_Ga=p,i=5\x1b\\");
    assert!(
        term.screen_mut()
            .line_mut(0)
            .get_cell(0)
            .and_then(|c| c.attrs().images())
            .is_some(),
        "explicit ids should still work after exhaustion"
    );
}

#[test]
fn the_animation_frame_cap_is_reported_to_the_client() {
    let (mut term, tap) = term_with_tap(640, 384);
    term.advance_bytes(XMIT_2X2);

    // The first a=f converts the still image into a two-frame animation;
    // each one after appends a frame. Command number MAX_ANIM_FRAMES is
    // therefore the first append past the cap. The byte cap is far away
    // (2x2 frames), so this exercises the frame-count gate.
    for _ in 0..crate::terminalstate::kitty::MAX_ANIM_FRAMES {
        term.advance_bytes("\x1b_Ga=f,i=1,f=32,s=2,v=2;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");
    }

    let reply = drain(&mut term, &tap);
    assert!(
        reply.matches("EFBIG").count() == 1 && reply.contains("i=1"),
        "the first refused frame should be reported once, got {:?}",
        reply
    );
}

#[test]
fn transmit_and_display_is_not_acknowledged_until_it_displays() {
    // a=T is transmit and display in one command. Answering OK after the
    // transmit half claims success for an image that never appeared — a
    // pane with no pixel geometry makes the display half fail.
    let (mut term, tap) = term_with_tap(0, 0);
    term.advance_bytes("\x1b_Ga=T,i=61,f=32,s=2,v=2;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");
    let reply = drain(&mut term, &tap);
    assert!(
        reply.contains("i=61") && reply.contains("ERROR") && !reply.contains("OK"),
        "a display that failed must not be acknowledged with OK, got {:?}",
        reply
    );

    let (mut term, tap) = term_with_tap(640, 384);
    term.advance_bytes("\x1b_Ga=T,i=62,f=32,s=2,v=2;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");
    let reply = drain(&mut term, &tap);
    assert!(
        reply.contains("i=62") && reply.contains("OK"),
        "a display that worked should be acknowledged, got {:?}",
        reply
    );
    assert!(
        term.screen_mut()
            .line_mut(0)
            .get_cell(0)
            .and_then(|c| c.attrs().images())
            .is_some(),
        "the transmit-and-display image should be on screen"
    );
}

#[test]
fn converting_a_real_placement_to_virtual_erases_it() {
    let mut term = term(640, 384, true);
    term.advance_bytes(XMIT_2X2);
    term.advance_bytes("\x1b_Ga=p,i=1,p=1\x1b\\");
    assert!(
        term.screen_mut()
            .line_mut(0)
            .get_cell(0)
            .and_then(|c| c.attrs().images())
            .is_some(),
        "the real placement should paint first"
    );

    let (before_x, before_y) = (term.cursor_pos().x, term.cursor_pos().y);
    term.advance_bytes("\x1b_Ga=p,i=1,p=1,U=1,c=20,r=10\x1b\\");
    let (after_x, after_y) = (term.cursor_pos().x, term.cursor_pos().y);

    assert!(
        (before_x, before_y) == (after_x, after_y),
        "the virtual conversion must not move the cursor"
    );
    assert!(
        term.screen_mut()
            .line_mut(0)
            .get_cell(0)
            .and_then(|c| c.attrs().images())
            .is_none(),
        "converting a placement to virtual must erase the real one"
    );
}

#[test]
fn a_frame_transmit_marks_the_placed_rows_changed() {
    // Frame edits mutate pixels behind a shared Arc. Nothing downstream
    // notices unless the lines the image occupies are marked changed: the
    // local quad cache keeps its first render, and a mux server sees a clean
    // line and never resends it.
    let mut term = term(640, 384, true);
    term.advance_bytes(XMIT_2X2);
    term.advance_bytes("\x1b_Ga=p,i=1\x1b\\");

    let before = term.screen_mut().line_mut(0).current_seqno();
    term.advance_bytes("\x1b_Ga=f,i=1,f=32,s=2,v=2,z=70;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");
    let after = term.screen_mut().line_mut(0).current_seqno();

    assert!(
        after > before,
        "the placed row should be marked changed by a frame transmit: {} -> {}",
        before,
        after
    );
}

#[test]
fn animation_growth_is_counted_against_the_memory_budget() {
    let mut term = term(640, 384, true);
    term.advance_bytes(XMIT_2X2);
    let one_frame = term.kitty_used_memory();

    term.advance_bytes("\x1b_Ga=f,i=1,f=32,s=2,v=2,z=70;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");

    assert!(
        term.kitty_used_memory() > one_frame,
        "appending a frame grows the image, so the budget should see it: \
         still {} after growing past {}",
        term.kitty_used_memory(),
        one_frame
    );
}

#[test]
fn ris_releases_image_memory() {
    let mut term = term(640, 384, true);
    term.advance_bytes(XMIT_2X2);
    assert!(term.kitty_used_memory() > 0);

    term.advance_bytes("\x1bc");

    assert!(
        term.kitty_used_memory() == 0,
        "a hard reset should release image data, still holding {}",
        term.kitty_used_memory()
    );
    // And the id must no longer resolve.
    term.advance_bytes("\x1b_Ga=p,i=1\x1b\\");
    let line = term.screen_mut().line_mut(0);
    assert!(
        line.get_cell(0).and_then(|c| c.attrs().images()).is_none(),
        "an image dropped by RIS must not be placeable"
    );
}

#[test]
fn clearing_the_scrollback_and_viewport_keeps_reusable_image_data() {
    // Applications transmit an image once and re-place it on every redraw.
    // Clearing the screen erases the placements; the data stays until it
    // outgrows the memory budget (kitty behaves the same way), so an a=p
    // naming the old id after a clear must still work.
    let mut term = term(640, 384, true);
    term.advance_bytes(XMIT_2X2);
    term.advance_bytes("\x1b_Ga=p,i=1\x1b\\");
    // A second image makes id 1 ineligible for the newest-image exemption.
    term.advance_bytes("\x1b_Ga=T,i=2,f=32,s=2,v=2;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");
    assert!(term.kitty_used_memory() > 0);

    term.erase_scrollback_and_viewport();

    assert!(
        term.screen_mut()
            .line_mut(0)
            .get_cell(0)
            .and_then(|c| c.attrs().images())
            .is_none(),
        "clearing must remove the painted placement"
    );

    for _ in 0..4 {
        term.idle_image_tick(2);
    }
    assert!(
        term.kitty_image_data_for_id(1).is_some(),
        "cleared image data must survive the idle sweep even when not newest"
    );
    assert!(term.kitty_image_data_for_id(2).is_some());
    term.advance_bytes("\x1b[H");
    term.advance_bytes("\x1b_Ga=p,i=1\x1b\\");
    assert!(
        term.screen_mut()
            .line_mut(0)
            .get_cell(0)
            .and_then(|c| c.attrs().images())
            .is_some(),
        "the stored image data should still be placeable after a clear"
    );
}

#[test]
fn deleting_one_or_all_placements_keeps_idle_image_data_reusable() {
    for delete_placements in ["\x1b_Ga=d,d=i,i=1\x1b\\", "\x1b_Ga=d,d=a\x1b\\"] {
        let mut term = term(640, 384, true);
        term.advance_bytes(XMIT_2X2);
        term.advance_bytes("\x1b_Ga=p,i=1\x1b\\");
        term.advance_bytes("\x1b_Ga=T,i=2,f=32,s=2,v=2;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");
        term.advance_bytes(delete_placements);
        assert!(
            term.screen_mut()
                .line_mut(0)
                .get_cell(0)
                .and_then(|c| c.attrs().images())
                .is_none()
        );

        for _ in 0..6 {
            term.idle_image_tick(2);
        }
        assert!(term.kitty_image_data_for_id(1).is_some());
        assert!(term.kitty_image_data_for_id(2).is_some());
        term.advance_bytes("\x1b[H\x1b_Ga=p,i=1\x1b\\");
        assert!(
            term.screen_mut()
                .line_mut(0)
                .get_cell(0)
                .and_then(|c| c.attrs().images())
                .is_some(),
            "deleting placements must not expire the reusable data: {:?}",
            delete_placements
        );

        // Explicit data deletion still releases the image immediately.
        term.advance_bytes("\x1b_Ga=d,d=I,i=1\x1b\\");
        assert!(term.kitty_image_data_for_id(1).is_none());
        assert!(term.kitty_image_data_for_id(2).is_some());
    }
}

#[test]
fn eviction_reaches_a_placement_on_the_inactive_screen() {
    // Budget: two 2x2 frames. Image 1 is placed on the primary screen;
    // the stream that busts the budget runs on the alt screen. Eviction
    // must detach image 1 from the primary screen's cells even though the
    // alt screen is active when the sweep fires -- aiming at the active
    // screen would free nothing and the picture would revive on switch.
    let mut term = term_with_budget(640, 384, true, 2 * 16);
    let one = base64_of(&[1u8; 16]);
    term.advance_bytes(format!("\x1b[H\x1b_Ga=T,i=1,f=32,s=2,v=2;{one}\x1b\\"));
    term.advance_bytes("\x1b[?1049h");
    for id in 2..=5 {
        let pixels = base64_of(&[id as u8; 16]);
        term.advance_bytes(format!(
            "\x1b[H\x1b_Ga=T,i={id},f=32,s=2,v=2;{pixels}\x1b\\"
        ));
    }
    assert!(
        term.kitty_used_memory() <= 2 * 16,
        "stored image bytes {} exceed the budget",
        term.kitty_used_memory()
    );
    term.advance_bytes("\x1b[?1049l");
    let line = term.screen_mut().line_mut(0);
    let cell = line.get_cell(0).expect("cell 0,0");
    let holds_evicted = cell
        .attrs()
        .images()
        .map(|imgs| imgs.iter().any(|img| img.image_id() == Some(1)))
        .unwrap_or(false);
    assert!(
        !holds_evicted,
        "the evicted image is still attached to the primary screen"
    );
}

#[test]
fn appended_animation_frames_stay_within_the_image_budget() {
    // Budget: four 2x2 RGBA frames (64 bytes). Two still images fill half
    // of it; growing the first into an animation must reclaim the other
    // (unreferenced) picture before it is allowed to grow further, and
    // must be refused once the animation alone would fill the budget.
    let mut term = term_with_budget(640, 384, true, 4 * 16);
    // Placed, so the sweep treats it as in use and reclaims image 2 instead.
    term.advance_bytes("\x1b[H\x1b_Ga=T,i=1,f=32,s=2,v=2;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");
    // Different pixels from image 1: identical small images are content-
    // deduplicated into one shared object, and growing that object would
    // grow both.
    term.advance_bytes("\x1b_Ga=t,i=2,f=32,s=2,v=2;/////////////////////w==\x1b\\");
    k9::assert_equal!(term.kitty_used_memory(), 32);
    for _ in 0..6 {
        term.advance_bytes("\x1b_Ga=f,i=1,f=32,s=2,v=2;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");
        assert!(
            term.kitty_used_memory() <= 4 * 16,
            "stored image bytes {} exceed the budget after an append",
            term.kitty_used_memory()
        );
    }
    // Four frames of image 1 are exactly the budget; image 2 made room.
    k9::assert_equal!(term.kitty_used_memory(), 4 * 16);
}

/// Standard base64 for test payloads; the crate has no encoder of its own.
pub(super) fn base64_of(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let mut word = 0u32;
        for (i, byte) in chunk.iter().enumerate() {
            word |= (*byte as u32) << (16 - 8 * i);
        }
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((word >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[test]
fn a_frame_stream_with_fresh_ids_is_capped_by_the_image_budget() {
    // Every frame is transmitted-and-displayed under a new id on the same
    // cells and never deleted, the way a streaming client behaves. Placed
    // images stack, so without a hard cap each cell keeps every frame ever
    // shown. Budget: three 2x2 RGBA images (16 bytes each).
    let mut term = term_with_budget(640, 384, true, 3 * 16);
    for id in 1..=20 {
        // Home first: a=T advances the cursor, and the point is to stack
        // every frame on the same cells like a redrawing client does.
        // Distinct pixels per frame, as a real stream has: identical small
        // payloads are content-deduplicated into one shared object, which
        // the budget correctly counts once.
        let pixels = base64_of(&[id as u8; 16]);
        term.advance_bytes(format!(
            "\x1b[H\x1b_Ga=T,i={id},f=32,s=2,v=2;{pixels}\x1b\\"
        ));
    }
    assert!(
        term.kitty_used_memory() <= 3 * 16,
        "stored image bytes {} exceed the budget",
        term.kitty_used_memory()
    );
    let line = term.screen_mut().line_mut(0);
    let cell = line.get_cell(0).expect("cell 0,0");
    let stacked = cell.attrs().images().map(|v| v.len()).unwrap_or(0);
    assert!(
        (1..=3).contains(&stacked),
        "cell keeps {} frames; expected the newest few, not all twenty",
        stacked
    );
    assert!(
        cell.attrs()
            .images()
            .unwrap()
            .iter()
            .any(|img| img.image_id() == Some(20)),
        "the newest frame must survive eviction"
    );
}

/// Physical rows of `screen` that changed after `since`.
fn dirty_phys_rows(screen: &crate::Screen, since: wezterm_surface::SequenceNo) -> Vec<usize> {
    let mut rows = vec![];
    screen.for_each_phys_line(|idx, line| {
        if line.changed_since(since) {
            rows.push(idx);
        }
    });
    rows
}

/// One appended 2x2 animation frame for image 1.
const FRAME_2X2: &str = "\x1b_Ga=f,i=1,f=32,s=2,v=2,z=70;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\";

#[test]
fn a_screen_switch_marks_the_rows_now_on_view_changed() {
    // A mux client caches lines by stable index and refetches only what
    // moved past the seqno it last saw. Switching screens replaces what
    // every visible row shows without writing a line, so the switch has
    // to stamp them itself, on the screen that is now active. The old code
    // stamped the primary screen's physical rows 0..n, which once there is
    // scrollback are its oldest history lines: the alternate screen came
    // up with stale seqnos and the client kept painting the primary one.
    let mut term = term(640, 384, true);
    for i in 0..40 {
        term.advance_bytes(format!("line {i}\r\n"));
    }
    let rows = term.screen().physical_rows as i64;

    let before_alt = term.current_seqno();
    term.advance_bytes("\x1b[?1049h");
    let alt = term.screen();
    std::assert_eq!(
        dirty_phys_rows(alt, before_alt),
        alt.phys_range(&(0..rows)).collect::<Vec<_>>(),
        "every alternate-screen row must be marked changed by the switch"
    );
    assert!(
        dirty_phys_rows(term.screen_for_alt(false), before_alt).is_empty(),
        "entering the alternate screen must not touch the primary screen, \
         least of all its scrollback: {:?}",
        dirty_phys_rows(term.screen_for_alt(false), before_alt)
    );

    let before_primary = term.current_seqno();
    term.advance_bytes("\x1b[?1049l");
    let primary = term.screen();
    std::assert_eq!(
        dirty_phys_rows(primary, before_primary),
        primary.phys_range(&(0..rows)).collect::<Vec<_>>(),
        "the rows on view, and only those, must be marked changed when \
         the primary screen returns"
    );
}

#[test]
fn a_frame_dirties_the_rows_holding_its_placement() {
    let mut term = term(640, 384, true);
    term.advance_bytes("\x1b[H");
    term.advance_bytes(XMIT_2X2);
    term.advance_bytes("\x1b_Ga=p,i=1\x1b\\");

    let before = term.current_seqno();
    term.advance_bytes(FRAME_2X2);
    std::assert_eq!(
        dirty_phys_rows(term.screen(), before),
        vec![0],
        "the row carrying the placement must be marked changed by the frame"
    );
}

#[test]
fn a_frame_moves_the_image_generation_but_not_its_hash() {
    // A mux client holds a copy of the image and sees only the hash on the
    // wire. The hash must stay (it is the glyph cache's key, and a new one
    // per frame would restart the animation), so the generation is what
    // tells the client its copy fell behind.
    let mut term = term(640, 384, true);
    term.advance_bytes("\x1b[H");
    term.advance_bytes(XMIT_2X2);
    term.advance_bytes("\x1b_Ga=p,i=1\x1b\\");
    // The first mutation detaches from immutable content deduplication.
    term.advance_bytes(FRAME_2X2);
    let image = term
        .screen()
        .lines_in_phys_range(0..1)
        .remove(0)
        .get_cell(0)
        .and_then(|c| c.attrs().images())
        .and_then(|imgs| imgs.into_iter().next())
        .map(|img| Arc::clone(img.image_data()))
        .expect("the placement attached the image");
    let (hash, generation) = (image.hash(), image.generation());

    term.advance_bytes(FRAME_2X2);
    std::assert_eq!(image.hash(), hash, "the identity must not change");
    assert!(
        image.generation() > generation,
        "the generation must move: {} -> {}",
        generation,
        image.generation()
    );
}

#[test]
fn a_frame_for_a_placement_on_the_other_screen_dirties_that_screen() {
    // Frames keep arriving for a picture on the primary screen while a
    // full-screen app has the alternate one up. Dirtying the active screen
    // left the placement's rows clean, so a mux server never resent them
    // and the remote pane stayed on the first frame -- while unrelated rows
    // of the alternate screen were resent for nothing.
    let mut term = term(640, 384, true);
    term.advance_bytes("\x1b[H");
    term.advance_bytes(XMIT_2X2);
    term.advance_bytes("\x1b_Ga=p,i=1\x1b\\");
    term.advance_bytes("\x1b[?1049h");

    let before = term.current_seqno();
    term.advance_bytes(FRAME_2X2);
    assert!(
        dirty_phys_rows(term.screen(), before).is_empty(),
        "the frame must not mark alternate-screen rows changed; they do \
         not carry the picture: {:?}",
        dirty_phys_rows(term.screen(), before)
    );
    std::assert_eq!(
        dirty_phys_rows(term.screen_for_alt(false), before),
        vec![0],
        "the primary-screen row holding the placement must be marked changed"
    );
}

#[test]
fn security_sixel_repeat_is_bounded_by_the_declared_image() {
    let mut terminal = term(640, 384, false);
    for repeat in ["!4294967295?", "!?"] {
        terminal.advance_bytes(format!("\x1bPq\"1;1;1;1{repeat}\x1b\\"));
    }
    terminal.advance_bytes("still alive");
}

#[test]
fn security_kitty_reply_filters_control_bytes_for_all_transmits() {
    use wezterm_escape_parser::apc::{KittyImage, KittyImageData};
    use wezterm_escape_parser::Action;
    let (mut terminal, tap) = term_with_tap(640, 384);
    for action in ["q", "t", "T"] {
        let mut image = KittyImage::parse_apc(format!("Ga={action},i=42,f=32,s=1,v=1;AAAAAA==").as_bytes()).unwrap();
        let data = KittyImageData::MaterializedError {
            kind: std::io::ErrorKind::InvalidInput,
            message: format!("bad\n\r\x1b[31m\x07\x7f非ASCII{}", "x".repeat(400)),
        };
        match &mut image {
            KittyImage::Query { transmit, .. }
            | KittyImage::TransmitData { transmit, .. }
            | KittyImage::TransmitDataAndDisplay { transmit, .. } => transmit.data = data,
            _ => unreachable!(),
        }
        terminal.perform_actions(vec![Action::KittyImage(Box::new(image))]);
    }
    let replies = drain(&mut terminal, &tap);
    assert_eq!(replies.matches("\x1b_Gi=42;").count(), 3);
    for reply in replies.split("\x1b_Gi=42;").skip(1) {
        let text = reply.strip_suffix("\x1b\\").unwrap();
        assert!(text.len() <= 256);
        assert!(text.bytes().all(|b| (b' '..=b'~').contains(&b)));
    }
}

#[test]
fn security_kitty_placement_caps_wire_cell_counts() {
    kitty(640, 384, "\x1b_Ga=p,i=1,c=4294967295,r=4294967295\x1b\\");
}


#[test]
fn kitty_mutations_do_not_alias_other_ids_or_the_content_cache() {
    use wezterm_cell::image::ImageDataType;
    for alternate in [false, true] {
        let mut terminal = term(640, 384, true);
        terminal.advance_bytes(XMIT_2X2);
        terminal.advance_bytes("\x1b_Ga=t,i=2,f=32,s=2,v=2;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");
        let original = terminal.kitty_image_data_for_id(1).unwrap();
        assert!(Arc::ptr_eq(&original, &terminal.kitty_image_data_for_id(2).unwrap()));
        terminal.advance_bytes("\x1b_Ga=p,i=1,p=1,c=2,r=2,C=1,z=-2\x1b\\");
        terminal.advance_bytes("\x1b_Ga=p,i=2,p=1,c=2,r=2,C=1,z=3\x1b\\");
        let cached_row = terminal.screen().lines_in_phys_range(0..1).remove(0);
        terminal.screen_mut().line_mut(0).compress_for_scrollback();
        if alternate {
            terminal.advance_bytes("\x1b[?1049h");
        }
        let before = terminal.current_seqno();
        terminal.advance_bytes("\x1b_Ga=f,i=1,f=32,s=1,v=1,r=1;/////w==\x1b\\");
        let edited = terminal.kitty_image_data_for_id(1).unwrap();
        assert_ne!(edited.hash(), original.hash());
        assert!(ImageDataType::is_nonce_key(&edited.hash()));
        assert_eq!(edited.generation(), 1);
        assert_eq!(terminal.kitty_used_memory(), 32, "detached pixels count separately");
        {
            let pixels = original.data();
            let ImageDataType::Rgba8 { data, .. } = &*pixels else { panic!("static image"); };
            assert!(data.iter().all(|byte| *byte == 0));
        }
        assert_eq!(original.generation(), 0);
        assert!(Arc::ptr_eq(&original, &terminal.kitty_image_data_for_id(2).unwrap()));
        if alternate {
            assert!(dirty_phys_rows(terminal.screen(), before).is_empty());
            terminal.advance_bytes("\x1b[?1049l");
        }
        let live_row = terminal.screen().lines_in_phys_range(0..1).remove(0);
        let live = live_row.get_cell(0).unwrap().attrs().images().unwrap().remove(0);
        let cached = cached_row.get_cell(0).unwrap().attrs().images().unwrap().remove(0);
        assert!(Arc::ptr_eq(live.image_data(), &edited));
        assert!(live_row.get_cell(0).unwrap().attrs().image_attachments()
            .any(|im| im.image_id() == Some(2) && Arc::ptr_eq(im.image_data(), &original)));
        assert!(Arc::ptr_eq(cached.image_data(), &original), "retained line clones are immutable");
        assert_eq!(live.z_index(), cached.z_index());
        assert_eq!(live.top_left(), cached.top_left());
        assert_eq!(live.bottom_right(), cached.bottom_right());
        assert_eq!(live.padding(), cached.padding());
        assert_eq!(live.placement_id(), cached.placement_id());
        assert!(live_row.current_seqno() > before);
        // Later edits and composition reuse the private image, without copying.
        terminal.advance_bytes("\x1b_Ga=f,i=1,f=32,s=1,v=1,r=1;AQIDBA==\x1b\\");
        terminal.advance_bytes("\x1b_Ga=c,i=1,r=1,c=1,x=1,y=1,w=1,h=1,C=1\x1b\\");
        assert!(Arc::ptr_eq(&edited, &terminal.kitty_image_data_for_id(1).unwrap()));
        assert_eq!(edited.generation(), 3);
        terminal.advance_bytes("\x1b_Ga=d,d=I,i=1\x1b\\");
        terminal.advance_bytes("\x1b_Ga=t,i=3,f=32,s=2,v=2;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");
        assert!(Arc::ptr_eq(&original, &terminal.kitty_image_data_for_id(3).unwrap()));
        assert_eq!(terminal.kitty_used_memory(), 16);
    }
}

#[test]
fn kitty_first_composition_detaches_and_invalid_edits_do_not_change_identity() {
    let mut terminal = term(640, 384, true);
    terminal.advance_bytes("\x1b_Ga=t,i=1,f=32,s=2,v=1;/////wAAAAA=\x1b\\");
    terminal.advance_bytes("\x1b_Ga=t,i=2,f=32,s=2,v=1;/////wAAAAA=\x1b\\");
    let original = terminal.kitty_image_data_for_id(1).unwrap();
    terminal.advance_bytes("\x1b_Ga=f,i=1,f=32,s=1,v=1,r=99;AQIDBA==\x1b\\");
    terminal.advance_bytes("\x1b_Ga=c,i=1,r=99,c=1,w=1,h=1\x1b\\");
    assert!(Arc::ptr_eq(&original, &terminal.kitty_image_data_for_id(1).unwrap()));
    assert_eq!(original.generation(), 0);
    terminal.advance_bytes("\x1b_Ga=c,i=1,r=1,c=1,x=1,w=1,h=1,C=1\x1b\\");
    let edited = terminal.kitty_image_data_for_id(1).unwrap();
    assert_ne!(original.hash(), edited.hash());
    assert_eq!(edited.generation(), 1);
    assert!(Arc::ptr_eq(&original, &terminal.kitty_image_data_for_id(2).unwrap()));
    assert_eq!(terminal.kitty_used_memory(), 16);
}

#[test]
fn kitty_images_with_equal_bytes_but_different_dimensions_do_not_alias() {
    let mut terminal = term(640, 384, true);
    terminal.advance_bytes("\x1b_Ga=t,i=1,f=32,s=2,v=1;AAAAAAAAAAA=\x1b\\");
    terminal.advance_bytes("\x1b_Ga=t,i=2,f=32,s=1,v=2;AAAAAAAAAAA=\x1b\\");
    let a = terminal.kitty_image_data_for_id(1).unwrap();
    let b = terminal.kitty_image_data_for_id(2).unwrap();
    assert_ne!(a.hash(), b.hash());
    assert_eq!(a.data().dimensions().unwrap(), (2, 1));
    assert_eq!(b.data().dimensions().unwrap(), (1, 2));
}

#[test]
#[cfg(feature = "use_serde")]
fn kitty_detached_identity_survives_snapshot_restore() {
    let mut original = term(640, 384, true);
    original.advance_bytes(XMIT_2X2);
    original.advance_bytes("\x1b_Ga=t,i=2,f=32,s=2,v=2;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");
    original.advance_bytes("\x1b_Ga=p,i=1,p=1,c=2,r=2,C=1\x1b\\");
    original.advance_bytes("\x1b_Ga=f,i=1,f=32,s=1,v=1,r=1;/////w==\x1b\\");
    let mut restored = term(640, 384, true);
    restored.restore(original.snapshot()).unwrap();
    let edited = restored.kitty_image_data_for_id(1).unwrap();
    let other = restored.kitty_image_data_for_id(2).unwrap();
    assert_ne!(edited.hash(), other.hash());
    let row = restored.screen().lines_in_phys_range(0..1).remove(0);
    assert_eq!(row.get_cell(0).unwrap().attrs().image_attachments().count(), 1);
    assert!(row.get_cell(0).unwrap().attrs().image_attachments().all(|im| Arc::ptr_eq(im.image_data(), &edited)));
    restored.advance_bytes(FRAME_2X2);
    assert!(Arc::ptr_eq(&edited, &restored.kitty_image_data_for_id(1).unwrap()));
    assert_eq!(other.len(), 16);
    assert_eq!(restored.kitty_used_memory(), 48);
}

#[test]
fn kitty_detaching_shared_pixels_enforces_the_image_budget() {
    for compose in [false, true] {
        let mut terminal = term_with_budget(640, 384, true, 16);
        terminal.advance_bytes(XMIT_2X2);
        terminal.advance_bytes("\x1b_Ga=t,i=2,f=32,s=2,v=2;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");
        assert_eq!(terminal.kitty_used_memory(), 16);
        terminal.advance_bytes(if compose {
            "\x1b_Ga=c,i=1,r=1,c=1,w=1,h=1\x1b\\"
        } else {
            "\x1b_Ga=f,i=1,f=32,s=1,v=1,r=1;/////w==\x1b\\"
        });
        assert!(terminal.kitty_image_data_for_id(1).is_some());
        assert!(terminal.kitty_image_data_for_id(2).is_none());
        assert_eq!(terminal.kitty_used_memory(), 16);
    }
}

fn kitty_placement_keys(term: &Terminal, alternate: bool) -> std::collections::BTreeSet<(u32, Option<u32>)> {
    let mut keys = std::collections::BTreeSet::new();
    term.screen_for_alt(alternate).for_each_phys_line(|_, line| {
        if line.has_images() {
            for cell in line.visible_cells() {
                for image in cell.attrs().image_attachments() {
                    if let Some(id) = image.image_id() {
                        keys.insert((id, image.placement_id()));
                    }
                }
            }
        }
    });
    keys
}

fn place_kitty_for_delete(term: &mut Terminal, id: u32, placement: u32, row: usize, col: usize, z: i32) {
    term.advance_bytes(format!("\x1b[{row};{col}H\x1b_Ga=p,i={id},p={placement},c=2,r=2,C=1,z={z}\x1b\\"));
}

#[test]
fn kitty_spatial_deletion_selects_whole_placements_from_actual_cells() {
    let cases: &[(&str, &[u32])] = &[
        ("a", &[1, 2, 3]), ("c", &[1, 2]), ("p,x=4,y=2", &[1, 2]),
        ("q,x=4,y=2,z=-2", &[1]), ("x,x=9", &[3]), ("y,y=6", &[3]),
        ("z,z=-2", &[1, 3]), ("r,x=1,y=2", &[1, 2]),
        ("p,x=0,y=2", &[]), ("r,x=3,y=1", &[]),
    ];
    for &(selector, removed) in cases {
        for uppercase in [false, true] {
            for compressed in [false, true] {
                let mut term = term(640, 384, true);
                for id in 1..=3 {
                    term.advance_bytes(XMIT_2X2.replace("i=1", &format!("i={id}")));
                }
                place_kitty_for_delete(&mut term, 1, 1, 2, 3, -2);
                place_kitty_for_delete(&mut term, 2, 1, 2, 4, 3);
                place_kitty_for_delete(&mut term, 3, 1, 6, 9, -2);
                term.advance_bytes("\x1b[2;4H");
                if compressed {
                    term.screen_mut().for_each_phys_line_mut(|_, line| line.compress_for_scrollback());
                }
                let cursor = term.cursor_pos();
                let selector = if uppercase {
                    format!("{}{}", selector[..1].to_uppercase(), &selector[1..])
                } else { selector.to_string() };
                term.advance_bytes(format!("\x1b_Ga=d,d={selector}\x1b\\"));
                let expected = (1..=3).filter(|id| !removed.contains(id)).map(|id| (id, Some(1))).collect();
                assert_eq!(kitty_placement_keys(&term, false), expected, "{selector}");
                assert_eq!(term.cursor_pos(), cursor);
                for id in 1..=3 {
                    assert_eq!(term.kitty_image_data_for_id(id).is_some(), !uppercase || !removed.contains(&id), "{selector} id={id}");
                }
            }
        }
    }
}

#[test]
fn kitty_uppercase_delete_retains_virtual_and_other_placement_references() {
    for selector in ["A", "C", "P,x=1,y=1", "Q,x=1,y=1,z=0", "X,x=1", "Y,y=1", "Z,z=0", "I,i=1,p=2"] {
        let mut term = term(640, 384, true);
        term.advance_bytes(XMIT_2X2);
        place_kitty_for_delete(&mut term, 1, 2, 1, 1, 0);
        term.advance_bytes("\x1b_Ga=p,i=1,U=1,p=3,c=2,r=2\x1b\\");
        term.advance_bytes(format!("\x1b_Ga=d,d={selector}\x1b\\"));
        assert!(kitty_placement_keys(&term, false).is_empty(), "{selector}");
        assert!(term.kitty_image_data_for_id(1).is_some(), "{selector}");
        assert_eq!(term.snapshot_kitty_virtual()[0].placements[0].placement_id, 3);
        term.advance_bytes("\x1b_Ga=d,d=I,i=1,p=999\x1b\\");
        assert!(term.kitty_image_data_for_id(1).is_some());
        term.advance_bytes("\x1b_Ga=d,d=I,i=1,p=0\x1b\\");
        assert!(term.kitty_image_data_for_id(1).is_none());
        assert!(term.snapshot_kitty_virtual().is_empty());
    }
    let mut term = term(640, 384, true);
    term.advance_bytes(XMIT_2X2);
    place_kitty_for_delete(&mut term, 1, 2, 1, 1, 0);
    place_kitty_for_delete(&mut term, 1, 3, 4, 1, 0);
    term.advance_bytes("\x1b_Ga=d,d=I,i=1,p=2\x1b\\");
    assert_eq!(kitty_placement_keys(&term, false), [(1, Some(3))].into());
    assert!(term.kitty_image_data_for_id(1).is_some());
}

#[test]
fn kitty_delete_all_preserves_scrollback_inactive_screen_and_unplaced_data() {
    let mut term = term(640, 384, true);
    for id in 1..=3 { term.advance_bytes(XMIT_2X2.replace("i=1", &format!("i={id}"))); }
    place_kitty_for_delete(&mut term, 1, 1, 1, 1, 0);
    term.advance_bytes("\x1b[24;1H\n\n\n");
    place_kitty_for_delete(&mut term, 1, 2, 4, 1, 0);
    term.advance_bytes("\x1b_Ga=d,d=A\x1b\\");
    assert_eq!(kitty_placement_keys(&term, false), [(1, Some(1))].into());
    assert!(term.kitty_image_data_for_id(1).is_some());
    assert!(term.kitty_image_data_for_id(2).is_some());
    term.advance_bytes("\x1b[?1049h");
    place_kitty_for_delete(&mut term, 3, 1, 1, 1, 0);
    term.advance_bytes("\x1b_Ga=d,d=A\x1b\\");
    assert!(kitty_placement_keys(&term, true).is_empty());
    assert_eq!(kitty_placement_keys(&term, false), [(1, Some(1))].into());
    assert!(term.kitty_image_data_for_id(3).is_none());
    term.advance_bytes("\x1b_Ga=d,d=R,x=1,y=2\x1b\\");
    assert!(kitty_placement_keys(&term, false).is_empty());
    assert_eq!(term.kitty_used_memory(), 0);
}

#[test]
fn kitty_delete_by_id_finds_reflowed_cells_and_preserves_shared_rows() {
    let mut term = term(640, 384, true);
    term.advance_bytes(XMIT_2X2);
    place_kitty_for_delete(&mut term, 1, 1, 1, 75, -2);
    term.advance_bytes("\x1b[1;80HX");
    let size = TerminalSize { rows: 24, cols: 20, pixel_width: 160, pixel_height: 384, dpi: 96 };
    term.resize(size);
    let saved: Vec<_> = (0..6).map(|row| term.screen_mut().line_mut(row).clone()).collect();
    assert!(saved.iter().skip(2).any(|line| line.has_images()));
    term.advance_bytes("\x1b_Ga=d,d=I,i=1\x1b\\");
    assert!(kitty_placement_keys(&term, false).is_empty());
    assert!(saved.iter().any(|line| line.has_images()));
    assert!(term.kitty_image_data_for_id(1).is_none());
}

#[test]
fn kitty_number_and_range_delete_select_virtual_and_unplaced_images() {
    let mut term = term(640, 384, true);
    term.advance_bytes(XMIT_2X2);
    term.advance_bytes("\x1b_Ga=t,I=7,f=32,s=2,v=2;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\");
    let id = term.kitty_frame_selections(None).unwrap().1.iter().map(|s| s.image_id).max().unwrap();
    term.advance_bytes("\x1b_Ga=p,I=7,U=1,p=4,c=2,r=2\x1b\\");
    term.advance_bytes("\x1b_Ga=d,d=n,I=7,p=4\x1b\\");
    assert!(term.snapshot_kitty_virtual().is_empty());
    assert!(term.kitty_image_data_for_id(id).is_some());
    term.advance_bytes("\x1b_Ga=d,d=N,I=7\x1b\\");
    assert!(term.kitty_image_data_for_id(id).is_none());
    assert!(term.kitty_image_data_for_id(1).is_some());
    term.advance_bytes("\x1b_Ga=p,i=1,U=1,p=4,c=2,r=2\x1b\\");
    term.advance_bytes("\x1b_Ga=d,d=r,x=1,y=1\x1b\\");
    assert!(term.snapshot_kitty_virtual().is_empty());
    assert!(term.kitty_image_data_for_id(1).is_some());
    term.advance_bytes("\x1b_Ga=d,d=R,x=1,y=1\x1b\\");
    assert_eq!(term.kitty_used_memory(), 0);
}

#[test]
fn kitty_frame_deletion_replaces_identity_and_keeps_remaining_frames_and_timeline() {
    use wezterm_cell::image::ImageDataType;
    let mut term = term(640, 384, true);
    term.advance_bytes("\x1b_Ga=t,i=1,f=32,s=1,v=1;/wAA/w==\x1b\\");
    term.advance_bytes("\x1b_Ga=f,i=1,f=32,s=1,v=1,z=30;AP8A/w==\x1b\\");
    term.advance_bytes("\x1b_Ga=f,i=1,f=32,s=1,v=1,z=40;AAD//w==\x1b\\");
    term.advance_bytes("\x1b_Ga=a,i=1,r=1,z=20,c=3,s=1\x1b\\");
    place_kitty_for_delete(&mut term, 1, 1, 1, 1, 0);
    term.advance_bytes("\x1b_Ga=p,i=1,U=1,p=2,c=2,r=2\x1b\\");
    let old = term.kitty_image_data_for_id(1).unwrap();
    term.advance_bytes("\x1b_Ga=d,d=f,i=1,r=0\x1b\\");
    let image = term.kitty_image_data_for_id(1).unwrap();
    assert_ne!(old.hash(), image.hash());
    assert_eq!(image.generation(), old.generation() + 1);
    assert_eq!(term.kitty_used_memory(), 8);
    assert!(matches!(&*old.data(), ImageDataType::AnimRgba8 { frames, .. } if frames.len() == 3));
    match &*image.data() {
        ImageDataType::AnimRgba8 { frames, .. } => assert_eq!(frames, &[vec![0, 255, 0, 255], vec![0, 0, 255, 255]]),
        _ => panic!("expected remaining animation"),
    }
    let selection = &term.kitty_frame_selections(None).unwrap().1[0];
    assert_eq!(selection.animation.frame_ends, [30, 70]);
    assert_eq!(selection.animation.frame, 1);
    assert_eq!(selection.virtual_placements.len(), 1);
    assert_eq!(kitty_placement_keys(&term, false), [(1, Some(1))].into());
    term.advance_bytes("\x1b_Ga=d,d=f,i=1,r=999\x1b\\");
    let image = term.kitty_image_data_for_id(1).unwrap();
    assert!(matches!(&*image.data(), ImageDataType::Rgba8 { data, .. } if data == &[0, 255, 0, 255]));
    term.advance_bytes("\x1b_Ga=d,d=f,i=1\x1b\\");
    assert!(Arc::ptr_eq(&image, &term.kitty_image_data_for_id(1).unwrap()));
    term.advance_bytes("\x1b_Ga=d,d=F,i=1\x1b\\");
    assert!(kitty_placement_keys(&term, false).is_empty());
    assert!(term.snapshot_kitty_virtual().is_empty());
    assert_eq!(term.kitty_used_memory(), 0);
}

#[test]
fn kitty_frame_deletion_on_a_nonce_keyed_picture_still_changes_identity() {
    use wezterm_cell::image::ImageDataType;
    // Past the content-hash limit a frame is keyed by a nonce. Dropping the
    // second frame left the first frame's nonce, which was the animation's
    // own identity too, and a remote copy never took the shorter list.
    let mut term = term(640, 384, true);
    let (width, height) = (1024, 257);
    let first = base64_of(&vec![1u8; width * height * 4]);
    let second = base64_of(&vec![2u8; width * height * 4]);
    term.advance_bytes(format!("\x1b_Ga=t,i=1,f=32,s={width},v={height};{first}\x1b\\"));
    term.advance_bytes(format!("\x1b_Ga=f,i=1,f=32,s={width},v={height},z=40;{second}\x1b\\"));
    let old = term.kitty_image_data_for_id(1).unwrap();
    assert!(matches!(&*old.data(), ImageDataType::AnimRgba8 { frames, .. } if frames.len() == 2));
    term.advance_bytes("\x1b_Ga=d,d=f,i=1,r=2\x1b\\");
    let image = term.kitty_image_data_for_id(1).unwrap();
    assert_ne!(old.hash(), image.hash());
    assert!(matches!(&*image.data(), ImageDataType::Rgba8 { data, .. } if data.iter().all(|byte| *byte == 1)));
}

fn kitty_internal_keys(term: &Terminal, alternate: bool) -> std::collections::BTreeSet<(u32, u64)> {
    let mut keys = std::collections::BTreeSet::new();
    term.screen_for_alt(alternate).for_each_phys_line(|_, line| {
        if !line.has_images() { return; }
        for cell in line.visible_cells() {
            for image in cell.attrs().image_attachments() {
                if let Some(id) = image.image_id() { keys.insert((id, image.placement_tag())); }
            }
        }
    });
    keys
}

#[test]
fn anonymous_placements_coexist_and_spatial_deletion_preserves_siblings() {
    for p in ["", ",p=0"] {
        for compressed in [false, true] {
            let mut terminal = term(640, 384, true);
            terminal.advance_bytes(XMIT_2X2);
            for row in [2, 7] {
                terminal.advance_bytes(format!("\x1b[{row};4H\x1b_Ga=p,i=1{p},c=2,r=2,C=1\x1b\\"));
            }
            place_kitty_for_delete(&mut terminal, 1, u32::MAX, 12, 4, 0);
            let before = kitty_internal_keys(&terminal, false);
            assert_eq!(before.len(), 3);
            assert_eq!(before.iter().filter(|(_, tag)| *tag > u64::from(u32::MAX)).count(), 2);
            if compressed { terminal.screen_mut().for_each_phys_line_mut(|_, line| line.compress_for_scrollback()); }
            let retained = terminal.screen_mut().line_mut(1).clone();
            terminal.advance_bytes("\x1b_Ga=d,d=P,x=4,y=2\x1b\\");
            let after = kitty_internal_keys(&terminal, false);
            assert_eq!(after.len(), 2);
            assert!(after.is_subset(&before));
            assert!(retained.has_images());
            assert!(terminal.kitty_image_data_for_id(1).is_some());
            terminal.advance_bytes("\x1b_Ga=d,d=I,i=1,p=0\x1b\\");
            assert!(kitty_internal_keys(&terminal, false).is_empty());
            assert_eq!(terminal.kitty_image_stats(), (0, 0, 0));
        }
    }
}

#[test]
fn named_replacement_preserves_anonymous_siblings_and_invalid_replacement_keeps_pixels() {
    let mut terminal = term(640, 384, true);
    terminal.advance_bytes(XMIT_2X2);
    place_kitty_for_delete(&mut terminal, 1, 0, 2, 4, 0);
    place_kitty_for_delete(&mut terminal, 1, 8, 7, 4, 0);
    let before = kitty_internal_keys(&terminal, false);
    let retained = terminal.screen_mut().line_mut(6).clone();
    terminal.advance_bytes("\x1b[12;4H\x1b_Ga=p,i=1,p=8,x=2,c=2,r=2,C=1\x1b\\");
    assert_eq!(terminal.screen_mut().line_mut(6), &retained);
    assert_eq!(kitty_internal_keys(&terminal, false), before);
    place_kitty_for_delete(&mut terminal, 1, 8, 12, 4, 0);
    assert!(!terminal.screen_mut().line_mut(6).has_images());
    assert!(terminal.screen_mut().line_mut(11).has_images());
    assert!(terminal.screen_mut().line_mut(1).has_images());
    assert_eq!(kitty_internal_keys(&terminal, false), before);
    assert!(retained.has_images());
}

#[test]
fn anonymous_placements_survive_reflow_and_active_screen_deletion() {
    let mut terminal = term(640, 384, true);
    terminal.advance_bytes(XMIT_2X2);
    place_kitty_for_delete(&mut terminal, 1, 0, 1, 75, 0);
    terminal.advance_bytes("\x1b[1;80HX");
    let primary = kitty_internal_keys(&terminal, false);
    terminal.resize(TerminalSize { rows: 24, cols: 20, pixel_width: 160, pixel_height: 384, dpi: 96 });
    assert_eq!(kitty_internal_keys(&terminal, false), primary);
    terminal.advance_bytes("\x1b[?1049h");
    place_kitty_for_delete(&mut terminal, 1, 0, 1, 1, 0);
    assert_eq!(kitty_internal_keys(&terminal, true).len(), 1);
    assert_ne!(kitty_internal_keys(&terminal, true), primary);
    terminal.advance_bytes("\x1b_Ga=d,d=A\x1b\\");
    assert_eq!(kitty_internal_keys(&terminal, false), primary);
    assert!(kitty_internal_keys(&terminal, true).is_empty());
    assert!(terminal.kitty_image_data_for_id(1).is_some());
    terminal.advance_bytes("\x1b_Ga=d,d=I,i=1\x1b\\");
    assert!(kitty_internal_keys(&terminal, false).is_empty());
    assert_eq!(terminal.kitty_image_stats(), (0, 0, 0));
}

#[test]
#[cfg(feature = "use_serde")]
fn anonymous_handoff_restores_tags_before_trimming_and_validates_before_mutation() {
    use serde::{Deserialize, Serialize};
    let mut original = term(640, 384, true);
    original.advance_bytes(XMIT_2X2);
    place_kitty_for_delete(&mut original, 1, 0, 1, 1, 0);
    original.advance_bytes("\x1b[24;1H\n\n\n");
    // Equal z values and overlapping anonymous placements exercise attachment ordering.
    for _ in 0..2 { place_kitty_for_delete(&mut original, 1, 0, 3, 1, 0); }
    original.advance_bytes("\x1b[?1049h");
    place_kitty_for_delete(&mut original, 1, 0, 2, 1, 0);
    let state = original.snapshot_kitty_graphics().placements.unwrap();
    let mut bytes = Vec::new();
    original.snapshot().serialize(&mut varbincode::Serializer::new(&mut bytes)).unwrap();
    let snapshot = || crate::TerminalSnapshot::deserialize(&mut varbincode::Deserializer::new(&mut &bytes[..])).unwrap();
    let mut restored = term(640, 384, true);
    restored.restore_with_kitty_placements(snapshot(), Some(state.clone())).unwrap();
    for alternate in [false, true] {
        assert_eq!(kitty_internal_keys(&restored, alternate), kitty_internal_keys(&original, alternate));
    }
    // A second handoff must carry the tags in the restored attachment order.
    let again = restored.snapshot_kitty_graphics().placements.unwrap();
    let mut second = term(640, 384, true);
    second.restore_with_kitty_placements(restored.snapshot(), Some(again)).unwrap();
    assert_eq!(kitty_internal_keys(&second, false), kitty_internal_keys(&original, false));
    place_kitty_for_delete(&mut second, 1, 0, 7, 1, 0);
    let new_tag = *kitty_internal_keys(&second, true).iter().next_back().unwrap();
    assert!(new_tag.1 > state.next_id);
    second.advance_bytes("\x1b_Ga=d,d=P,x=1,y=2\x1b\\");
    assert_eq!(kitty_internal_keys(&second, true), [new_tag].into());
    for kind in 0..6 {
        let mut invalid = state.clone();
        match kind {
            0 => invalid.cell_tags[0].pop().map(|_| ()).unwrap(),
            1 => invalid.cell_tags[0][0] = u64::MAX,
            2 => invalid.placements.push(invalid.placements[0]),
            3 => invalid.placements[0].1.alt_screen = true,
            4 => invalid.next_id = 0,
            _ => invalid.placements[0].0.image_id = 99,
        }
        let before = second.snapshot();
        assert!(second.restore_with_kitty_placements(snapshot(), Some(invalid)).is_err());
        assert_eq!(second.snapshot(), before);
    }
    #[derive(Debug)]
    struct NoHistory;
    impl TerminalConfiguration for NoHistory {
        fn scrollback_size(&self) -> usize { 0 }
        fn color_palette(&self) -> ColorPalette { ColorPalette::default() }
        fn enable_kitty_graphics(&self) -> bool { true }
    }
    let mut trimmed = Terminal::new(original.get_size(), Arc::new(NoHistory), "ThinkTerm", "test", Box::new(Vec::new()));
    trimmed.restore_with_kitty_placements(snapshot(), Some(state.clone())).unwrap();
    assert_eq!(kitty_internal_keys(&trimmed, false).len(), 2);
    assert_eq!(trimmed.snapshot_kitty_graphics().placements.unwrap().placements.len(), 3);
    assert_eq!(kitty_internal_keys(&trimmed, true), kitty_internal_keys(&original, true));
    trimmed.advance_bytes("\x1b[?1049l\x1b_Ga=d,d=P,x=1,y=3\x1b\\");
    assert!(kitty_internal_keys(&trimmed, false).is_empty());
    assert_eq!(kitty_internal_keys(&trimmed, true).len(), 1);
}

#[test]
fn anonymous_virtual_placement_does_not_replace_an_ordinary_placement() {
    let mut terminal = term(640, 384, true);
    terminal.advance_bytes(XMIT_2X2);
    place_kitty_for_delete(&mut terminal, 1, 0, 1, 1, 0);
    let before = kitty_internal_keys(&terminal, false);
    terminal.advance_bytes("\x1b_Ga=p,i=1,U=1,c=20,r=10\x1b\\");
    assert_eq!(kitty_internal_keys(&terminal, false), before);
    assert_eq!(terminal.snapshot_kitty_virtual()[0].placements.len(), 1);
}

#[test]
#[cfg(feature = "use_serde")]
fn internal_placement_identity_does_not_change_image_cell_wire_bytes_or_shape() {
    use serde::Serialize;
    use std::hash::Hasher;
    use wezterm_cell::image::{ImageCell, ImageData, ImageDataType, TextureCoordinate};
    let cell = ImageCell::new(TextureCoordinate::new_f32(0.0, 0.0), TextureCoordinate::new_f32(1.0, 1.0),
        Arc::new(ImageData::with_data(ImageDataType::new_single_frame(1, 1, vec![0; 4]))));
    let tagged = cell.clone().with_placement_tag(u64::from(u32::MAX) + 1);
    let mut old = Vec::new();
    let mut new = Vec::new();
    cell.serialize(&mut varbincode::Serializer::new(&mut old)).unwrap();
    tagged.serialize(&mut varbincode::Serializer::new(&mut new)).unwrap();
    assert_eq!(old, new);
    let mut first = std::collections::hash_map::DefaultHasher::new();
    let mut second = std::collections::hash_map::DefaultHasher::new();
    cell.compute_shape_hash(&mut first);
    tagged.compute_shape_hash(&mut second);
    assert_eq!(first.finish(), second.finish());
    // Storage equality still distinguishes them so scrollback compression
    // cannot merge adjacent, visually identical anonymous placements.
    assert_ne!(cell, tagged);
}

fn relative_views(terminal: &mut Terminal, id: u32) -> Vec<crate::kitty_relative::RelativeView> {
    terminal.kitty_frame_selections(None).unwrap().1.into_iter()
        .find(|s| s.image_id == id).map(|s| s.relative_placements).unwrap_or_default()
}

fn relative_images(terminal: &mut Terminal) {
    for id in 1..=3 { terminal.advance_bytes(XMIT_2X2.replace("i=1", &format!("i={id}"))); }
}

#[test]
fn relative_metadata_tracks_position_updates_without_resending_for_unrelated_text() {
    let mut terminal = term(640, 384, true);
    relative_images(&mut terminal);
    place_kitty_for_delete(&mut terminal, 1, 1, 2, 4, 0);
    terminal.advance_bytes("\x1b_Ga=p,i=2,p=5,P=1,Q=1,H=3,V=2,c=2,r=2\x1b\\");
    let initial = relative_views(&mut terminal, 2)[0];
    assert_eq!(initial.source_seqno, terminal.current_seqno() as u64);
    let revision = terminal.kitty_frame_selections(None).unwrap().0;
    terminal.advance_bytes("\x1b[20;1Hunrelated text");
    assert_eq!(relative_views(&mut terminal, 2)[0], initial);
    assert!(terminal.kitty_frame_selections(Some(revision)).is_none());
    place_kitty_for_delete(&mut terminal, 1, 1, 3, 4, 0);
    let moved = relative_views(&mut terminal, 2)[0];
    assert_eq!(moved.source_seqno, terminal.current_seqno() as u64);
    assert!(moved.source_seqno > initial.source_seqno);
    assert_eq!(moved.anchor.row, initial.anchor.row + 1);
}

#[test]
fn relative_placements_follow_named_parents_and_chains_without_moving_the_cursor() {
    let mut terminal = term(640, 384, true);
    relative_images(&mut terminal);
    place_kitty_for_delete(&mut terminal, 1, 1, 2, 4, 0);
    terminal.advance_bytes("\x1b[15;20H");
    let cursor = terminal.cursor_pos();
    terminal.advance_bytes("\x1b_Ga=p,i=2,p=5,P=1,Q=1,H=3,V=-1,c=4,r=2,C=0\x1b\\");
    assert_eq!((terminal.cursor_pos().x, terminal.cursor_pos().y), (cursor.x, cursor.y));
    assert_eq!(relative_views(&mut terminal, 2)[0].anchor, crate::kitty_relative::Anchor { column: 6, row: 0, alt_screen: false });
    assert!(kitty_internal_keys(&terminal, false).iter().all(|(id, _)| *id == 1));
    terminal.advance_bytes("\x1b_Ga=p,i=3,p=6,P=2,Q=5,H=-2,V=5,c=1,r=1\x1b\\");
    assert_eq!(relative_views(&mut terminal, 3)[0].anchor, crate::kitty_relative::Anchor { column: 4, row: 5, alt_screen: false });
    place_kitty_for_delete(&mut terminal, 1, 1, 8, 10, 0);
    assert_eq!(relative_views(&mut terminal, 3)[0].anchor, crate::kitty_relative::Anchor { column: 10, row: 11, alt_screen: false });
    // A cycle must retain the ordinary root and both relative placements.
    terminal.advance_bytes("\x1b_Ga=p,i=1,p=1,P=3,Q=6,c=2,r=2\x1b\\");
    assert_eq!(kitty_internal_keys(&terminal, false), [(1, 1)].into());
    assert_eq!(relative_views(&mut terminal, 3)[0].anchor.column, 10);
    terminal.advance_bytes("\x1b_Ga=d,d=i,i=1,p=1\x1b\\");
    assert!(relative_views(&mut terminal, 2).is_empty());
    assert!(terminal.kitty_image_data_for_id(2).is_none());
    assert!(terminal.kitty_image_data_for_id(3).is_none());
    assert!(terminal.kitty_image_data_for_id(1).is_some());
}

#[test]
fn relative_parent_selection_is_stable_for_anonymous_siblings() {
    let mut terminal = term(640, 384, true);
    relative_images(&mut terminal);
    place_kitty_for_delete(&mut terminal, 1, 0, 2, 4, 0);
    place_kitty_for_delete(&mut terminal, 1, 0, 7, 4, 0);
    terminal.advance_bytes("\x1b_Ga=p,i=2,P=1,H=2,V=1,c=2,r=2\x1b\\");
    let view = relative_views(&mut terminal, 2);
    assert_eq!(view[0].anchor.row, 2);
    terminal.advance_bytes("\x1b_Ga=d,d=p,x=4,y=7\x1b\\");
    assert_eq!(relative_views(&mut terminal, 2), view);
    terminal.advance_bytes("\x1b_Ga=d,d=p,x=4,y=2\x1b\\");
    assert!(terminal.kitty_image_data_for_id(2).is_none());
}

#[test]
fn virtual_parent_can_hide_and_reappear_without_deleting_relative_children() {
    let mut terminal = term(640, 384, true);
    relative_images(&mut terminal);
    terminal.advance_bytes("\x1b_Ga=p,i=1,p=5,U=1,c=1,r=1\x1b\\");
    terminal.advance_bytes("\x1b_Ga=p,i=2,p=7,P=1,Q=5,H=-1,V=2,c=2,r=2\x1b\\");
    assert!(relative_views(&mut terminal, 2).is_empty());
    assert!(terminal.kitty_image_data_for_id(2).is_some());
    terminal.advance_bytes("\x1b[4;10H\x1b[38;2;0;0;1;58;2;0;0;5m\u{10eeee}\x1b[0m");
    assert_eq!(relative_views(&mut terminal, 2)[0].anchor, crate::kitty_relative::Anchor { column: 8, row: 5, alt_screen: false });
    terminal.advance_bytes("\x1b[4;1H\x1b[2K");
    assert!(relative_views(&mut terminal, 2).is_empty());
    assert!(terminal.kitty_image_data_for_id(2).is_some());
    terminal.advance_bytes("\x1b[2;8H\x1b[38;2;0;0;1;58;2;0;0;5m\u{10eeee}\x1b[0m");
    assert_eq!(relative_views(&mut terminal, 2)[0].anchor.column, 6);
    terminal.advance_bytes("\x1b_Ga=d,d=i,i=1,p=5\x1b\\");
    assert!(terminal.kitty_image_data_for_id(2).is_none());
}

#[test]
fn relative_deletion_selects_the_current_rectangle_and_releases_descendants() {
    for selector in ["p,x=7,y=4", "q,x=7,y=4,z=-2", "x,x=7", "y,y=4", "z,z=-2"] {
        let mut terminal = term(640, 384, true);
        relative_images(&mut terminal);
        place_kitty_for_delete(&mut terminal, 1, 1, 2, 4, 0);
        terminal.advance_bytes("\x1b_Ga=p,i=2,p=5,P=1,Q=1,H=3,V=2,c=4,r=2,z=-2\x1b\\");
        terminal.advance_bytes("\x1b_Ga=p,i=3,p=6,P=2,Q=5,V=4,c=2,r=2\x1b\\");
        terminal.advance_bytes(format!("\x1b_Ga=d,d={selector}\x1b\\"));
        assert!(terminal.kitty_image_data_for_id(2).is_none());
        assert!(terminal.kitty_image_data_for_id(3).is_none());
        assert_eq!(kitty_internal_keys(&terminal, false), [(1, 1)].into());
    }
}

#[test]
fn relative_roots_follow_reflow_and_can_stay_outside_the_viewport() {
    let mut terminal = term(640, 384, true);
    relative_images(&mut terminal);
    place_kitty_for_delete(&mut terminal, 1, 1, 1, 75, 0);
    terminal.advance_bytes("\x1b[1;80HX\x1b_Ga=p,i=2,p=5,P=1,Q=1,H=-1,V=4,c=2,r=2\x1b\\");
    terminal.resize(TerminalSize { rows: 24, cols: 20, pixel_width: 160, pixel_height: 384, dpi: 96 });
    let view = relative_views(&mut terminal, 2)[0];
    assert_eq!(view.anchor.column, 13);
    assert_eq!(view.anchor.row, 7);
    terminal.advance_bytes("\x1b[24;1H\n\n\n\n\n");
    assert_eq!(relative_views(&mut terminal, 2)[0], view);
    terminal.advance_bytes("\x1b[?1049h");
    assert!(!relative_views(&mut terminal, 2)[0].anchor.alt_screen);
    terminal.advance_bytes("\x1b[?1049l\x1b[2J\x1b[3J");
    assert!(terminal.kitty_image_data_for_id(2).is_none());
}

#[test]
fn relative_errors_keep_existing_placements_and_replies_identify_the_placement() {
    let (mut terminal, tap) = term_with_tap(640, 384);
    relative_images(&mut terminal);
    place_kitty_for_delete(&mut terminal, 1, 1, 2, 4, 0);
    terminal.advance_bytes("\x1b_Ga=p,i=2,p=5,P=1,Q=1,c=2,r=2\x1b\\");
    let view = relative_views(&mut terminal, 2);
    drain(&mut terminal, &tap);
    for (keys, code) in [("P=99", "ENOPARENT"), ("P=2,Q=5", "ECYCLE"), ("P=1,Q=1,U=1", "EINVAL")] {
        terminal.advance_bytes(format!("\x1b_Ga=p,i=2,p=5,{keys},c=2,r=2\x1b\\"));
        let reply = drain(&mut terminal, &tap);
        assert!(reply.contains(&format!("i=2,p=5;{code}:")), "unexpected reply: {:?}", reply);
        assert_eq!(relative_views(&mut terminal, 2), view);
    }
    terminal.advance_bytes("\x1b_Ga=p,i=1,I=9,p=1\x1b\\");
    assert!(drain(&mut terminal, &tap).contains("EINVAL:"));
}

#[test]
#[cfg(feature = "use_serde")]
fn relative_graph_and_cell_origins_survive_repeated_restore_and_image_retransmission() {
    let mut original = term(640, 384, true);
    relative_images(&mut original);
    place_kitty_for_delete(&mut original, 1, 0, 2, 4, 0);
    original.advance_bytes("\x1b_Ga=p,i=2,P=1,H=3,V=2,c=2,r=2\x1b\\");
    let expected = relative_views(&mut original, 2);
    for _ in 0..2 {
        let state = original.snapshot_kitty_graphics();
        let mut next = term(640, 384, true);
        next.restore_with_kitty_placements(original.snapshot(), state.placements).unwrap();
        next.restore_kitty_relatives(state.relatives.unwrap()).unwrap();
        assert_eq!(relative_views(&mut next, 2), expected);
        original = next;
    }
    original.advance_bytes(XMIT_2X2);
    assert!(kitty_internal_keys(&original, false).is_empty());
    assert!(relative_views(&mut original, 2).is_empty());
    assert!(original.kitty_image_data_for_id(2).is_none());
}
