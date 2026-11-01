//! A docked sidebar slides out of its edge as it opens, and back in as it
//! closes.
//!
//! Only the picture moves, as with a split's slide. The terminal is laid out
//! for where the sidebar ends up the moment it is toggled, so its programs
//! hear of a single resize; each frame after, the panel is drawn shifted
//! towards its edge by the part of it still to come, or already gone. Beside
//! the left panel the terminal -- tab row, panes, dividers -- goes along with
//! the panel's edge rather than waiting under it or beside a gap. The right
//! panel moves alone: the terminal keeps its left edge, and is narrower or
//! wider at once. Room the terminal does not reach yet looks like the panes
//! beside it. Its right edge stays where it is either way, and so do the
//! buttons along it, at the ends of the tab row and of the panes' bars.
//!
//! A panel a hover reveal has out over the terminal, docked, comes the rest
//! of the way out, and the terminal beside the left one comes out from
//! under it to make way.
//!
//! Either way a panel goes a little past where it goes and comes back: out,
//! it is off its edge by a sliver for a moment, which is drawn from what it
//! has at that edge; back, the terminal beside the left one touches the
//! window's edge and comes back off it. But for a hover reveal docked: that
//! eases the rest of the way, as the reveal eased out.
//!
//! A sidebar on its way back is collapsed already -- the terminal is laid out
//! without it -- but it is still on screen, as a hover reveal is while it
//! retreats: it is painted, and counts as presented, until it is gone, and
//! what it shows is let go of then.

use crate::termwindow::render::split_slide::{progress, Drawn, MOST_PAINTS};
use crate::ui::anim::{settle, tension, MOST_OVERSHOOT, OVERSHOOT};
use ::window::{RectF, WindowOps};
use std::time::{Duration, Instant};

/// A slide's length. A panel gets where it goes in under two thirds of it,
/// and spends the rest just past there and coming back.
const DURATION: Duration = Duration::from_millis(240);

/// One of the two sidebars.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Side {
    Left,
    Right,
}

/// How far a sidebar on its way is: from 0 to 1, and a little past either
/// end on the way to it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Out {
    /// How much of the panel is out...
    pub(crate) panel: f32,
    /// ...and how much of it the terminal beside it is clear of: as much,
    /// but for a hover reveal docked, which the terminal comes out from
    /// under.
    pub(crate) clear: f32,
}

impl Out {
    /// `out` of the panel out, the terminal along with its edge.
    pub(crate) fn along(out: f32) -> Self {
        Self {
            panel: out,
            clear: out,
        }
    }
}

/// A sidebar on its way out of its edge, or back in.
#[derive(Clone, Copy, Debug)]
struct Slide {
    opening: bool,
    /// Where it was as it set off.
    from: Out,
    /// See `settle`.
    tension: f32,
    started: Instant,
    /// Frames shown with it so far.
    shown: u32,
    /// Times painted with it so far, shown or not.
    paints: u32,
}

impl Slide {
    /// On its way from `from`, going `overshoot` of the way past its end.
    fn new(opening: bool, from: Out, overshoot: f32, now: Instant) -> Self {
        Self {
            opening,
            from,
            tension: tension(overshoot),
            started: now,
            shown: 0,
            paints: 0,
        }
    }

    /// Where it is; `None` once it is where it goes.
    fn out(&self, now: Instant) -> Option<Out> {
        let to = if self.opening { 1.0 } else { 0.0 };
        progress(self.started, self.shown, DURATION, now).map(|t| {
            let along = settle(t, self.tension);
            let clear = self.from.clear + (to - self.from.clear) * along;
            Out {
                // The terminal is never clear of more of the panel than is
                // out.
                panel: (self.from.panel + (to - self.from.panel) * along).max(clear),
                clear,
            }
        })
    }
}

/// Which sidebars went back into their edges.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Gone {
    pub(crate) left: bool,
    pub(crate) right: bool,
}

/// The sidebars on their way, each one at most.
#[derive(Debug, Default)]
pub(crate) struct SidebarSlides {
    left: Option<Slide>,
    right: Option<Slide>,
}

impl SidebarSlides {
    fn slide(&self, side: Side) -> Option<&Slide> {
        match side {
            Side::Left => self.left.as_ref(),
            Side::Right => self.right.as_ref(),
        }
    }

    fn slot(&mut self, side: Side) -> &mut Option<Slide> {
        match side {
            Side::Left => &mut self.left,
            Side::Right => &mut self.right,
        }
    }

    /// Set `side` on its way out (`opening`) or back, going `overshoot` of
    /// the way past its end. One on its way the other way turns round where
    /// it is, rather than from its end.
    pub(crate) fn start(&mut self, side: Side, opening: bool, overshoot: f32, now: Instant) {
        let slot = self.slot(side);
        if slot.is_some_and(|slide| slide.opening == opening) {
            return;
        }
        let from = slot
            .and_then(|slide| slide.out(now))
            .unwrap_or(Out::along(if opening { 0.0 } else { 1.0 }));
        *slot = Some(Slide::new(opening, from, overshoot, now));
    }

    /// Set `side` on its way out from `revealed` of it out over the
    /// terminal, as a hover reveal docked: the rest of it comes out, and the
    /// terminal beside it out from under it, easing there as the reveal did
    /// rather than going past.
    pub(crate) fn dock(&mut self, side: Side, revealed: f32, now: Instant) {
        let from = Out {
            panel: revealed,
            clear: 0.0,
        };
        *self.slot(side) = Some(Slide::new(true, from, 0.0, now));
    }

    /// Where `side` is while it is on its way (see `Out`).
    pub(crate) fn out(&self, side: Side, now: Instant) -> Option<Out> {
        let slide = self.slide(side)?;
        // Past its end and not yet let go of: where it goes.
        Some(
            slide
                .out(now)
                .unwrap_or(Out::along(if slide.opening { 1.0 } else { 0.0 })),
        )
    }

    /// Whether `side` is on its way back: collapsed, and still on screen.
    pub(crate) fn closing(&self, side: Side) -> bool {
        self.slide(side).is_some_and(|slide| !slide.opening)
    }

    /// Whether `side` is on its way, either way.
    pub(crate) fn sliding(&self, side: Side) -> bool {
        self.slide(side).is_some()
    }

    /// Whether `side`'s panel is on its way, rather than out where it docks
    /// while only the terminal beside it moves.
    pub(crate) fn panel_moving(&self, side: Side, now: Instant) -> bool {
        self.out(side, now).is_some_and(|out| out.panel != 1.0)
    }

    pub(crate) fn is_running(&self) -> bool {
        self.left.is_some() || self.right.is_some()
    }

    /// Take a frame's paint: those where they go by now are done. A frame
    /// painted again -- the quads ran out, a font came in -- is counted
    /// again, so one that is never shown still comes to an end.
    pub(crate) fn advance(&mut self, now: Instant) -> Gone {
        let ended = |slot: &mut Option<Slide>| -> Option<bool> {
            let slide = slot.as_mut()?;
            slide.paints += 1;
            if slide.paints <= MOST_PAINTS && slide.out(now).is_some() {
                return None;
            }
            slot.take().map(|slide| !slide.opening)
        };
        Gone {
            left: ended(&mut self.left).unwrap_or(false),
            right: ended(&mut self.right).unwrap_or(false),
        }
    }

    /// The frame painted with the slides was shown: they are a frame on.
    pub(crate) fn shown(&mut self) {
        for slide in [&mut self.left, &mut self.right] {
            if let Some(slide) = slide.as_mut() {
                slide.shown += 1;
            }
        }
    }

    /// Put both where they go at once.
    pub(crate) fn finish(&mut self) -> Gone {
        Gone {
            left: self.left.take().is_some_and(|slide| !slide.opening),
            right: self.right.take().is_some_and(|slide| !slide.opening),
        }
    }
}

/// One sidebar's part in a frame, in window pixels.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Panel {
    /// Its width when it is out.
    pub(crate) docked: f32,
    /// The width the terminal is laid out to leave it.
    pub(crate) laid_out: f32,
    /// Where it is, while it is on its way.
    pub(crate) out: Option<Out>,
}

impl Panel {
    /// Its width on screen this frame, in whole pixels.
    fn drawn(&self) -> f32 {
        match self.out {
            Some(out) => (out.panel * self.docked).round(),
            None => self.laid_out,
        }
    }

    /// How much of that the terminal beside it is clear of this frame.
    fn cleared(&self) -> f32 {
        match self.out {
            Some(out) => (out.clear * self.docked).round(),
            None => self.laid_out,
        }
    }
}

/// The bar along the top of a pane at the terminal's right edge, as laid
/// out: the band it runs along, and where along that the buttons at its
/// end start.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct EndBar {
    pub(crate) band: RectF,
    pub(crate) buttons: f32,
}

/// How a frame draws the sidebars on their way and the terminal between
/// them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SidebarSlideFrame {
    /// The left panel, painted where it is docked: drawn towards the
    /// window's edge by the part of it not out, and cut to the rest.
    pub(crate) left: Option<Drawn>,
    /// The right one, likewise.
    pub(crate) right: Option<Drawn>,
    /// Past where it docks, a panel is off its window edge by a sliver,
    /// drawn from what it has at that edge, where it docks.
    pub(crate) left_sliver: Option<Drawn>,
    /// The right one's, likewise.
    pub(crate) right_sliver: Option<Drawn>,
    /// The terminal, painted where it is laid out: along with the left
    /// panel's edge, or on its way out from under it, and cut to what is
    /// between the panels.
    pub(crate) terminal: Drawn,
    /// Room between the panels the terminal does not reach yet.
    pub(crate) room: Option<RectF>,
}

impl SidebarSlideFrame {
    /// The frame for `left` and `right` in a window whose panels go out from
    /// `left_edge` and `right_edge` across `height`; `None` while neither is
    /// on its way.
    pub(crate) fn new(
        left_edge: f32,
        right_edge: f32,
        height: f32,
        left: Panel,
        right: Panel,
    ) -> Option<Self> {
        if left.out.is_none() && right.out.is_none() {
            return None;
        }
        let (left_drawn, right_drawn) = (left.drawn(), right.drawn());
        let shift = left.cleared() - left.laid_out;
        // Gone past its edge, a panel is nothing on screen.
        let (left_on, right_on) = (left_drawn.max(0.), right_drawn.max(0.));
        let between_left = left_edge + left_on;
        let between_right = (right_edge - right_on).max(between_left);
        let terminal = Drawn {
            shift: (shift, 0.),
            clip: euclid::rect(between_left, 0., between_right - between_left, height),
        };
        // Where the terminal laid out against the right panel ends, drawn.
        let reach = (right_edge - right.laid_out + shift).max(between_left);
        let room =
            (reach < between_right).then(|| euclid::rect(reach, 0., between_right - reach, height));
        let sliver = |panel: &Panel, drawn: f32, edge: f32| {
            let past = drawn - panel.docked;
            (panel.out.is_some() && past > 0.).then(|| Drawn {
                shift: (0., 0.),
                clip: euclid::rect(edge, 0., past, height),
            })
        };
        let left_sliver = sliver(&left, left_drawn, left_edge);
        let right_sliver = sliver(
            &right,
            right_drawn,
            right_edge - (right_drawn - right.docked),
        );
        let left = left.out.map(|_| Drawn {
            shift: (left_drawn - left.docked, 0.),
            clip: euclid::rect(left_edge, 0., left_on, height),
        });
        let right = right.out.map(|_| Drawn {
            shift: (right.docked - right_drawn, 0.),
            clip: euclid::rect(right_edge - right_on, 0., right_on, height),
        });
        Some(Self {
            left,
            right,
            left_sliver,
            right_sliver,
            terminal,
            room,
        })
    }

    /// The pieces the terminal is drawn in, `bars` those along its right
    /// edge: as `terminal` has it, but for the buttons at the bars' ends,
    /// which stay where they are, with the bars' tabs cut short of them.
    /// With them, what of the bars neither covers, to be filled in their
    /// colour.
    pub(crate) fn terminal_pieces(&self, bars: &[EndBar]) -> (Vec<Drawn>, Vec<RectF>) {
        let between = self.terminal.clip;
        let shift = self.terminal.shift;
        if shift.0 == 0. {
            return (vec![self.terminal], vec![]);
        }
        let mut bands: Vec<_> = bars
            .iter()
            .filter_map(|bar| {
                let top = bar.band.min_y().max(between.min_y());
                let bottom = bar.band.max_y().min(between.max_y());
                (bottom > top).then_some((top, bottom, bar.buttons))
            })
            .collect();
        bands.sort_by(|a, b| a.0.total_cmp(&b.0));
        let (left, right) = (between.min_x(), between.max_x());
        let span = |top: f32, bottom: f32, from: f32, to: f32| {
            euclid::rect(from, top, to - from, bottom - top)
        };
        let mut pieces = vec![];
        let mut bare = vec![];
        let mut y = between.min_y();
        for (top, bottom, buttons) in bands {
            let top = top.max(y);
            if top >= bottom {
                continue;
            }
            pieces.push(Drawn {
                shift,
                clip: span(y, top, left, right),
            });
            let ends = buttons.clamp(left, right);
            // Drawn further left than they stand, the tabs would show the
            // buttons again.
            let tabs = (buttons + shift.0.min(0.)).clamp(left, ends);
            pieces.push(Drawn {
                shift,
                clip: span(top, bottom, left, tabs),
            });
            pieces.push(Drawn {
                shift: (0., 0.),
                clip: span(top, bottom, ends, right),
            });
            if ends > tabs {
                bare.push(span(top, bottom, tabs, ends));
            }
            y = bottom;
        }
        pieces.push(Drawn {
            shift,
            clip: span(y, between.max_y(), left, right),
        });
        pieces.retain(|drawn| !drawn.clip.is_empty());
        (pieces, bare)
    }
}

impl crate::TermWindow {
    /// Set `side` on its way out of its edge (`opening`) or back, unless the
    /// window draws nothing a slide can move: then the sidebars are where
    /// they go at once. Returns whether it is on its way.
    pub(crate) fn slide_sidebar(&mut self, side: Side, opening: bool) -> bool {
        if !self.sidebar_slides_drawable() {
            self.finish_sidebar_slides();
            return false;
        }
        let overshoot = self.sidebar_overshoot(side);
        self.sidebar_slides
            .start(side, opening, overshoot, Instant::now());
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
        true
    }

    /// Set `side`, docked from a hover reveal `revealed` of the way out, on
    /// its way the rest of the way, and the terminal beside the left one
    /// out from under it -- as `slide_sidebar` does, unless the window draws
    /// nothing a slide can move. Returns whether it is on its way.
    pub(crate) fn dock_revealed_sidebar(&mut self, side: Side, revealed: f32) -> bool {
        // All the way out, the right one has nothing to move.
        if side == Side::Right && revealed >= 1.0 {
            return false;
        }
        if !self.sidebar_slides_drawable() {
            self.finish_sidebar_slides();
            return false;
        }
        self.sidebar_slides.dock(side, revealed, Instant::now());
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
        true
    }

    /// Whether `side`'s panel is on its way rather than where it docks: what
    /// it shows is not where it is laid out, so it takes no wheel, as it
    /// takes no click.
    pub(crate) fn sidebar_panel_moving(&self, side: Side) -> bool {
        self.sidebar_slides.panel_moving(side, Instant::now())
    }

    /// How far past its end `side`'s panel goes, as a share of its width.
    fn sidebar_overshoot(&self, side: Side) -> f32 {
        let docked = match side {
            Side::Left => self.workspace_sidebar_docked_width(),
            Side::Right => self.right_sidebar_docked_width(),
        } as f32;
        OVERSHOOT.min(self.ui_f32(MOST_OVERSHOOT) / docked.max(1.0))
    }

    /// Whether a frame would draw the sidebars and the terminal between them
    /// from recordings a slide can move: not while hidden, nor while a view
    /// holds the terminal's place or is on its way to it, nor when panes are
    /// painted straight to the window.
    fn sidebar_slides_drawable(&self) -> bool {
        self.occluded.is_none()
            && !self.config.use_box_model_render
            && !self.content_view_foreground()
            && self.content_view_fade.is_none()
    }

    /// Have the sidebars on their way get there now.
    pub(crate) fn finish_sidebar_slides(&mut self) {
        let gone = self.sidebar_slides.finish();
        self.sidebar_slides_gone(gone);
    }

    /// What a sidebar that went back showed is let go of, as it would have
    /// been had it collapsed at once.
    fn sidebar_slides_gone(&mut self, gone: Gone) {
        if gone.right && self.right_sidebar_collapsed {
            self.let_go_of_right_sidebar();
        }
    }

    /// Take this frame's paint: the slides under way move on, and ask for
    /// the next frame.
    pub(crate) fn advance_sidebar_slides(&mut self, now: Instant) {
        if !self.sidebar_slides.is_running() {
            return;
        }
        if !self.sidebar_slides_drawable() {
            self.finish_sidebar_slides();
            return;
        }
        let gone = self.sidebar_slides.advance(now);
        self.sidebar_slides_gone(gone);
        if self.sidebar_slides.is_running() {
            if let Some(window) = self.window.as_ref() {
                window.invalidate();
            }
        }
    }

    /// How this frame draws the sidebars on their way.
    pub(crate) fn sidebar_slide_frame(&self, now: Instant) -> Option<SidebarSlideFrame> {
        let left_out = self.sidebar_slides.out(Side::Left, now);
        let right_out = self.sidebar_slides.out(Side::Right, now);
        if left_out.is_none() && right_out.is_none() {
            return None;
        }
        let border = self.get_os_border();
        SidebarSlideFrame::new(
            border.left.get() as f32,
            self.dimensions
                .pixel_width
                .saturating_sub(border.right.get()) as f32,
            self.dimensions.pixel_height as f32,
            Panel {
                docked: self.workspace_sidebar_docked_width() as f32,
                laid_out: self.workspace_sidebar_width() as f32,
                out: left_out,
            },
            Panel {
                docked: self.right_sidebar_docked_width() as f32,
                laid_out: self.right_sidebar_width() as f32,
                out: right_out,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::termwindow::render::split_slide::MIN_FRAMES;

    const FRAME: Duration = Duration::from_micros(16_667);
    /// How far past its end each slide here goes, as a panel's would.
    const PAST: f32 = OVERSHOOT;

    /// Where each frame has `side` from `now` on, a frame at a time, until
    /// it is where it goes, and which sides went back then.
    fn play_out(slides: &mut SidebarSlides, side: Side, now: &mut Instant) -> (Vec<Out>, Gone) {
        let mut seen = vec![];
        loop {
            let gone = slides.advance(*now);
            match slides.out(side, *now) {
                Some(out) => seen.push(out),
                None => return (seen, gone),
            }
            slides.shown();
            *now += FRAME;
            assert!(seen.len() < 100, "never got there: {:?}", seen);
        }
    }

    /// What each frame shows of `side`'s panel, as `play_out`.
    fn play(slides: &mut SidebarSlides, side: Side, now: &mut Instant) -> (Vec<f32>, Gone) {
        let (seen, gone) = play_out(slides, side, now);
        (seen.iter().map(|out| out.panel).collect(), gone)
    }

    /// Where `seen` is furthest along `towards` (1 out, 0 back): it goes
    /// that way up to there, and only back from there on.
    fn turn_of(seen: &[f32], towards: f32) -> usize {
        let along = |out: f32| (out - (1.0 - towards)).abs();
        let top = (0..seen.len())
            .max_by(|&a, &b| along(seen[a]).total_cmp(&along(seen[b])))
            .unwrap();
        assert!(
            seen[..=top]
                .windows(2)
                .all(|pair| along(pair[0]) < along(pair[1])),
            "{:?}",
            seen
        );
        assert!(
            seen[top..]
                .windows(2)
                .all(|pair| along(pair[0]) >= along(pair[1])),
            "{:?}",
            seen
        );
        top
    }

    #[test]
    fn opening_comes_out_quickly_goes_a_little_past_and_settles() {
        let mut now = Instant::now();
        let mut slides = SidebarSlides::default();
        slides.start(Side::Left, true, PAST, now);
        let (seen, gone) = play(&mut slides, Side::Left, &mut now);
        assert!(seen.len() as u32 >= MIN_FRAMES - 1, "{:?}", seen);
        // On its way in the first frame, but not most of the way there.
        assert!(seen[0] > 0.0 && seen[0] < 0.5, "{:?}", seen);
        // Past where it goes, by no more than it was to, and back.
        let top = seen[turn_of(&seen, 1.0)];
        assert!(top > 1.0 && top <= 1.0 + PAST + 1e-3, "{:?}", seen);
        assert!((seen.last().unwrap() - 1.0).abs() < 0.005, "{:?}", seen);
        assert_eq!(gone, Gone::default());
        assert!(!slides.is_running());
    }

    #[test]
    fn closing_goes_back_a_little_past_and_says_so_when_it_is_gone() {
        let mut now = Instant::now();
        let mut slides = SidebarSlides::default();
        slides.start(Side::Right, false, PAST, now);
        assert!(slides.closing(Side::Right));
        assert!(!slides.closing(Side::Left));
        let (seen, gone) = play(&mut slides, Side::Right, &mut now);
        assert!(seen.len() as u32 >= MIN_FRAMES - 1, "{:?}", seen);
        // Still most of the way out in the first frame.
        assert!(seen[0] > 0.5 && seen[0] < 1.0, "{:?}", seen);
        let bottom = seen[turn_of(&seen, 0.0)];
        assert!((-PAST - 1e-3..0.0).contains(&bottom), "{:?}", seen);
        assert_eq!(
            gone,
            Gone {
                left: false,
                right: true
            }
        );
        assert!(!slides.closing(Side::Right));
    }

    #[test]
    fn told_to_go_no_further_it_stops_where_it_goes() {
        let mut now = Instant::now();
        let mut slides = SidebarSlides::default();
        slides.start(Side::Left, true, 0.0, now);
        let (seen, _) = play(&mut slides, Side::Left, &mut now);
        assert!(seen.windows(2).all(|pair| pair[0] < pair[1]), "{:?}", seen);
        assert!(seen.iter().all(|&out| out <= 1.0), "{:?}", seen);
    }

    #[test]
    fn turned_round_it_goes_back_from_where_it_was() {
        let mut now = Instant::now();
        let mut slides = SidebarSlides::default();
        slides.start(Side::Left, true, PAST, now);
        let mut last = 0.0;
        for _ in 0..3 {
            slides.advance(now);
            last = slides.out(Side::Left, now).unwrap().panel;
            slides.shown();
            now += FRAME;
        }
        slides.start(Side::Left, false, PAST, now);
        let (seen, gone) = play(&mut slides, Side::Left, &mut now);
        // No jump: the next frame is about a frame's travel from the last.
        assert!((seen[0] - last).abs() < 0.3, "{} then {:?}", last, seen);
        assert!(seen[0] < last, "{} then {:?}", last, seen);
        let bottom = seen[turn_of(&seen, 0.0)];
        assert!(bottom >= -PAST - 1e-3, "{:?}", seen);
        assert!(gone.left);
    }

    #[test]
    fn asked_the_same_way_again_it_keeps_going() {
        let mut now = Instant::now();
        let mut slides = SidebarSlides::default();
        slides.start(Side::Right, true, PAST, now);
        slides.advance(now);
        slides.shown();
        now += FRAME;
        let before = slides.out(Side::Right, now).unwrap();
        slides.start(Side::Right, true, PAST, now);
        assert_eq!(slides.out(Side::Right, now), Some(before));
    }

    #[test]
    fn one_never_shown_still_ends() {
        let now = Instant::now();
        let mut slides = SidebarSlides::default();
        slides.start(Side::Right, false, PAST, now);
        let mut paints = 0;
        while slides.is_running() {
            // Painted again and again within the same instant, never shown.
            let gone = slides.advance(now);
            paints += 1;
            assert!(paints <= MOST_PAINTS + 1, "{}", paints);
            if !slides.is_running() {
                assert!(gone.right);
            }
        }
    }

    #[test]
    fn finishing_puts_both_where_they_go() {
        let now = Instant::now();
        let mut slides = SidebarSlides::default();
        slides.start(Side::Left, true, PAST, now);
        slides.start(Side::Right, false, PAST, now);
        assert_eq!(
            slides.finish(),
            Gone {
                left: false,
                right: true
            }
        );
        assert!(!slides.is_running());
        assert_eq!(slides.out(Side::Left, now), None);
    }

    #[test]
    fn docked_from_a_reveal_the_terminal_comes_out_from_under_it() {
        let mut now = Instant::now();
        let mut slides = SidebarSlides::default();
        slides.dock(Side::Left, 1.0, now);
        assert!(!slides.closing(Side::Left));
        let (seen, gone) = play_out(&mut slides, Side::Left, &mut now);
        assert!(seen.len() as u32 >= MIN_FRAMES - 1, "{:?}", seen);
        // From under the panel, which is out already and stays out...
        assert!(seen[0].clear > 0.0 && seen[0].clear < 0.5, "{:?}", seen);
        assert!(seen.iter().all(|out| out.panel == 1.0), "{:?}", seen);
        // ...easing to where it goes, and no further.
        assert!(
            seen.windows(2).all(|pair| pair[0].clear < pair[1].clear),
            "{:?}",
            seen
        );
        assert!(seen.iter().all(|out| out.clear <= 1.0), "{:?}", seen);
        let last = seen.last().unwrap();
        assert!((last.clear - 1.0).abs() < 0.005, "{:?}", seen);
        assert_eq!(gone, Gone::default());
    }

    #[test]
    fn docked_all_the_way_out_the_panel_stays_put() {
        let mut now = Instant::now();
        let mut slides = SidebarSlides::default();
        slides.dock(Side::Left, 1.0, now);
        slides.advance(now);
        assert!(slides.sliding(Side::Left));
        assert!(!slides.panel_moving(Side::Left, now));
        slides.dock(Side::Left, 0.4, now);
        assert!(slides.panel_moving(Side::Left, now));
        slides.start(Side::Right, true, PAST, now);
        slides.advance(now);
        slides.shown();
        now += FRAME;
        assert!(slides.panel_moving(Side::Right, now));
        play_out(&mut slides, Side::Right, &mut now);
        assert!(!slides.sliding(Side::Right));
        assert!(!slides.panel_moving(Side::Right, now));
    }

    #[test]
    fn docked_part_way_out_the_rest_of_the_panel_comes_out_too() {
        let mut now = Instant::now();
        let mut slides = SidebarSlides::default();
        slides.dock(Side::Left, 0.4, now);
        let (seen, _) = play_out(&mut slides, Side::Left, &mut now);
        // On from where it was, ahead of the terminal coming out from under
        // it until both get there, and no further.
        assert!(seen[0].panel > 0.4 && seen[0].panel < 0.8, "{:?}", seen);
        assert!(seen.iter().all(|out| out.panel >= out.clear), "{:?}", seen);
        assert!(
            seen.windows(2).all(|pair| pair[0].panel < pair[1].panel),
            "{:?}",
            seen
        );
        assert!(seen.iter().all(|out| out.panel <= 1.0), "{:?}", seen);
        let last = seen.last().unwrap();
        assert!((last.panel - 1.0).abs() < 0.005, "{:?}", seen);
    }

    #[test]
    fn turned_round_coming_out_from_under_it_both_go_back_from_where_they_were() {
        let mut now = Instant::now();
        let mut slides = SidebarSlides::default();
        slides.dock(Side::Left, 1.0, now);
        let mut last = Out::along(0.0);
        for _ in 0..2 {
            slides.advance(now);
            last = slides.out(Side::Left, now).unwrap();
            slides.shown();
            now += FRAME;
        }
        slides.start(Side::Left, false, PAST, now);
        let (seen, gone) = play_out(&mut slides, Side::Left, &mut now);
        assert!((seen[0].panel - last.panel).abs() < 0.3, "{:?}", seen);
        assert!((seen[0].clear - last.clear).abs() < 0.3, "{:?}", seen);
        assert!(seen.iter().all(|out| out.panel >= out.clear), "{:?}", seen);
        assert!(gone.left);
    }

    fn panel(docked: f32, laid_out: f32, out: Option<f32>) -> Panel {
        Panel {
            docked,
            laid_out,
            out: out.map(Out::along),
        }
    }

    #[test]
    fn out_of_a_reveal_the_terminal_is_drawn_from_under_the_panel() {
        // A quarter of the way out from under a 200 wide panel that is all
        // the way out, beside a docked 300 wide right one.
        let frame = SidebarSlideFrame::new(
            0.,
            1000.,
            600.,
            Panel {
                docked: 200.,
                laid_out: 200.,
                out: Some(Out {
                    panel: 1.0,
                    clear: 0.25,
                }),
            },
            panel(300., 300., None),
        )
        .unwrap();
        let left = frame.left.unwrap();
        assert_eq!(left.shift, (0., 0.));
        assert_eq!(left.clip, euclid::rect(0., 0., 200., 600.));
        assert_eq!(frame.left_sliver, None);
        // Laid out beside all of the panel, and drawn 150 short of there:
        // under it, but for what is past its edge.
        assert_eq!(frame.terminal.shift, (-150., 0.));
        assert_eq!(frame.terminal.clip, euclid::rect(200., 0., 500., 600.));
        assert_eq!(frame.room, Some(euclid::rect(550., 0., 150., 600.)));
    }

    fn bar(top: f32, buttons: f32) -> EndBar {
        EndBar {
            band: euclid::rect(200., top, 500., 30.),
            buttons,
        }
    }

    #[test]
    fn the_buttons_at_the_ends_of_the_bars_stay_where_they_are() {
        // Going back, the terminal beside the left panel 100 right of where
        // it is laid out.
        let terminal = |shift| SidebarSlideFrame {
            left: None,
            right: None,
            left_sliver: None,
            right_sliver: None,
            terminal: Drawn {
                shift: (shift, 0.),
                clip: euclid::rect(100., 0., 600., 600.),
            },
            room: None,
        };
        let bars = [bar(300., 600.), bar(40., 620.)];
        let (pieces, bare) = terminal(100.).terminal_pieces(&bars);
        let moved = (100., 0.);
        let expected = [
            (moved, euclid::rect(100., 0., 600., 40.)),
            (moved, euclid::rect(100., 40., 520., 30.)),
            ((0., 0.), euclid::rect(620., 40., 80., 30.)),
            (moved, euclid::rect(100., 70., 600., 230.)),
            (moved, euclid::rect(100., 300., 500., 30.)),
            ((0., 0.), euclid::rect(600., 300., 100., 30.)),
            (moved, euclid::rect(100., 330., 600., 270.)),
        ];
        assert_eq!(
            pieces
                .iter()
                .map(|drawn| (drawn.shift, drawn.clip))
                .collect::<Vec<_>>(),
            expected
        );
        assert!(bare.is_empty());

        // Coming out, drawn 100 left of there: the tabs stop that much short
        // of the buttons, the bar's colour between.
        let (pieces, bare) = terminal(-100.).terminal_pieces(&bars[..1]);
        assert_eq!(pieces[1].clip, euclid::rect(100., 300., 400., 30.));
        assert_eq!(pieces[2].shift, (0., 0.));
        assert_eq!(pieces[2].clip, euclid::rect(600., 300., 100., 30.));
        assert_eq!(bare, vec![euclid::rect(500., 300., 100., 30.)]);

        // Nothing moving beside the left panel, it is all one piece.
        let still = terminal(0.);
        assert_eq!(still.terminal_pieces(&bars), (vec![still.terminal], vec![]));
    }

    #[test]
    fn nothing_on_its_way_draws_as_laid_out() {
        assert_eq!(
            SidebarSlideFrame::new(
                0.,
                1000.,
                600.,
                panel(200., 200., None),
                panel(300., 0., None)
            ),
            None
        );
    }

    #[test]
    fn the_terminal_rides_the_left_panel_out_and_the_room_behind_is_filled() {
        // A quarter out of a 200 wide panel, the terminal laid out beside
        // all of it and a docked 300 wide right panel.
        let frame = SidebarSlideFrame::new(
            0.,
            1000.,
            600.,
            panel(200., 200., Some(0.25)),
            panel(300., 300., None),
        )
        .unwrap();
        let left = frame.left.unwrap();
        assert_eq!(left.shift, (-150., 0.));
        assert_eq!(left.clip, euclid::rect(0., 0., 50., 600.));
        assert_eq!(frame.right, None);
        assert_eq!(frame.terminal.shift, (-150., 0.));
        assert_eq!(frame.terminal.clip, euclid::rect(50., 0., 650., 600.));
        // Laid out to end at 700, it is drawn to end at 550.
        assert_eq!(frame.room, Some(euclid::rect(550., 0., 150., 600.)));
    }

    #[test]
    fn the_terminal_rides_the_left_panel_back_and_is_cut_short_of_the_right() {
        let frame = SidebarSlideFrame::new(
            10.,
            990.,
            600.,
            panel(200., 0., Some(0.5)),
            panel(300., 0., None),
        )
        .unwrap();
        let left = frame.left.unwrap();
        assert_eq!(left.shift, (-100., 0.));
        assert_eq!(left.clip, euclid::rect(10., 0., 100., 600.));
        assert_eq!(frame.terminal.shift, (100., 0.));
        assert_eq!(frame.terminal.clip, euclid::rect(110., 0., 880., 600.));
        assert_eq!(frame.room, None);
    }

    #[test]
    fn the_right_panel_moves_alone() {
        // Coming out: the terminal is laid out narrower at once, and what
        // the panel has not covered yet is room.
        let frame = SidebarSlideFrame::new(
            0.,
            1000.,
            600.,
            panel(200., 200., None),
            panel(300., 300., Some(0.5)),
        )
        .unwrap();
        assert_eq!(frame.left, None);
        let right = frame.right.unwrap();
        assert_eq!(right.shift, (150., 0.));
        assert_eq!(right.clip, euclid::rect(850., 0., 150., 600.));
        assert_eq!(frame.terminal.shift, (0., 0.));
        assert_eq!(frame.terminal.clip, euclid::rect(200., 0., 650., 600.));
        assert_eq!(frame.room, Some(euclid::rect(700., 0., 150., 600.)));

        // Going back: laid out wider at once, and cut where the panel is.
        let frame = SidebarSlideFrame::new(
            0.,
            1000.,
            600.,
            panel(200., 200., None),
            panel(300., 0., Some(0.5)),
        )
        .unwrap();
        assert_eq!(frame.terminal.clip, euclid::rect(200., 0., 650., 600.));
        assert_eq!(frame.room, None);
    }

    #[test]
    fn a_panel_is_drawn_in_whole_pixels() {
        let frame = SidebarSlideFrame::new(
            0.,
            1000.,
            600.,
            panel(201., 201., Some(1.0 / 3.0)),
            panel(0., 0., None),
        )
        .unwrap();
        assert_eq!(frame.left.unwrap().clip.width(), 67.);
        assert_eq!(frame.terminal.shift, (-134., 0.));
    }

    #[test]
    fn past_where_it_docks_a_panel_is_off_its_edge_by_a_sliver() {
        let frame = SidebarSlideFrame::new(
            0.,
            1000.,
            600.,
            panel(200., 200., Some(1.02)),
            panel(300., 300., Some(1.02)),
        )
        .unwrap();
        let left = frame.left.unwrap();
        assert_eq!(left.shift, (4., 0.));
        assert_eq!(left.clip, euclid::rect(0., 0., 204., 600.));
        let sliver = frame.left_sliver.unwrap();
        assert_eq!(sliver.shift, (0., 0.));
        assert_eq!(sliver.clip, euclid::rect(0., 0., 4., 600.));
        let right = frame.right.unwrap();
        assert_eq!(right.shift, (-6., 0.));
        assert_eq!(right.clip, euclid::rect(694., 0., 306., 600.));
        let sliver = frame.right_sliver.unwrap();
        assert_eq!(sliver.shift, (0., 0.));
        assert_eq!(sliver.clip, euclid::rect(994., 0., 6., 600.));
        // Along with the left one's edge, the terminal is past its place.
        assert_eq!(frame.terminal.shift, (4., 0.));
        assert_eq!(frame.terminal.clip, euclid::rect(204., 0., 490., 600.));
        assert_eq!(frame.room, None);

        // Short of it, there is none.
        let frame = SidebarSlideFrame::new(
            0.,
            1000.,
            600.,
            panel(200., 200., Some(0.98)),
            panel(300., 300., None),
        )
        .unwrap();
        assert_eq!(frame.left_sliver, None);
        assert_eq!(frame.right_sliver, None);
    }

    #[test]
    fn past_its_edge_a_panel_is_nothing_and_the_terminal_meets_the_window_edge() {
        // The left one back, and a little past: the terminal beside it is
        // drawn a little left of its place, room behind it.
        let frame = SidebarSlideFrame::new(
            0.,
            1000.,
            600.,
            panel(200., 0., Some(-0.02)),
            panel(300., 0., None),
        )
        .unwrap();
        assert_eq!(frame.left.unwrap().clip.width(), 0.);
        assert_eq!(frame.left_sliver, None);
        assert_eq!(frame.terminal.shift, (-4., 0.));
        assert_eq!(frame.terminal.clip, euclid::rect(0., 0., 1000., 600.));
        assert_eq!(frame.room, Some(euclid::rect(996., 0., 4., 600.)));

        // The right one: nothing moves but it.
        let frame = SidebarSlideFrame::new(
            0.,
            1000.,
            600.,
            panel(200., 200., None),
            panel(300., 0., Some(-0.02)),
        )
        .unwrap();
        assert_eq!(frame.right.unwrap().clip.width(), 0.);
        assert_eq!(frame.right_sliver, None);
        assert_eq!(frame.terminal.shift, (0., 0.));
        assert_eq!(frame.terminal.clip, euclid::rect(200., 0., 800., 600.));
        assert_eq!(frame.room, None);
    }
}
