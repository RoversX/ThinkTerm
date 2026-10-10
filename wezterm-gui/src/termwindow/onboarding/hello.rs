//! The intro the page opens with: "hello." written out in one stroke, then
//! taken back up along the same path while the page fades in under it. Just
//! before the tail has it all, the pen sets off again: on from the word's
//! end in one wide swoop to the page's icon, once round it, and the ring it
//! closes breaks into sparks that drift away.
//!
//! The word is a single hand-drawn path. The pen is a chain of round stamps
//! along it, one every `STEP` units, each tinted on its own: the colour of a
//! stamp is how far behind the pen it lies, read round a looping rainbow, so
//! the colours run along the ink exactly as fast as the pen moves and come
//! to rest when it stops, blue at the first stroke through green at the
//! last. Taking the word back up, a tail runs the same path after the pen
//! and the colours run on with it. Stamps are one small disc sprite drawn
//! many times, so a frame uploads nothing and draws the same on either
//! renderer.

use crate::quad::TripleLayerQuadAllocator;
use crate::ui::DrawContext;
use std::cell::RefCell;
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
/// When the pen leaves the word's end, counted from the start of taking it
/// back up: with about two thirds of the word gone, so the pen sets off
/// while the tail is still running towards it.
const LAUNCH: Duration = Duration::from_millis(950);
/// From the word's end round the icon until the ring closes.
const FLIGHT: Duration = Duration::from_millis(1300);
/// The closed ring goes this quickly once it has broken into sparks.
const RING_FADE: f32 = 0.22;
/// Sparks set off up to this late, in seconds, and drift for this long.
const SPARK_DELAY: f32 = 0.16;
const SPARK_LIFE: (f32, f32) = (0.9, 1.4);
/// From the ring closing until the last spark is gone.
const SPARKS_END: Duration = Duration::from_millis(1560);
/// The swoop's shape was drawn for a word this many pixels wide.
const SWOOP_WORD: f32 = 605.0;
/// The icon's rounded square inside its image (512 square: 50 in from each
/// side, corners about 100 round), as fractions of the image's side.
const BODY_INSET: f32 = 50.0 / 512.0;
const BODY_RADIUS: f32 = 100.0 / 512.0;
/// The ring's distance outside the rounded square, as a fraction of it.
const RING_GAP: f32 = 0.14;
/// The pen's width on the ring, and a spark's, against the word's.
const THIN: f32 = 0.28;
const SPARK: f32 = 0.3;
/// Past the word the rainbow turns this much faster than round the ring's
/// length: on a line this short it would otherwise read as one colour.
const TURN: f32 = 1.1;
/// How fast the colours keep running round the closed ring, in turns a
/// second.
const RING_RUN: f32 = 0.4;
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
    /// Whether the pen flies to the icon, settled the moment it would set
    /// off: a window resized afterwards does not cut the flight short or
    /// start one halfway.
    fly: Option<bool>,
}

impl Intro {
    pub(super) fn new() -> Self {
        Self {
            hello: Hello::new(),
            elapsed: Duration::ZERO,
            last_paint: None,
            warmed: false,
            fly: None,
        }
    }

    /// Advance to `now` and say where the intro is; `None` once it is over.
    /// `to_mark`: the page shows its icon, so the pen flies round it.
    pub(super) fn frame(&mut self, now: Instant, to_mark: bool) -> Option<Frame> {
        if let Some(last) = self.last_paint {
            self.elapsed += now.saturating_duration_since(last).min(MAX_STEP);
        }
        self.last_paint = Some(now);
        if self.fly.is_none() && self.elapsed >= WRITE + DOT_IN + HOLD + LAUNCH {
            self.fly = Some(to_mark);
        }
        Frame::at(self.elapsed, self.fly.unwrap_or(to_mark))
    }

    /// An intro whose page has just come fully in, for tests.
    #[cfg(test)]
    pub(super) fn with_page_shown() -> Self {
        Self {
            elapsed: WRITE + DOT_IN + HOLD + PAGE_AT + PAGE_IN,
            ..Self::new()
        }
    }

    /// Whether the page is fully in. From then on it takes input while
    /// whatever is left of the intro plays on over it.
    pub(super) fn page_shown(&self) -> bool {
        self.elapsed >= WRITE + DOT_IN + HOLD + PAGE_AT + PAGE_IN
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
    /// goes over it rather than under everything. `mark`: where the page's
    /// icon was last laid out.
    pub(super) fn paint(
        &self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        ground: LinearRgba,
        frame: Frame,
        page_under: bool,
        mark: Option<RectF>,
    ) -> anyhow::Result<()> {
        self.hello
            .paint(ctx, layers, area, ground, frame, page_under, mark)
    }
}

/// The path, sampled once.
struct Hello {
    /// A stamp every `STEP` units: position, and arc length from the start.
    stamps: Vec<(f32, f32, f32)>,
    length: f32,
    /// One trip round the loop: blue, to green, back to blue.
    colours: Vec<LinearRgba>,
    /// The pen's way on to the icon, laid out once as it sets off and kept
    /// to the end, gone with the intro: a page that moves its icon or loses
    /// it mid-flight (a resize, another language) cannot make the pen jump.
    course: RefCell<Option<Course>>,
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
    /// Once the pen has left the word for the icon.
    pub flight: Option<Flight>,
}

/// The pen's way on from the word: the swoop to the icon, then once round.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Flight {
    /// How far the pen has gone, as a fraction of swoop and ring together.
    pub pen: f32,
    /// How far the tail has followed onto the swoop, as a fraction of it.
    /// It stops where the ring starts, so the ring is whole when it closes.
    pub tail: f32,
    /// Seconds since the ring closed.
    pub closed: Option<f32>,
}

impl Frame {
    /// `None` once the intro is over: the page is fully in and, when the
    /// pen flies to the icon (`to_mark`), the last spark is gone.
    pub(super) fn at(elapsed: Duration, to_mark: bool) -> Option<Self> {
        let t = elapsed.as_secs_f32();
        let phase = |start: Duration, length: Duration| {
            ((t - start.as_secs_f32()) / length.as_secs_f32()).clamp(0.0, 1.0)
        };
        let retract_at = WRITE + DOT_IN + HOLD;
        let page_at = retract_at + PAGE_AT;
        let launch = retract_at + LAUNCH;
        let close = launch + FLIGHT;
        let end = if to_mark {
            (close + SPARKS_END).max(page_at + PAGE_IN)
        } else {
            page_at + PAGE_IN
        };
        if elapsed >= end {
            return None;
        }
        let tail = ease_in_steady_out(phase(retract_at, RETRACT));
        let dot_out = ((tail - DOT_OUT_FROM) / (1.0 - DOT_OUT_FROM)).clamp(0.0, 1.0);
        // The tail goes on onto the swoop once it has the whole word.
        let word_gone = retract_at + RETRACT;
        let flight = (to_mark && elapsed >= launch).then(|| Flight {
            pen: ease_in_out(phase(launch, FLIGHT)),
            tail: sine_in_out(phase(word_gone, close.saturating_sub(word_gone))),
            closed: (elapsed >= close).then(|| (elapsed - close).as_secs_f32()),
        });
        Some(Self {
            pen: ease_in_out(phase(Duration::ZERO, WRITE)),
            tail,
            dot: phase(WRITE, DOT_IN) - dot_out,
            page_in: (elapsed >= page_at).then(|| ease_in_out(phase(page_at, PAGE_IN))),
            flight,
        })
    }
}

/// Gently off and gently in.
fn sine_in_out(t: f32) -> f32 {
    (1.0 - (std::f32::consts::PI * t).cos()) / 2.0
}

/// Quick off the mark, settling slowly.
fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
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
            course: RefCell::new(None),
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
        mark: Option<RectF>,
    ) -> anyhow::Result<()> {
        let page_in = frame.page_in.unwrap_or(0.0);
        let (layer, cover) = if page_under {
            (2, ground.mul_alpha(1.0 - page_in))
        } else {
            (0, ground)
        };
        // Once the page is fully in, the ground over it is gone.
        if !page_under || page_in < 1.0 {
            ctx.draw_rect(
                layers,
                layer,
                area.origin.x,
                area.origin.y,
                area.size.width,
                area.size.height,
                cover,
            )?;
        }
        let width = ctx
            .px(WORD_W)
            .min(area.size.width - ctx.px(WORD_PAD) * 2.0)
            .max(1.0);
        let k = width / VIEW_W;
        let left = area.origin.x + (area.size.width - width) / 2.0 - VIEW_X * k;
        let top = area.origin.y + (area.size.height - VIEW_H * k) / 2.0 - VIEW_Y * k;

        let diameter = PEN_WIDTH * k;
        if let (Some(_), Some(mark)) = (frame.flight, mark) {
            let mut cached = self.course.borrow_mut();
            if cached.is_none() {
                let end = self.stamps[self.stamps.len() - 1];
                let before = self.stamps[self.stamps.len().saturating_sub(4)];
                let margin = ctx.px(16.0);
                let bounds = RectF::new(
                    euclid::point2(area.min_x() + margin, area.min_y() + margin),
                    euclid::size2(
                        (area.size.width - 2.0 * margin).max(1.0),
                        (area.size.height - 2.0 * margin).max(1.0),
                    ),
                );
                let course = Course::new(
                    (left + end.0 * k, top + end.1 * k),
                    (end.0 - before.0, end.1 - before.1),
                    diameter,
                    width / SWOOP_WORD,
                    mark,
                    bounds,
                );
                *cached = Some(course);
            }
        }
        let cached = self.course.borrow();
        let course = frame.flight.zip(cached.as_ref());
        // Past the word, a pixel of the course counts as this much of the
        // word's length, so a whole turn of the rainbow spans the ring.
        let warp = |course: &Course| 2.0 * self.length / (TURN * course.ring_length()).max(1.0);
        let (pen, tail) = match &course {
            Some((flight, course)) => {
                let w = warp(course);
                let tail = if frame.tail < 1.0 {
                    self.length * frame.tail
                } else {
                    self.length + course.ring_at * flight.tail * w
                };
                let run = flight.closed.unwrap_or(0.0) * RING_RUN * 2.0 * self.length;
                (self.length + course.length * flight.pen * w + run, tail)
            }
            None => (self.length * frame.pen, self.length * frame.tail),
        };
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
        if let Some((flight, course)) = &course {
            let w = warp(course);
            let ink = |s: f32| self.ink(self.length + s * w, pen + tail);
            let (head, from) = (course.length * flight.pen, course.ring_at * flight.tail);
            let ring = match flight.closed {
                Some(closed) => 1.0 - ease_out((closed / RING_FADE).min(1.0)),
                None => 1.0,
            };
            // The ring and its sparks can reach past the page in a short
            // window; what would cross its edge is left out rather than
            // drawn over the window's chrome.
            for &(x, y, s, d) in course.stamps.iter().take_while(|stamp| stamp.2 <= head) {
                if s < from || ring <= 0.0 || !within(area, x, y, d) {
                    continue;
                }
                ctx.draw_disc(layers, 2, x, y, d, ink(s).mul_alpha(ring))?;
            }
            if let Some(closed) = flight.closed {
                for spark in course.sparks(diameter, closed) {
                    if !within(area, spark.x, spark.y, spark.diameter) {
                        continue;
                    }
                    ctx.draw_disc(
                        layers,
                        2,
                        spark.x,
                        spark.y,
                        spark.diameter,
                        ink(spark.s).mul_alpha(spark.alpha),
                    )?;
                }
            }
        }
        Ok(())
    }
}

/// The pen's way on from the word, in pixels for the page as it is laid out
/// now: one swoop from the word's end, then once round the icon clockwise
/// from the middle of its top edge.
struct Course {
    /// A stamp every so often: position, distance along from the word's
    /// end, and the pen's width there, which narrows over the swoop.
    stamps: Vec<(f32, f32, f32, f32)>,
    /// How far along the ring starts, and the whole way.
    ring_at: f32,
    length: f32,
    /// The icon's middle, which the sparks fly away from.
    centre: (f32, f32),
}

/// One spark, drawn as a disc.
struct Spark {
    x: f32,
    y: f32,
    /// Where along the ring it came from, for its colour.
    s: f32,
    diameter: f32,
    alpha: f32,
}

impl Course {
    /// `from`: the word's end, leaving in direction `heading`, the pen
    /// `pen` wide there. `scale`: the swoop's size against its design.
    /// `mark`: the icon's image. The swoop's middle and handles are kept
    /// inside `bounds`, and with them the curve between its two ends; the
    /// ends are left where they are, so it starts on the word and lands
    /// exactly where the ring begins. The ring itself is not kept inside:
    /// what of it would cross the page's edge is left out when drawn.
    fn new(
        from: (f32, f32),
        heading: (f32, f32),
        pen: f32,
        scale: f32,
        mark: RectF,
        bounds: RectF,
    ) -> Self {
        let side = mark.size.width.min(mark.size.height);
        let body = side * (1.0 - 2.0 * BODY_INSET);
        let gap = body * RING_GAP;
        let (rx, ry) = (
            mark.min_x() + side * BODY_INSET - gap,
            mark.min_y() + side * BODY_INSET - gap,
        );
        let (rw, radius) = (body + 2.0 * gap, side * BODY_RADIUS + gap);
        let centre = (rx + rw / 2.0, ry + rw / 2.0);
        let thin = pen * THIN;
        // Close enough that the discs read as one line, and never closer
        // than a pixel, however small the window makes the pen.
        let spacing = (thin / 4.0).max(1.0);

        let inside = |(x, y): (f32, f32)| {
            (
                x.clamp(bounds.min_x(), bounds.max_x()),
                y.clamp(bounds.min_y(), bounds.max_y()),
            )
        };
        let length = (heading.0 * heading.0 + heading.1 * heading.1)
            .sqrt()
            .max(1e-3);
        let dir = (heading.0 / length, heading.1 / length);
        let (ex, ey) = from;
        let (cx, top) = (centre.0, ry);
        let lift = (40.0 * scale).max((ey - top) * 0.6);
        let handle = (150.0 * scale, -10.0 * scale);
        // Over the icon and down onto its top edge from the left needs room
        // above it to turn round in. Where the page leaves too little, the
        // pen comes up from the right onto the top edge instead and goes
        // round the other way: no turn back, so no hairpin.
        let room = top - bounds.min_y() - handle.1.abs();
        let over = room >= 60.0 * scale;
        let swoop = if over {
            let mid = (
                (cx + (ex - cx) * 0.35).clamp(
                    bounds.min_x() + handle.0,
                    (bounds.max_x() - handle.0).max(bounds.min_x() + handle.0),
                ),
                top - lift.min(room),
            );
            [
                [
                    from,
                    inside((ex + dir.0 * 150.0 * scale, ey + dir.1 * 150.0 * scale)),
                    (mid.0 + handle.0, mid.1 + handle.1),
                    mid,
                ],
                [
                    mid,
                    (mid.0 - handle.0, mid.1 - handle.1),
                    inside((cx - 130.0 * scale, top - 4.0 * scale)),
                    (cx, top),
                ],
            ]
        } else {
            // One rising curve, split in two at its middle so both shapes
            // share the code below.
            let a1 = inside((ex + dir.0 * 150.0 * scale, ey + dir.1 * 150.0 * scale));
            let a2 = inside((cx + 130.0 * scale, top - (4.0 * scale).min(room.max(0.0))));
            let half = |p: (f32, f32), q: (f32, f32)| ((p.0 + q.0) / 2.0, (p.1 + q.1) / 2.0);
            let (m01, m12, m23) = (half(from, a1), half(a1, a2), half(a2, (cx, top)));
            let (m012, m123) = (half(m01, m12), half(m12, m23));
            let mid = half(m012, m123);
            [[from, m01, m012, mid], [mid, m123, m23, (cx, top)]]
        };
        let mut swoop_line = vec![from];
        for [p0, p1, p2, p3] in swoop {
            for i in 1..=FLATTEN {
                let t = i as f32 / FLATTEN as f32;
                let u = 1.0 - t;
                let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
                swoop_line.push((
                    a * p0.0 + b * p1.0 + c * p2.0 + d * p3.0,
                    a * p0.1 + b * p1.1 + c * p2.1 + d * p3.1,
                ));
            }
        }

        // Round the ring clockwise from the middle of its top edge.
        let mut ring_line = vec![(cx, top)];
        let corner = |line: &mut Vec<(f32, f32)>, ax: f32, ay: f32, a0: f32| {
            for i in 1..=24 {
                let a = a0 + std::f32::consts::FRAC_PI_2 * i as f32 / 24.0;
                line.push((ax + radius * a.cos(), ay + radius * a.sin()));
            }
        };
        let (right, bottom) = (rx + rw, ry + rw);
        ring_line.push((right - radius, ry));
        corner(
            &mut ring_line,
            right - radius,
            ry + radius,
            -std::f32::consts::FRAC_PI_2,
        );
        ring_line.push((right, bottom - radius));
        corner(&mut ring_line, right - radius, bottom - radius, 0.0);
        ring_line.push((rx + radius, bottom));
        corner(
            &mut ring_line,
            rx + radius,
            bottom - radius,
            std::f32::consts::FRAC_PI_2,
        );
        ring_line.push((rx, ry + radius));
        corner(
            &mut ring_line,
            rx + radius,
            ry + radius,
            std::f32::consts::PI,
        );
        ring_line.push((cx, top));
        if !over {
            ring_line.reverse();
        }

        // Stamps every `spacing` along both, the swoop narrowing as it goes.
        let swoop_length = polyline_length(&swoop_line);
        let mut stamps = Vec::new();
        let (mut along, mut next) = (0.0f32, 0.0f32);
        let mut stamp_along = |line: &[(f32, f32)], width: &dyn Fn(f32) -> f32| {
            for pair in line.windows(2) {
                let ((x0, y0), (x1, y1)) = (pair[0], pair[1]);
                let step = ((x1 - x0).powi(2) + (y1 - y0).powi(2)).sqrt();
                while step > 0.0 && next <= along + step {
                    let t = (next - along) / step;
                    stamps.push((x0 + (x1 - x0) * t, y0 + (y1 - y0) * t, next, width(next)));
                    next += spacing;
                }
                along += step;
            }
        };
        stamp_along(&swoop_line, &|s| {
            pen + (thin - pen) * sine_in_out((s / swoop_length.max(1e-3)).min(1.0))
        });
        let ring_at = swoop_length;
        stamp_along(&ring_line, &|_| thin);
        // A stamp exactly where the ring closes, however the spacing fell.
        if next - spacing < along - 1e-3 {
            stamps.push((cx, top, along, thin));
        }
        let length = stamps.last().map_or(ring_at, |stamp| stamp.2);
        Self {
            stamps,
            ring_at,
            length,
            centre,
        }
    }

    fn ring_length(&self) -> f32 {
        self.length - self.ring_at
    }

    /// The ring's sparks `closed` seconds after it closed: one every so
    /// often along it, each flying out from the icon at its own angle,
    /// distance, start and pace, shrinking and fading as it goes. The same
    /// every time: the spread comes from each spark's place, not chance.
    fn sparks(&self, pen: f32, closed: f32) -> impl Iterator<Item = Spark> + '_ {
        let every = (pen * 0.22).max(1.0);
        let mut next = self.ring_at;
        self.stamps
            .iter()
            .filter(move |stamp| {
                if stamp.2 < next {
                    return false;
                }
                next = stamp.2 + every;
                true
            })
            .enumerate()
            .filter_map(move |(i, &(x, y, s, _))| {
                let r = |salt: u32| scatter(i as u32 * 4 + salt);
                let distance = pen * (1.06 + 2.0 * r(0));
                let angle = (r(1) - 0.5) * 0.9;
                let delay = SPARK_DELAY * r(2);
                let life = SPARK_LIFE.0 + (SPARK_LIFE.1 - SPARK_LIFE.0) * r(3);
                let u = (closed - delay) / life;
                if !(0.0..1.0).contains(&u) {
                    return None;
                }
                let (dx, dy) = (x - self.centre.0, y - self.centre.1);
                let len = (dx * dx + dy * dy).sqrt().max(1e-3);
                let (c, sn) = (angle.cos(), angle.sin());
                let (ux, uy) = ((dx * c - dy * sn) / len, (dx * sn + dy * c) / len);
                let go = distance * ease_out(u);
                Some(Spark {
                    x: x + ux * go,
                    y: y + uy * go,
                    s,
                    diameter: pen * SPARK * (1.0 - 0.7 * u),
                    alpha: 1.0 - u,
                })
            })
    }
}

/// Whether a disc `d` wide at (`x`, `y`) lies wholly inside `area`.
fn within(area: RectF, x: f32, y: f32, d: f32) -> bool {
    let r = d / 2.0;
    x - r >= area.min_x() && x + r <= area.max_x() && y - r >= area.min_y() && y + r <= area.max_y()
}

fn polyline_length(line: &[(f32, f32)]) -> f32 {
    line.windows(2)
        .map(|pair| ((pair[1].0 - pair[0].0).powi(2) + (pair[1].1 - pair[0].1).powi(2)).sqrt())
        .sum()
}

/// A number in 0..1 that looks random but is fixed by `n`.
fn scatter(n: u32) -> f32 {
    let mut h = n.wrapping_mul(0x9E37_79B9) ^ 0x85EB_CA6B;
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    (h & 0x00FF_FFFF) as f32 / 0x0100_0000 as f32
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
        let written = Frame::at(retract_at, false).unwrap();
        assert_eq!(hello.shown(written).count(), hello.stamps.len());
        let halfway = Frame::at(retract_at + RETRACT / 2, false).unwrap();
        let left = hello.shown(halfway).count();
        assert!(left > 0 && left < hello.stamps.len());
        // From the moment the tail arrives until the page is fully in.
        let after = retract_at + RETRACT + (PAGE_AT + PAGE_IN - RETRACT) / 2;
        assert!(Frame::at(after, false).is_some());
        for at in [retract_at + RETRACT, after] {
            let frame = Frame::at(at, false).unwrap();
            assert_eq!(hello.shown(frame).count(), 0, "at {:?}", at);
            assert!(frame.dot <= 1e-4);
        }
    }

    #[test]
    fn the_page_warms_up_once_while_the_word_is_still() {
        let mut intro = Intro::new();
        let writing = Frame::at(WRITE / 2, false).unwrap();
        assert!(!intro.wants_warm_up(writing));
        let still = Frame::at(WRITE + DOT_IN + HOLD / 2, false).unwrap();
        assert!(intro.wants_warm_up(still));
        assert!(!intro.wants_warm_up(still));
    }

    #[test]
    fn the_clock_waits_while_nothing_is_painted() {
        let mut intro = Intro::new();
        let t0 = Instant::now();
        assert_eq!(intro.frame(t0, false).unwrap().pen, 0.0);
        // A minute covered up moves the intro on by one step, not a minute.
        intro.frame(t0 + Duration::from_secs(60), false);
        assert_eq!(intro.elapsed, MAX_STEP);
        let mut now = t0 + Duration::from_secs(60);
        while intro.frame(now, false).is_some() {
            now += Duration::from_millis(16);
        }
        assert!(intro.elapsed >= WRITE + DOT_IN + HOLD + PAGE_AT + PAGE_IN);
    }

    #[test]
    fn timeline_writes_then_takes_it_back_up_under_the_page() {
        let start = Frame::at(Duration::ZERO, false).unwrap();
        assert_eq!((start.pen, start.tail, start.dot), (0.0, 0.0, 0.0));
        assert_eq!(start.page_in, None);

        let written = Frame::at(WRITE + DOT_IN, false).unwrap();
        assert_eq!((written.pen, written.tail), (1.0, 0.0));
        assert!(written.dot > 0.999);

        let retract_at = WRITE + DOT_IN + HOLD;
        // The page holds off until the tail is well along.
        let before_page = Frame::at(retract_at + PAGE_AT / 2, false).unwrap();
        assert!(before_page.tail > 0.0 && before_page.page_in.is_none());
        let page_starts = Frame::at(retract_at + PAGE_AT, false).unwrap();
        assert!(page_starts.tail > 0.55, "{}", page_starts.tail);

        // The tail leaves at a steady pace, not a crawl: its last tenth of the
        // path takes about a tenth of the time, and the full stop goes with
        // it rather than after.
        let near_end = Frame::at(retract_at + RETRACT * 9 / 10, false).unwrap();
        assert!(near_end.tail < 0.9 && near_end.dot > 0.0);
        let ink_gone = Frame::at(retract_at + RETRACT, false).unwrap();
        assert_eq!(ink_gone.tail, 1.0);
        assert!(ink_gone.dot.abs() < 1e-4);
        assert!(ink_gone.page_in.is_some_and(|p| p > 0.0 && p < 1.0));

        assert_eq!(Frame::at(retract_at + PAGE_AT + PAGE_IN, false), None);
    }

    #[test]
    fn the_pen_flies_round_the_icon_after_the_page_is_in() {
        let retract_at = WRITE + DOT_IN + HOLD;
        let launch = retract_at + LAUNCH;
        let close = launch + FLIGHT;
        // Up to the launch, flying changes nothing.
        let before = Frame::at(launch - Duration::from_millis(1), true).unwrap();
        assert_eq!(
            before,
            Frame::at(launch - Duration::from_millis(1), false).unwrap()
        );
        // The pen sets off while the tail is still on the word.
        let off = Frame::at(launch + Duration::from_millis(100), true).unwrap();
        let flight = off.flight.unwrap();
        assert!(flight.pen > 0.0 && off.tail < 1.0 && flight.tail == 0.0);
        // The page is in before the ring closes, and the intro runs on
        // until the last spark is gone.
        assert!(retract_at + PAGE_AT + PAGE_IN < close);
        let closed = Frame::at(close, true).unwrap().flight.unwrap();
        assert_eq!(
            (closed.pen, closed.tail, closed.closed),
            (1.0, 1.0, Some(0.0))
        );
        assert!(Frame::at(close + SPARKS_END - Duration::from_millis(1), true).is_some());
        assert_eq!(Frame::at(close + SPARKS_END, true), None);
        // No icon on the page: no flight, and over when the page is in.
        assert_eq!(
            Frame::at(launch + Duration::from_millis(100), false)
                .unwrap()
                .flight,
            None
        );
    }

    fn course() -> (Course, RectF) {
        let bounds = RectF::new(euclid::point2(0.0, 0.0), euclid::size2(1600.0, 1100.0));
        let mark = RectF::new(euclid::point2(728.0, 180.0), euclid::size2(144.0, 144.0));
        let course = Course::new((1300.0, 520.0), (1.0, -0.6), 30.0, 2.0, mark, bounds);
        (course, bounds)
    }

    #[test]
    fn the_course_runs_from_the_word_to_a_closed_ring_inside_the_page() {
        let (course, bounds) = course();
        let first = course.stamps[0];
        assert!((first.0 - 1300.0).abs() < 0.01 && (first.1 - 520.0).abs() < 0.01);
        assert_eq!(first.2, 0.0);
        assert!(course.ring_at > 0.0 && course.ring_at < course.length);
        for pair in course.stamps.windows(2) {
            assert!(pair[1].2 > pair[0].2);
        }
        // The pen narrows over the swoop and keeps a thin line round the ring.
        assert_eq!(first.3, 30.0);
        let last = course.stamps.last().unwrap();
        assert!((last.3 - 30.0 * THIN).abs() < 0.01);
        // The ring closes where it started: the middle of its top edge.
        let start = course
            .stamps
            .iter()
            .find(|stamp| stamp.2 >= course.ring_at)
            .unwrap();
        assert!((last.0 - start.0).abs() < 2.0 && (last.1 - start.1).abs() < 2.0);
        assert!((start.0 - 800.0).abs() < 2.0);
        for &(x, y, _, _) in &course.stamps {
            assert!(bounds.contains(euclid::point2(x, y)), "({}, {})", x, y);
        }
    }

    #[test]
    fn sparks_fly_outwards_and_are_gone_by_the_end() {
        let (course, _) = course();
        let at = |closed: f32| course.sparks(30.0, closed).collect::<Vec<_>>();
        assert!(!at(0.3).is_empty());
        // The same every time.
        let (a, b) = (at(0.5), at(0.5));
        assert_eq!(a.len(), b.len());
        assert!(a.iter().zip(&b).all(|(a, b)| a.x == b.x && a.y == b.y));
        // Further from the icon later on than where they started.
        let far = |sparks: &[Spark]| {
            sparks
                .iter()
                .map(|s| ((s.x - course.centre.0).powi(2) + (s.y - course.centre.1).powi(2)).sqrt())
                .sum::<f32>()
                / sparks.len() as f32
        };
        assert!(far(&at(0.8)) > far(&at(0.2)));
        assert!(at(SPARKS_END.as_secs_f32()).is_empty());
    }

    #[test]
    fn the_flight_is_settled_when_the_pen_sets_off() {
        let mut intro = Intro::new();
        let mut now = Instant::now();
        let launch = WRITE + DOT_IN + HOLD + LAUNCH;
        while intro.elapsed < launch {
            intro.frame(now, true);
            now += Duration::from_millis(16);
        }
        // The icon leaving the page afterwards does not ground the pen.
        let frame = intro.frame(now, false).unwrap();
        assert!(frame.flight.is_some());
        assert!(!intro.page_shown());
        while !intro.page_shown() {
            now += Duration::from_millis(16);
            assert!(intro.frame(now, false).is_some());
        }
        // The intro outlives the page coming in: the sparks are still to come.
        assert!(intro.frame(now, false).is_some());
    }

    #[test]
    fn the_swoop_stays_smooth_when_the_page_cuts_its_arc() {
        // The icon close under the page's top edge: the arc's high point
        // is pulled down onto the page, and the curve must not kink there.
        let bounds = RectF::new(euclid::point2(0.0, 0.0), euclid::size2(1600.0, 1100.0));
        let mark = RectF::new(euclid::point2(728.0, 40.0), euclid::size2(144.0, 144.0));
        let course = Course::new((1300.0, 900.0), (1.0, -0.6), 30.0, 2.0, mark, bounds);
        let swoop: Vec<_> = course
            .stamps
            .iter()
            .take_while(|stamp| stamp.2 <= course.ring_at)
            .collect();
        for w in swoop.windows(3) {
            let a = (w[1].1 - w[0].1).atan2(w[1].0 - w[0].0);
            let b = (w[2].1 - w[1].1).atan2(w[2].0 - w[1].0);
            let mut turn = (b - a).abs();
            if turn > std::f32::consts::PI {
                turn = 2.0 * std::f32::consts::PI - turn;
            }
            assert!(turn < 0.35, "kink of {} at {:?}", turn, w[1]);
        }
        // It lands where the ring starts, so the pen does not jump: the two
        // stamps either side of the join are no further apart than any two.
        let ring = course
            .stamps
            .iter()
            .find(|stamp| stamp.2 >= course.ring_at)
            .unwrap();
        let landed = swoop.last().unwrap();
        let gap = ((ring.0 - landed.0).powi(2) + (ring.1 - landed.1).powi(2)).sqrt();
        assert!(gap <= 30.0 * THIN / 4.0 + 0.01, "gap {}", gap);
        for stamp in &swoop {
            assert!(stamp.1 >= 0.0, "left the page at {:?}", stamp);
        }
    }
}
