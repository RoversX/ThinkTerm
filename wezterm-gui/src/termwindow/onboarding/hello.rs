//! The intro the page opens with: "hello." written out in one stroke, then
//! taken back up along the same path while the page fades in under it.
//!
//! The word is a single hand-drawn path. The pen is a chain of round stamps
//! along it, one every `STEP` units, each tinted on its own: the colour of a
//! stamp is how far behind the pen it lies, read round a looping rainbow, so
//! the colours run along the ink exactly as fast as the pen moves and come
//! to rest when it stops, blue at the first stroke through green at the
//! last. Taking the word back up, a tail runs the same path after the pen
//! and the colours run on with it. Stamps are one small disc sprite drawn many times, so a frame
//! uploads nothing and draws the same on either renderer.

use crate::quad::TripleLayerQuadAllocator;
use crate::ui::DrawContext;
use std::time::{Duration, Instant};
use window::color::LinearRgba;
use window::RectF;

/// Where the path starts, then each cubic as control, control, end. In the
/// units of the box below, which is the drawing's extent with the stroke's
/// width around it.
const START: (f32, f32) = (40.0, 300.0);
#[rustfmt::skip]
const CUBICS: &[[f32; 6]] = &[
    // h
    [110.0, 270.0, 190.0, 200.0, 222.0, 128.0],
    [246.0, 74.0, 236.0, 40.0, 214.0, 42.0],
    [190.0, 44.0, 175.0, 110.0, 166.0, 185.0],
    [160.0, 238.0, 155.0, 282.0, 154.0, 304.0],
    [164.0, 250.0, 192.0, 216.0, 226.0, 216.0],
    [262.0, 216.0, 268.0, 246.0, 260.0, 272.0],
    [252.0, 296.0, 264.0, 308.0, 290.0, 306.0],
    // e
    [322.0, 304.0, 356.0, 284.0, 372.0, 260.0],
    [386.0, 238.0, 374.0, 214.0, 350.0, 218.0],
    [322.0, 222.0, 308.0, 256.0, 314.0, 280.0],
    [320.0, 304.0, 348.0, 310.0, 378.0, 302.0],
    // l
    [418.0, 292.0, 456.0, 230.0, 474.0, 150.0],
    [488.0, 90.0, 484.0, 48.0, 462.0, 48.0],
    [440.0, 48.0, 428.0, 100.0, 426.0, 170.0],
    [424.0, 240.0, 434.0, 302.0, 470.0, 304.0],
    // l
    [508.0, 304.0, 546.0, 232.0, 564.0, 150.0],
    [578.0, 90.0, 574.0, 48.0, 552.0, 48.0],
    [530.0, 48.0, 518.0, 100.0, 516.0, 170.0],
    [514.0, 240.0, 524.0, 302.0, 560.0, 304.0],
    // o, and the tail off it
    [596.0, 304.0, 616.0, 224.0, 672.0, 218.0],
    [648.0, 218.0, 636.0, 250.0, 640.0, 276.0],
    [644.0, 302.0, 672.0, 312.0, 696.0, 298.0],
    [722.0, 282.0, 726.0, 244.0, 706.0, 226.0],
    [696.0, 217.0, 680.0, 216.0, 672.0, 222.0],
    [700.0, 238.0, 750.0, 240.0, 790.0, 212.0],
];
/// The full stop, after the word: centre and radius.
const DOT: (f32, f32, f32) = (812.0, 300.0, 12.0);
const PEN_WIDTH: f32 = 22.0;
const VIEW_X: f32 = 20.0;
const VIEW_Y: f32 = 20.0;
const VIEW_W: f32 = 820.0;
const VIEW_H: f32 = 300.0;

/// Arc length between stamps, in path units. A tenth of the pen's width
/// leaves no visible scallop along the edge.
const STEP: f32 = 2.0;
/// Pieces each cubic is flattened into while measuring it.
const FLATTEN: usize = 1000;

/// Design pixels (see the parent module): the word's width when the page
/// has room, and the margin it keeps when it does not.
const WORD_W: f32 = 1120.0;
const WORD_PAD: f32 = 64.0;

const WRITE: Duration = Duration::from_millis(2400);
const DOT_IN: Duration = Duration::from_millis(220);
const HOLD: Duration = Duration::from_millis(500);
/// Taking the word back up, a little slower on average than it was
/// written. The tail eases off the mark and leaves at a steady pace: easing
/// it to a stop left the last scrap of ink crawling, which read as the
/// animation catching, and leaving at a run read as hurried.
const RETRACT: Duration = Duration::from_millis(1400);
/// The full stop goes as the tail covers this last part of the path, so it
/// is never left on its own.
const DOT_OUT_FROM: f32 = 0.9;
/// The page waits until the tail has taken back most of the word, so the
/// two barely cross, and finishes a little after the ink is gone.
const PAGE_AT: Duration = Duration::from_millis(1300);
const PAGE_IN: Duration = Duration::from_millis(700);
/// The longest step one paint advances the intro by. A window that is
/// covered or on another Space is not painted at all, so the intro waits
/// for it instead of playing to nobody.
const MAX_STEP: Duration = Duration::from_millis(50);

/// The rainbow from the first stroke to the last; `colour_at` runs it back
/// the other way after green so it can loop without a seam.
const STOPS: &[(f32, (u8, u8, u8))] = &[
    (0.0, (0x1E, 0x9B, 0xFF)),
    (0.2, (0xB4, 0x3C, 0xF0)),
    (0.38, (0xF0, 0x28, 0x8C)),
    (0.55, (0xFF, 0x2D, 0x2D)),
    (0.72, (0xFF, 0x8A, 0x00)),
    (0.86, (0xF2, 0xC2, 0x00)),
    (1.0, (0x6C, 0xC8, 0x00)),
];
const LOOP_STEPS: usize = 512;

/// The intro's clock: it runs only while the page is being painted.
pub(super) struct Intro {
    hello: Hello,
    elapsed: Duration,
    last_paint: Option<Instant>,
    warmed: bool,
}

impl Intro {
    pub(super) fn new() -> Self {
        Self {
            hello: Hello::new(),
            elapsed: Duration::ZERO,
            last_paint: None,
            warmed: false,
        }
    }

    /// Advance to `now` and say where the intro is; `None` once it is over.
    pub(super) fn frame(&mut self, now: Instant) -> Option<Frame> {
        if let Some(last) = self.last_paint {
            self.elapsed += now.saturating_duration_since(last).min(MAX_STEP);
        }
        self.last_paint = Some(now);
        Frame::at(self.elapsed)
    }

    /// Whether to paint the page, hidden, this once. Its first paint shapes
    /// its text, rasterizes its glyphs and icons and may grow the atlas;
    /// done the moment the page starts to show, that stalls the tail
    /// mid-stroke. While the finished word sits still a slow frame cannot
    /// be seen.
    pub(super) fn wants_warm_up(&mut self, frame: Frame) -> bool {
        let still = frame.pen >= 1.0 && frame.dot >= 1.0 && frame.tail <= 0.0;
        if self.warmed || !still {
            return false;
        }
        self.warmed = true;
        true
    }

    /// `page_under`: the page was painted first this frame, so the ground
    /// goes over it rather than under everything.
    pub(super) fn paint(
        &self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        ground: LinearRgba,
        frame: Frame,
        page_under: bool,
    ) -> anyhow::Result<()> {
        self.hello
            .paint(ctx, layers, area, ground, frame, page_under)
    }
}

/// The path, sampled once.
struct Hello {
    /// A stamp every `STEP` units: position, and arc length from the start.
    stamps: Vec<(f32, f32, f32)>,
    length: f32,
    /// One trip round the loop: blue, to green, back to blue.
    colours: Vec<LinearRgba>,
}

/// Where the intro is at one moment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Frame {
    /// How far along the path the pen is, as a fraction of its length.
    pub pen: f32,
    /// How far the tail has followed it, likewise; the ink between the two
    /// is what shows.
    pub tail: f32,
    /// The full stop's opacity.
    pub dot: f32,
    /// Once it has started, how far the page has faded in.
    pub page_in: Option<f32>,
}

impl Frame {
    /// `None` once the page is fully in and the intro is over.
    pub(super) fn at(elapsed: Duration) -> Option<Self> {
        let t = elapsed.as_secs_f32();
        let phase = |start: Duration, length: Duration| {
            ((t - start.as_secs_f32()) / length.as_secs_f32()).clamp(0.0, 1.0)
        };
        let retract_at = WRITE + DOT_IN + HOLD;
        let page_at = retract_at + PAGE_AT;
        if elapsed >= page_at + PAGE_IN {
            return None;
        }
        let tail = ease_in_steady_out(phase(retract_at, RETRACT));
        let dot_out = ((tail - DOT_OUT_FROM) / (1.0 - DOT_OUT_FROM)).clamp(0.0, 1.0);
        Some(Self {
            pen: ease_in_out(phase(Duration::ZERO, WRITE)),
            tail,
            dot: phase(WRITE, DOT_IN) - dot_out,
            page_in: (elapsed >= page_at).then(|| ease_in_out(phase(page_at, PAGE_IN))),
        })
    }
}

/// From rest, ending at the average speed: never faster than 4/3 of it.
fn ease_in_steady_out(t: f32) -> f32 {
    t * t * (2.0 - t)
}

/// Slow off the mark, quick through the letters, easing into the end.
fn ease_in_out(t: f32) -> f32 {
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

impl Hello {
    fn new() -> Self {
        // Flatten finely (no piece longer than half a unit), then keep a
        // point every STEP of arc length.
        let mut stamps = vec![(START.0, START.1, 0.0)];
        let (mut length, mut since) = (0.0f32, 0.0f32);
        let (mut prev, mut from) = (START, START);
        for c in CUBICS {
            for i in 1..=FLATTEN {
                let t = i as f32 / FLATTEN as f32;
                let u = 1.0 - t;
                let x = u * u * u * from.0
                    + 3.0 * u * u * t * c[0]
                    + 3.0 * u * t * t * c[2]
                    + t * t * t * c[4];
                let y = u * u * u * from.1
                    + 3.0 * u * u * t * c[1]
                    + 3.0 * u * t * t * c[3]
                    + t * t * t * c[5];
                let step = ((x - prev.0).powi(2) + (y - prev.1).powi(2)).sqrt();
                length += step;
                since += step;
                if since >= STEP {
                    stamps.push((x, y, length));
                    since -= STEP;
                }
                prev = (x, y);
            }
            from = (c[4], c[5]);
        }
        if since > 0.0 {
            stamps.push((prev.0, prev.1, length));
        }
        let colours = (0..LOOP_STEPS)
            .map(|i| colour_at(i as f32 / LOOP_STEPS as f32))
            .collect();
        Self {
            stamps,
            length,
            colours,
        }
    }

    /// The ink at arc length `s` when pen and tail together have travelled
    /// `travel`: green at the pen while it writes, cycling backwards along
    /// the stroke, half a loop over the whole word.
    fn ink(&self, s: f32, travel: f32) -> LinearRgba {
        let turn = 0.5 + (travel - s) / (2.0 * self.length);
        let i = (turn * LOOP_STEPS as f32).floor() as isize;
        self.colours[i.rem_euclid(LOOP_STEPS as isize) as usize]
    }

    /// The stamps between tail and pen. The last stamp sits exactly at the
    /// end of the path, so once the tail gets there nothing is left: a
    /// tail that stops a hair short of the end must not leave it behind.
    fn shown(&self, frame: Frame) -> impl Iterator<Item = &(f32, f32, f32)> {
        let pen = self.length * frame.pen;
        let tail = if frame.tail >= 1.0 - 1e-4 {
            f32::INFINITY
        } else {
            self.length * frame.tail
        };
        self.stamps
            .iter()
            .take_while(move |stamp| stamp.2 <= pen)
            .filter(move |stamp| stamp.2 > tail || (tail == 0.0 && stamp.2 == 0.0))
    }

    /// The intro over the whole content area. When the page is painted
    /// first this lays the ground over it, as see-through as the page has
    /// faded in, and what is left of the word on top.
    fn paint(
        &self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        ground: LinearRgba,
        frame: Frame,
        page_under: bool,
    ) -> anyhow::Result<()> {
        let page_in = frame.page_in.unwrap_or(0.0);
        let (layer, cover) = if page_under {
            (2, ground.mul_alpha(1.0 - page_in))
        } else {
            (0, ground)
        };
        ctx.draw_rect(
            layers,
            layer,
            area.origin.x,
            area.origin.y,
            area.size.width,
            area.size.height,
            cover,
        )?;
        let width = ctx
            .px(WORD_W)
            .min(area.size.width - ctx.px(WORD_PAD) * 2.0)
            .max(1.0);
        let k = width / VIEW_W;
        let left = area.origin.x + (area.size.width - width) / 2.0 - VIEW_X * k;
        let top = area.origin.y + (area.size.height - VIEW_H * k) / 2.0 - VIEW_Y * k;

        let (pen, tail) = (self.length * frame.pen, self.length * frame.tail);
        let diameter = PEN_WIDTH * k;
        for &(x, y, s) in self.shown(frame) {
            ctx.draw_disc(
                layers,
                2,
                left + x * k,
                top + y * k,
                diameter,
                self.ink(s, pen + tail),
            )?;
        }
        if frame.dot > 0.0 {
            ctx.draw_disc(
                layers,
                2,
                left + DOT.0 * k,
                top + DOT.1 * k,
                DOT.2 * 2.0 * k,
                self.ink(self.length, pen + tail).mul_alpha(frame.dot),
            )?;
        }
        Ok(())
    }
}

/// `turn` round the loop, 0 and 1 both blue and 0.5 green, mixed in sRGB as
/// the design was.
fn colour_at(turn: f32) -> LinearRgba {
    let mut u = turn.rem_euclid(1.0) * 2.0;
    if u > 1.0 {
        u = 2.0 - u;
    }
    let k = STOPS
        .windows(2)
        .position(|pair| u <= pair[1].0)
        .unwrap_or(STOPS.len() - 2);
    let ((u0, a), (u1, b)) = (STOPS[k], STOPS[k + 1]);
    let f = ((u - u0) / (u1 - u0)).clamp(0.0, 1.0);
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * f).round() as u8;
    LinearRgba::with_srgba(mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2), 255)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_run_the_whole_path_at_an_even_spacing() {
        let hello = Hello::new();
        // The path as drawn in the design measures a little over 2900.
        assert!((2800.0..3000.0).contains(&hello.length), "{}", hello.length);
        let last = hello.stamps.last().unwrap();
        assert_eq!(last.2, hello.length);
        let end = CUBICS.last().unwrap();
        assert!((last.0 - end[4]).abs() < 0.01 && (last.1 - end[5]).abs() < 0.01);
        for pair in hello.stamps.windows(2) {
            let gap = pair[1].2 - pair[0].2;
            assert!(gap > 0.0 && gap < STEP + 0.6, "gap {}", gap);
        }
    }

    #[test]
    fn colours_settle_blue_to_green_when_the_pen_stops() {
        let hello = Hello::new();
        let blue = colour_at(0.0);
        let green = colour_at(0.5);
        assert_eq!(hello.ink(0.0, hello.length), blue);
        assert_eq!(hello.ink(hello.length, hello.length), green);
        // The pen's own ink is green the whole way.
        assert_eq!(hello.ink(700.0, 700.0), green);
    }

    #[test]
    fn the_loop_has_no_seam() {
        let a = colour_at(0.999);
        let b = colour_at(0.0);
        let close = |x: f32, y: f32| (x - y).abs() < 0.02;
        assert!(close(a.0, b.0) && close(a.1, b.1) && close(a.2, b.2));
        assert_eq!(colour_at(0.25), colour_at(0.75));
    }

    #[test]
    fn nothing_is_left_once_the_tail_arrives() {
        let hello = Hello::new();
        let retract_at = WRITE + DOT_IN + HOLD;
        let written = Frame::at(retract_at).unwrap();
        assert_eq!(hello.shown(written).count(), hello.stamps.len());
        let halfway = Frame::at(retract_at + RETRACT / 2).unwrap();
        let left = hello.shown(halfway).count();
        assert!(left > 0 && left < hello.stamps.len());
        // From the moment the tail arrives until the page is fully in.
        let after = retract_at + RETRACT + (PAGE_AT + PAGE_IN - RETRACT) / 2;
        assert!(Frame::at(after).is_some());
        for at in [retract_at + RETRACT, after] {
            let frame = Frame::at(at).unwrap();
            assert_eq!(hello.shown(frame).count(), 0, "at {:?}", at);
            assert!(frame.dot <= 1e-4);
        }
    }

    #[test]
    fn the_page_warms_up_once_while_the_word_is_still() {
        let mut intro = Intro::new();
        let writing = Frame::at(WRITE / 2).unwrap();
        assert!(!intro.wants_warm_up(writing));
        let still = Frame::at(WRITE + DOT_IN + HOLD / 2).unwrap();
        assert!(intro.wants_warm_up(still));
        assert!(!intro.wants_warm_up(still));
    }

    #[test]
    fn the_clock_waits_while_nothing_is_painted() {
        let mut intro = Intro::new();
        let t0 = Instant::now();
        assert_eq!(intro.frame(t0).unwrap().pen, 0.0);
        // A minute covered up moves the intro on by one step, not a minute.
        intro.frame(t0 + Duration::from_secs(60));
        assert_eq!(intro.elapsed, MAX_STEP);
        let mut now = t0 + Duration::from_secs(60);
        while intro.frame(now).is_some() {
            now += Duration::from_millis(16);
        }
        assert!(intro.elapsed >= WRITE + DOT_IN + HOLD + PAGE_AT + PAGE_IN);
    }

    #[test]
    fn timeline_writes_then_takes_it_back_up_under_the_page() {
        let start = Frame::at(Duration::ZERO).unwrap();
        assert_eq!((start.pen, start.tail, start.dot), (0.0, 0.0, 0.0));
        assert_eq!(start.page_in, None);

        let written = Frame::at(WRITE + DOT_IN).unwrap();
        assert_eq!((written.pen, written.tail), (1.0, 0.0));
        assert!(written.dot > 0.999);

        let retract_at = WRITE + DOT_IN + HOLD;
        // The page holds off until the tail is well along.
        let before_page = Frame::at(retract_at + PAGE_AT / 2).unwrap();
        assert!(before_page.tail > 0.0 && before_page.page_in.is_none());
        let page_starts = Frame::at(retract_at + PAGE_AT).unwrap();
        assert!(page_starts.tail > 0.55, "{}", page_starts.tail);

        // The tail leaves at a steady pace, not a crawl: its last tenth of the
        // path takes about a tenth of the time, and the full stop goes with
        // it rather than after.
        let near_end = Frame::at(retract_at + RETRACT * 9 / 10).unwrap();
        assert!(near_end.tail < 0.9 && near_end.dot > 0.0);
        let ink_gone = Frame::at(retract_at + RETRACT).unwrap();
        assert_eq!(ink_gone.tail, 1.0);
        assert!(ink_gone.dot.abs() < 1e-4);
        assert!(ink_gone.page_in.is_some_and(|p| p > 0.0 && p < 1.0));

        assert_eq!(Frame::at(retract_at + PAGE_AT + PAGE_IN), None);
    }
}
