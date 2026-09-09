//! The arithmetic behind drawing a glyph with the browser's own fonts:
//! where the ink is, whether it carries its own colour, where it sits
//! relative to the baseline, how much of it one frame may do, and what an
//! atlas that cannot grow any further does next.
//!
//! All of it is pure, and none of it is `wasm32`-only, because `emit.rs`
//! and `glyphs.rs` are -- and this crate has no wasm test runner, so an
//! assertion written in either of those files never runs. `braille.rs` was
//! split the same way and for the same reason.

/// The tightest box around the non-transparent pixels of a drawing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ink {
    pub left: usize,
    pub top: usize,
    pub width: usize,
    pub height: usize,
    /// The ink reaches an edge of the buffer, so the glyph may have been
    /// cut off rather than merely being large.
    pub clipped: bool,
}

/// The ink in an RGBA buffer, or `None` when the browser drew nothing.
///
/// Alpha only: the fill colour is ours, and a glyph that carries its own
/// colour still has an alpha channel.
pub fn ink_box(rgba: &[u8], width: usize, height: usize) -> Option<Ink> {
    let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0usize, 0usize);
    for y in 0..height {
        let row = &rgba[y * width * 4..(y + 1) * width * 4];
        for x in 0..width {
            if row[x * 4 + 3] != 0 {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }
    (x0 != usize::MAX).then(|| Ink {
        left: x0,
        top: y0,
        width: x1 - x0 + 1,
        height: y1 - y0 + 1,
        clipped: x0 == 0 || y0 == 0 || x1 + 1 == width || y1 + 1 == height,
    })
}

/// Whether a glyph brought its own colour, from the same ink drawn twice in
/// two different fills.
///
/// Identical pixels mean the browser ignored the colour we asked for, which
/// only a glyph carrying its own does. The two fills must differ in **every**
/// channel: white and red share a red channel, and comparing that one
/// channel is what silently called five hundred Chinese characters coloured
/// in the first round of measurements.
///
/// Both buffers are the ink and nothing else -- outside it the drawings are
/// transparent and would agree whatever the glyph is.
pub fn carries_own_colour(first: &[u8], second: &[u8]) -> bool {
    first == second
}

/// The ink cut out of the buffer as its own tightly packed RGBA8.
///
/// A row at a time and nothing per pixel. The bytes are copied exactly as
/// `getImageData` gave them, **not** premultiplied: the pipeline blends
/// with `ALPHA_BLENDING` and the shader does not undo a premultiply, so
/// straight alpha is what it wants.
pub fn crop(rgba: &[u8], stride: usize, ink: &Ink) -> Vec<u8> {
    let mut out = Vec::with_capacity(ink.width * ink.height * 4);
    for row in 0..ink.height {
        let at = ((ink.top + row) * stride + ink.left) * 4;
        out.extend_from_slice(&rgba[at..at + ink.width * 4]);
    }
    out
}

/// Where a drawn glyph goes, in the terms `CachedGlyph` uses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub bearing_x: f64,
    pub bearing_y: f64,
    pub scale: f64,
}

/// Turn an ink box into a bearing, given where we put the pen and the
/// baseline.
///
/// `bearing_y` is read off the emitter's own arithmetic rather than
/// guessed. `emit.rs` puts a sprite's top edge at
///
/// ```text
/// cell_height + descender - (y_offset + bearing_y)
/// ```
///
/// and `cell_height + descender` *is* the baseline, so `bearing_y` is the
/// distance from the baseline up to the top of the ink. It goes negative
/// for a glyph that sits entirely below the baseline, such as a comma.
///
/// This is not `braille.rs`'s `cell_height + descender`: that is this same
/// expression for the one case where the sprite is the whole cell, so the
/// ink starts at the top of the row.
///
/// A glyph whose ink is wider than the columns it was given is scaled about
/// the baseline, which is the arithmetic `load_glyph` already uses.
pub fn place(ink: &Ink, pen_x: f64, baseline: f64, max_width: f64) -> Placement {
    let scale = if ink.width as f64 > max_width && max_width > 0.0 {
        max_width / ink.width as f64
    } else {
        1.0
    };
    // Never small enough to round the height away. `Image::scale_by`
    // truncates without a floor, so a one-pixel-tall rule -- U+203E, U+2015,
    // U+FF3F, which a system font draws full width -- becomes a zero-height
    // sprite: no panic, no error, an invisible glyph cached as a good one,
    // in a cell whose missing-glyph box has already been dropped.
    // Overflowing is the lesser evil, and is already allowed for wide ink.
    //
    // Only the height needs it. `max_width` is at least one and a quarter
    // cells, so the width can never be scaled below a pixel.
    let scale = scale.max(1.0 / ink.height.max(1) as f64).min(1.0);
    Placement {
        bearing_x: (ink.left as f64 - pen_x) * scale,
        bearing_y: (baseline - ink.top as f64) * scale,
        scale,
    }
}

/// Graphemes the caller draws itself, and would rather have the
/// missing-glyph box for than a substitute.
///
/// **Braille**, because `braille.rs` draws it: no bundled face covers a
/// single one of U+2800..=U+28FF, so every Braille character is a gap, and
/// routing them to a canvas would quietly turn that module into dead code
/// and take btop's graphs from a pixel-exact dot matrix to something slower
/// and off the grid.
/// Other terminal graphics (blocks, shades, borders and the desktop's
/// custom symbols) also bypass fonts and use the shared cell geometry.
///
/// **Right-to-left scripts**, because the emitter cannot lay them out. It
/// does no bidi and the shaper is hard-coded left to right, so a run of
/// them would come out as correct-looking glyphs in the wrong order --
/// harder for a reader to catch than a row of boxes, which at least says
/// plainly that this is not rendered. Joining scripts have a second reason
/// on top: joining crosses grapheme boundaries, so drawing one grapheme at
/// a time yields isolated forms.
///
/// The test is by script rather than by joining class deliberately. Hebrew
/// does not join at all, and would still be reversed.
pub fn keeps_notdef(text: &str) -> bool {
    let mut chars = text.chars();
    let (first, rest) = (chars.next(), chars.next());
    if rest.is_none()
        && first.and_then(thinkterm_render::customglyph::BlockKey::from_char).is_some()
    {
        return true;
    }
    text.chars().any(is_right_to_left)
}

/// The right-to-left blocks, contiguous so nothing between two of them is
/// left out: an earlier version of this list jumped from Mandaic straight
/// to Arabic Extended-A and silently routed Syriac Supplement and Arabic
/// Extended-B, both of which join.
fn is_right_to_left(c: char) -> bool {
    matches!(c as u32,
        // Hebrew, Arabic, Syriac, Thaana, N'Ko, Samaritan, Mandaic, and the
        // Syriac and Arabic extensions that sit between them.
        0x0590..=0x08ff
        // Hebrew and Arabic presentation forms.
        | 0xfb1d..=0xfdff | 0xfe70..=0xfeff
        // Mongolian, which is vertical as well as joining.
        | 0x1800..=0x18af
        // The historic right-to-left scripts of the supplementary plane --
        // Cypriot through Chorasmian -- and Mende Kikakui.
        | 0x10800..=0x10fff | 0x1e800..=0x1e8df
        // Adlam, and the Arabic mathematical alphabets.
        | 0x1e900..=0x1e95f | 0x1ee00..=0x1eeff)
}

/// How long a frame may spend drawing glyphs it has never drawn before.
///
/// The budget a frame actually runs on. A count bounds work, not time, and
/// the difference was measurable: at 512 glyphs a screen of fresh CJK
/// hitched for 124 ms, and a CPU profile put 89 ms of that in `fillText`
/// and `getImageData` against 0.6 ms in this crate's own code. Eight
/// milliseconds leaves the rest of a 60 Hz frame to the rest of the frame.
///
/// **Built in `frame`, shared by both `paint` attempts**: the retry loop
/// calls `paint` more than once, and a fresh deadline per attempt would let
/// one browser callback take several times this long -- which is the thing
/// the budget exists to prevent.
pub const NEW_GLYPH_MS_PER_FRAME: f64 = 8.0;

/// A backstop, not the budget.
///
/// The deadline above is what stops a frame. This catches a clock that does
/// not advance -- `performance.now()` is deliberately coarsened, and some
/// privacy settings pin it -- which would otherwise leave the deadline
/// permanently in the future and the cap gone altogether.
pub const NEW_GLYPHS_PER_FRAME: u32 = 512;

#[derive(Debug)]
pub struct FallbackBudget {
    remaining: u32,
    deferred: u32,
    deadline: f64,
    drawn: u32,
}

impl FallbackBudget {
    /// `now` here and the `now` passed to `take` must be the same clock.
    pub fn new(now: f64) -> Self {
        Self {
            remaining: NEW_GLYPHS_PER_FRAME,
            deferred: 0,
            deadline: now + NEW_GLYPH_MS_PER_FRAME,
            drawn: 0,
        }
    }

    /// Whether this frame may draw one more. A refusal is counted, and the
    /// caller leaves the missing-glyph box in place and asks again next
    /// frame; nothing is written down, so nothing has to be invalidated.
    pub fn take(&mut self, now: f64) -> bool {
        // The first is always allowed. A device slow enough to spend the
        // whole budget on a single glyph would otherwise refuse every glyph
        // of every frame, and the screen would stay boxes for ever.
        if self.remaining == 0 || (self.drawn > 0 && now >= self.deadline) {
            self.deferred += 1;
            return false;
        }
        self.remaining -= 1;
        self.drawn += 1;
        true
    }

    /// The atlas was rebuilt, so everything this frame drew went with it.
    ///
    /// Gives back the count and what the frame owes, but **not the time**:
    /// the deadline belongs to the frame, not to the attempt, and moving it
    /// would let one browser callback run a second full budget -- which is
    /// the thing the budget is for.
    pub fn refund(&mut self) {
        self.remaining = NEW_GLYPHS_PER_FRAME;
        self.deferred = 0;
        self.drawn = 0;
    }

    /// Glyphs this frame put off until the next one.
    pub fn deferred(&self) -> u32 {
        self.deferred
    }
}

/// The shortest and longest a screen waits for the atlas to be tried again.
///
/// The cap is the recovery bound: once the content will fit, it is on
/// screen within this. Not longer, because "the screen got simple and is
/// still blank" becomes something a person reports; not shorter, because a
/// clear rewrites the whole texture and a screen that genuinely never fits
/// would pay that over and over.
pub const MIN_RETRY_MS: f64 = 1000.0;
pub const MAX_RETRY_MS: f64 = 8000.0;

/// What the next frame does about an atlas that cannot grow any further.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Next {
    /// Everything the frame wanted, it got. Stop declining.
    Recovered,
    /// Clear the atlas and stop declining.
    Clear,
    /// Keep declining, and wake up at this time (same clock as `now`) to
    /// try again.
    RetryAt(f64),
}

/// An atlas that has grown as far as the GPU allows and still cannot fit
/// what is on screen.
///
/// There is no per-sprite eviction, so the only thing that frees space is
/// clearing the whole texture, and whether that will help is not knowable:
/// the content changes, and which glyphs win the race for space afterwards
/// depends on allocation order, so even "the set we had to decline" is not
/// a stable fingerprint of demand. So this does not try to know. It retries
/// on a backoff -- at once, then after 1 s, 2 s, 4 s, capped at 8 s.
///
/// The clock, not a frame count: once the terminal stops producing output
/// nothing asks for another frame, so a frame-counted deadline on a still
/// screen would never arrive -- and repainting a still screen thousands of
/// times to reach one would be pure waste. The caller schedules a timer.
#[derive(Debug)]
pub struct Capacity {
    frozen: bool,
    next_retry: f64,
    backoff: f64,
}

impl Default for Capacity {
    fn default() -> Self {
        Self::new()
    }
}

impl Capacity {
    pub fn new() -> Self {
        Self {
            frozen: false,
            next_retry: 0.0,
            backoff: 0.0,
        }
    }

    /// Whether this frame must be drawn without allocating anything.
    pub fn frozen(&self) -> bool {
        self.frozen
    }

    /// A frame reached the GPU owing nothing: no glyph put off by the
    /// budget, none declined by a full atlas.
    ///
    /// Both counts have to be zero. A frame whose budget ran out returns
    /// successfully too, and taking that for "the demand is satisfied"
    /// gives "clear, refill part of it, hit the ceiling again".
    ///
    /// **This does not touch the backoff.** The backoff is a limit on how
    /// often the whole texture may be rewritten, and it has to hold across
    /// a recovery, because "the screen fits for one frame and then fills up
    /// again" is the ordinary shape of scrolling through CJK. Resetting it
    /// here made a scroll clear the atlas every few frames -- 48 ms apart
    /// in simulation, which is the storm this whole mechanism exists to
    /// prevent. A terminal that stays healthy is not held back by it: the
    /// deadline is an absolute time, so by the time trouble comes round
    /// again it has long passed and the next clear happens at once.
    pub fn drew_everything(&mut self) {
        self.frozen = false;
    }

    /// The atlas can neither grow nor take another sprite.
    pub fn at_capacity(&mut self, now: f64) {
        self.frozen = true;
        // Never cleared: the atlas is most likely full of glyphs that
        // scrolled off the screen long ago, so try at once. Otherwise the
        // deadline the last clear set stands.
        if self.backoff == 0.0 {
            self.next_retry = now;
        }
    }

    /// Forget the history. For a `GlyphCache` rebuilt for a reason that has
    /// nothing to do with capacity -- a font size or device pixel ratio
    /// change -- where what fits is a different question entirely.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// A frame was submitted while declining allocations, having put off
    /// `deferred` glyphs and declined `declined` sprites.
    pub fn submitted_frozen(&mut self, now: f64, deferred: u32, declined: u32) -> Next {
        if deferred == 0 && declined == 0 {
            self.drew_everything();
            return Next::Recovered;
        }
        if now < self.next_retry {
            return Next::RetryAt(self.next_retry);
        }
        self.backoff = (self.backoff * 2.0).clamp(MIN_RETRY_MS, MAX_RETRY_MS);
        self.next_retry = now + self.backoff;
        self.frozen = false;
        Next::Clear
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `w`x`h` RGBA buffer with an opaque rectangle in it.
    fn drawn(w: usize, h: usize, rect: (usize, usize, usize, usize), rgb: [u8; 3]) -> Vec<u8> {
        let mut buf = vec![0u8; w * h * 4];
        let (x, y, rw, rh) = rect;
        for row in y..y + rh {
            for col in x..x + rw {
                let at = (row * w + col) * 4;
                buf[at..at + 3].copy_from_slice(&rgb);
                buf[at + 3] = 0xff;
            }
        }
        buf
    }

    #[test]
    fn a_blank_drawing_has_no_ink() {
        assert_eq!(ink_box(&vec![0u8; 8 * 8 * 4], 8, 8), None);
    }

    #[test]
    fn the_ink_box_is_the_tightest_one() {
        let buf = drawn(10, 10, (2, 3, 4, 5), [0xff, 0, 0]);
        let ink = ink_box(&buf, 10, 10).expect("ink");
        assert_eq!((ink.left, ink.top, ink.width, ink.height), (2, 3, 4, 5));
        assert!(!ink.clipped);
    }

    #[test]
    fn ink_that_reaches_an_edge_is_marked_clipped() {
        for rect in [(0, 3, 4, 4), (3, 0, 4, 4), (6, 3, 4, 4), (3, 6, 4, 4)] {
            assert!(ink_box(&drawn(10, 10, rect, [1, 2, 3]), 10, 10).unwrap().clipped, "{rect:?}");
        }
    }

    #[test]
    fn two_fills_that_agree_mean_the_glyph_brought_its_own_colour() {
        let ink = Ink { left: 2, top: 2, width: 4, height: 4, clipped: false };
        // A glyph that ignored us: the same pixels both times.
        let own = crop(&drawn(10, 10, (2, 2, 4, 4), [0x33, 0x66, 0x99]), 10, &ink);
        assert!(carries_own_colour(&own, &own.clone()));
        // A mask: it took each fill we asked for, so the two drawings
        // differ and it is not a colour glyph.
        let red = crop(&drawn(10, 10, (2, 2, 4, 4), [0xff, 0x00, 0x00]), 10, &ink);
        let cyan = crop(&drawn(10, 10, (2, 2, 4, 4), [0x00, 0xff, 0xff]), 10, &ink);
        assert!(!carries_own_colour(&red, &cyan));
        // The trap this pair of fills exists to avoid: white and red are
        // equal on the red channel, so the first round of measurements --
        // which compared that channel alone -- called every Chinese
        // character a colour glyph. Comparing all four bytes tells them
        // apart, and the fills we use differ in every channel anyway.
        let white = crop(&drawn(10, 10, (2, 2, 4, 4), [0xff, 0xff, 0xff]), 10, &ink);
        assert_eq!(white[0], red[0], "premise: they agree on the red channel");
        assert!(!carries_own_colour(&white, &red));
    }

    #[test]
    fn only_the_ink_is_compared() {
        // What lies outside the ink box never reaches the comparison, so a
        // stray pixel from an earlier draw cannot decide the question.
        let ink = Ink { left: 2, top: 2, width: 2, height: 2, clipped: false };
        let a = drawn(8, 8, (2, 2, 2, 2), [9, 9, 9]);
        let mut b = a.clone();
        let at = (6 * 8 + 6) * 4;
        b[at..at + 4].copy_from_slice(&[1, 2, 3, 4]);
        assert!(carries_own_colour(&crop(&a, 8, &ink), &crop(&b, 8, &ink)));
    }

    #[test]
    fn cropping_copies_the_ink_byte_for_byte() {
        let buf = drawn(10, 10, (3, 1, 2, 3), [0x12, 0x34, 0x56]);
        let ink = ink_box(&buf, 10, 10).unwrap();
        let out = crop(&buf, 10, &ink);
        assert_eq!(out.len(), ink.width * ink.height * 4);
        assert!(out.chunks(4).all(|px| px == [0x12, 0x34, 0x56, 0xff]));
    }

    #[test]
    fn the_bearing_is_the_one_the_emitter_subtracts() {
        // The derivation, not a restatement of it: build ink whose top edge
        // is exactly the top of the row, and the bearing must come out as
        // the constant `braille_glyph` hard-codes for a full-cell sprite.
        // Get a sign or a term order wrong and this goes red.
        let (cell_h, descender, row_top) = (32.0, -6.0, 10.0);
        let baseline = row_top + cell_h + descender;
        let ink = Ink { left: 0, top: row_top as usize, width: 10, height: 20, clipped: false };
        let p = place(&ink, 0.0, baseline, 100.0);
        assert_eq!(p.bearing_y, cell_h + descender);
        assert_eq!(p.scale, 1.0);
    }

    #[test]
    fn a_glyph_below_the_baseline_has_a_negative_bearing() {
        let ink = Ink { left: 0, top: 30, width: 4, height: 4, clipped: false };
        assert!(place(&ink, 0.0, 20.0, 100.0).bearing_y < 0.0);
    }

    #[test]
    fn ink_wider_than_its_columns_is_scaled_about_the_baseline() {
        let ink = Ink { left: 4, top: 10, width: 30, height: 10, clipped: false };
        let plain = place(&ink, 2.0, 40.0, 100.0);
        let squeezed = place(&ink, 2.0, 40.0, 10.0);
        assert_eq!(squeezed.scale, 10.0 / 30.0);
        assert_eq!(squeezed.bearing_x, plain.bearing_x * squeezed.scale);
        assert_eq!(squeezed.bearing_y, plain.bearing_y * squeezed.scale);
    }

    #[test]
    fn what_the_caller_draws_itself_keeps_its_box() {
        assert!(keeps_notdef("\u{2801}"), "braille is drawn by braille.rs");
        assert!(keeps_notdef("\u{28ff}"));
        for c in ["█", "▀", "▄", "░", "▒", "▓", "─", "│", "┌", "\u{1fb00}"] {
            assert!(keeps_notdef(c), "{c} must use cell geometry, not Canvas");
        }
        assert!(!keeps_notdef("█\u{301}"), "a combining sequence is not a single block");
        assert!(keeps_notdef("\u{0628}"), "Arabic joins across graphemes");
        assert!(!keeps_notdef("\u{4e2d}"));
        assert!(!keeps_notdef("\u{d55c}"));
        assert!(!keeps_notdef("\u{1f600}"));
        // Two Braille characters are two graphemes; one grapheme holding
        // two of them is not something `braille.rs` can draw.
        assert!(!keeps_notdef("\u{2801}\u{2802}"));
    }

    #[test]
    fn the_budget_defers_rather_than_dropping() {
        // A clock that never moves, so this is the count backstop alone.
        let mut budget = FallbackBudget::new(0.0);
        for _ in 0..NEW_GLYPHS_PER_FRAME {
            assert!(budget.take(0.0));
        }
        assert_eq!(budget.deferred(), 0);
        for n in 1..=5 {
            assert!(!budget.take(0.0));
            assert_eq!(budget.deferred(), n);
        }
    }

    #[test]
    fn a_frame_stops_at_its_deadline_long_before_the_count() {
        // A tenth of a millisecond a glyph: what a headless software
        // backend measured. The count alone would have allowed all 512 --
        // 51 ms in one frame, which is the hitch this replaced.
        let mut budget = FallbackBudget::new(1000.0);
        let (mut now, mut drawn) = (1000.0, 0u32);
        while budget.take(now) {
            drawn += 1;
            now += 0.1;
            assert!(drawn <= NEW_GLYPHS_PER_FRAME, "the deadline never fired");
        }
        assert_eq!(drawn, (NEW_GLYPH_MS_PER_FRAME / 0.1) as u32);
        assert!(drawn < NEW_GLYPHS_PER_FRAME);
    }

    #[test]
    fn the_first_glyph_is_drawn_even_when_the_frame_is_already_late() {
        // Pins the deadlock: a device that spends the whole budget on one
        // glyph must still make progress. Refusing here defers every glyph
        // of every frame, and the screen stays boxes for good.
        let mut budget = FallbackBudget::new(0.0);
        assert!(budget.take(1_000_000.0));
        assert!(!budget.take(1_000_000.0));
        assert_eq!(budget.deferred(), 1);
    }

    #[test]
    fn a_refund_gives_back_the_count_but_not_the_deadline() {
        let mut budget = FallbackBudget::new(0.0);
        assert!(budget.take(0.0));
        assert!(!budget.take(NEW_GLYPH_MS_PER_FRAME));
        assert_eq!(budget.deferred(), 1);
        budget.refund();
        assert_eq!(budget.deferred(), 0);
        // One more, because the rebuilt atlas threw away what was drawn,
        // and then the deadline stops it again. A refund that moved the
        // deadline would let this one frame run a second full budget.
        assert!(budget.take(NEW_GLYPH_MS_PER_FRAME));
        assert!(!budget.take(NEW_GLYPH_MS_PER_FRAME));
    }

    // --- the atlas recovery state machine ---------------------------------
    //
    // Every one of these pins a way an earlier version of this logic locked
    // the terminal up or thrashed it.

    #[test]
    fn the_first_time_at_capacity_clears_at_once() {
        let mut cap = Capacity::new();
        cap.at_capacity(1000.0);
        assert!(cap.frozen());
        assert_eq!(cap.submitted_frozen(1000.0, 0, 7), Next::Clear);
        assert!(!cap.frozen(), "a clear is followed by a normal frame");
    }

    #[test]
    fn a_screen_that_never_fits_backs_off_and_caps() {
        let mut cap = Capacity::new();
        let mut now = 0.0f64;
        let mut clears = vec![];
        // One frame every 16 ms, forever declining something.
        for _ in 0..4000 {
            if !cap.frozen() {
                cap.at_capacity(now);
            }
            if let Next::Clear = cap.submitted_frozen(now, 0, 3) {
                clears.push(now);
            }
            now += 16.0;
        }
        let gaps: Vec<f64> = clears.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(clears.len() > 4, "it has to keep trying: {clears:?}");
        assert!(
            gaps.iter().all(|g| *g >= MIN_RETRY_MS),
            "never once per frame: {gaps:?}"
        );
        assert!(
            gaps.iter().all(|g| *g <= MAX_RETRY_MS + 16.0),
            "and never longer than the cap: {gaps:?}"
        );
        // Doubling, then flat at the cap.
        assert!(gaps[1] > gaps[0] && gaps[2] > gaps[1], "{gaps:?}");
        assert_eq!(gaps.last(), gaps.iter().max_by(|a, b| a.total_cmp(b)));
    }

    #[test]
    fn a_frozen_frame_that_owes_nothing_ends_the_emergency() {
        let mut cap = Capacity::new();
        cap.at_capacity(0.0);
        assert_eq!(cap.submitted_frozen(0.0, 0, 0), Next::Recovered);
        assert!(!cap.frozen());
    }

    #[test]
    fn a_new_glyph_after_the_screen_got_simple_still_gets_drawn() {
        // The self-lock. The screen is cleared down to one character that
        // was never cached, so the frozen frame declines it and keeps
        // declining it. "Unfreeze when nothing was declined" never fires
        // here and the character is never drawn.
        let mut cap = Capacity::new();
        cap.at_capacity(0.0);
        assert_eq!(cap.submitted_frozen(0.0, 0, 9), Next::Clear);
        // ... and the clear did not help, so we are frozen again, at the
        // cap, with exactly one thing outstanding for ever after.
        let mut now = 16.0;
        let mut cleared = None;
        for _ in 0..2000 {
            if !cap.frozen() {
                cap.at_capacity(now);
            }
            if let Next::Clear = cap.submitted_frozen(now, 0, 1) {
                cleared = Some(now);
                break;
            }
            now += 16.0;
        }
        let at = cleared.expect("it must try again rather than stay frozen for ever");
        assert!(at <= MAX_RETRY_MS + 16.0, "recovery took {at} ms");
    }

    #[test]
    fn a_deferred_frame_does_not_end_the_emergency() {
        // A frame whose budget ran out returns successfully. Treating that
        // as "the demand is satisfied" resets the backoff, and the next
        // ceiling starts from scratch.
        let mut cap = Capacity::new();
        cap.at_capacity(0.0);
        assert_ne!(cap.submitted_frozen(0.0, 4, 0), Next::Recovered);
    }

    #[test]
    fn a_recovery_does_not_reset_the_backoff() {
        // Scrolling through CJK: after each clear the screen fits for a
        // frame, then fills up again. Treating that satisfied frame as the
        // end of the emergency reset the backoff, so the next capacity hit
        // cleared at once -- clears 48 ms apart in simulation, which is the
        // storm the backoff exists to prevent.
        let mut cap = Capacity::new();
        let mut now = 0.0f64;
        let mut clears = vec![];
        for _ in 0..3000 {
            if cap.frozen() {
                if let Next::Clear = cap.submitted_frozen(now, 0, 4) {
                    clears.push(now);
                }
            } else {
                cap.drew_everything();
                cap.at_capacity(now);
            }
            now += 16.0;
        }
        assert!(clears.len() > 2, "it has to keep trying: {clears:?}");
        for pair in clears.windows(2) {
            assert!(
                pair[1] - pair[0] >= MIN_RETRY_MS,
                "two clears {} ms apart: {clears:?}",
                pair[1] - pair[0]
            );
        }
    }

    #[test]
    fn a_long_healthy_stretch_still_clears_at_once() {
        // The other side of the same rule: the deadline is an absolute
        // time, so a terminal that has been fine for a minute does not have
        // to wait out an old backoff.
        let mut cap = Capacity::new();
        cap.at_capacity(0.0);
        assert_eq!(cap.submitted_frozen(0.0, 0, 1), Next::Clear);
        for _ in 0..600 {
            cap.drew_everything();
        }
        cap.at_capacity(60_000.0);
        assert_eq!(cap.submitted_frozen(60_000.0, 0, 1), Next::Clear);
    }

    #[test]
    fn a_thin_glyph_is_never_scaled_out_of_existence() {
        // A full-width rule -- U+203E, U+2015, U+FF3F -- is a pixel or two
        // tall and far wider than its cell. Each case asserts its own
        // premise first: without the clamp the height really would truncate
        // to zero, so the test cannot quietly pass on inputs that never
        // needed it.
        for (w, h, max) in [(24usize, 1usize, 10.0f64), (40, 2, 12.5), (30, 2, 10.0)] {
            let unclamped = max / w as f64;
            assert_eq!(
                (h as f64 * unclamped) as usize,
                0,
                "premise: {w}x{h} at {max} would round the height away"
            );
            let ink = Ink { left: 0, top: 10, width: w, height: h, clipped: false };
            let p = place(&ink, 0.0, 20.0, max);
            assert!(
                (h as f64 * p.scale) as usize >= 1,
                "{w}x{h}: scale {} rounds the height away",
                p.scale
            );
            assert!((w as f64 * p.scale) as usize >= 1, "{w}x{h}: scale {}", p.scale);
            assert!(p.scale <= 1.0, "a glyph is never enlarged: {}", p.scale);
        }
    }

    #[test]
    fn right_to_left_scripts_keep_their_boxes() {
        // Not just the joining ones: the emitter does no bidi, so Hebrew
        // would render as legible-but-reversed text, which a reader is far
        // less likely to notice than a row of boxes.
        for text in ["\u{628}", "\u{5d0}", "\u{780}", "\u{710}", "\u{870}", "\u{8a0}", "\u{fb2a}"] {
            assert!(keeps_notdef(text), "{text:?} would be laid out backwards");
        }
        for text in ["\u{4e2d}", "\u{d55c}", "\u{3042}", "\u{1f600}", "e\u{301}", "\u{e01}"] {
            assert!(!keeps_notdef(text), "{text:?} should reach the canvas");
        }
    }

    #[test]
    fn the_retry_deadline_is_a_time_not_a_frame_count() {
        // Output stopped, so the clock is the only thing that moves. Any
        // number of frames at the same instant must not bring the retry
        // forward, and the retry must arrive once the clock passes it.
        let mut cap = Capacity::new();
        cap.at_capacity(0.0);
        assert_eq!(cap.submitted_frozen(0.0, 0, 1), Next::Clear);
        cap.at_capacity(0.0);
        let deadline = match cap.submitted_frozen(0.0, 0, 1) {
            Next::RetryAt(t) => t,
            other => panic!("expected a deadline, got {other:?}"),
        };
        for _ in 0..500 {
            assert_eq!(cap.submitted_frozen(0.0, 0, 1), Next::RetryAt(deadline));
        }
        assert_eq!(cap.submitted_frozen(deadline, 0, 1), Next::Clear);
    }
}
