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
    /// `needle` to show up instead of guessing at a delay: replies are written
    /// in order, so once it appears everything sent before it is here too. On
    /// deadline the buffer is returned as-is so the caller's assertion can
    /// print it.
    fn read_until(&self, needle: &str) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let buf = String::from_utf8_lossy(&self.0.lock().unwrap()).to_string();
            if buf.contains(needle) || std::time::Instant::now() >= deadline {
                return buf;
            }
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
    term.advance_bytes(format!(
        "\x1b_Ga=q,i={SENTINEL},f=32,s=1,v=1;AAAAAA==\x1b\\"
    ));
    tap.read_until(&format!("i={SENTINEL}"))
        .replace(&format!("\x1b_Gi={SENTINEL};OK\x1b\\"), "")
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
    term.advance_bytes("\x1b_Ga=p,i=1\x1b\\");
    assert!(
        term.screen_mut()
            .line_mut(0)
            .get_cell(0)
            .and_then(|c| c.attrs().images())
            .is_some(),
        "the real placement should paint first"
    );

    let (before_x, before_y) = (term.cursor_pos().x, term.cursor_pos().y);
    term.advance_bytes("\x1b_Ga=p,i=1,U=1,c=20,r=10\x1b\\");
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
fn base64_of(bytes: &[u8]) -> String {
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
