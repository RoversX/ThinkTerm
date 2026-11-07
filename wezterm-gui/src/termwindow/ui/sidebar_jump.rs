//! The pill that floats at an edge of the left sidebar's thread list while
//! the thread the window is showing is out of sight -- scrolled away, or
//! folded into its project. It carries that thread's icon and an arrow
//! pointing where it is; a click unfolds its folder if need be and scrolls
//! the list back to it.

use crate::quad::TripleLayerQuadAllocator;
use crate::tab_icons::ResolvedIcon;
use crate::termwindow::ui::icons::SvgIcon;
use crate::termwindow::ui::sidebar::over;
use crate::termwindow::{UIItem, UIItemType};
use crate::ui::anim::{self, Easing, Timeline};
use crate::ui::draw::DrawContext;
use crate::ui::UiPalette;
use anyhow::Context;
use std::time::{Duration, Instant};
use window::color::LinearRgba;
use window::{RectF, WindowOps};

/// The pill's height, and the icon and arrow in it, in sidebar pixels.
const HEIGHT: usize = 56;
const ICON: usize = 36;
const ARROW: usize = 28;
const GAP: usize = 6;
const PAD_RIGHT: usize = 14;
/// How far in from the list's edge it floats.
const MARGIN: usize = 14;
/// How far it travels in from the edge as it comes.
const TRAVEL: f32 = 10.0;
/// How far the arrow leans the way it points under the pointer.
const NUDGE: f32 = 2.5;
/// A click presses it in and lets it back out over this long.
const PRESS: Duration = Duration::from_millis(200);
const PRESSED_SCALE: f32 = 0.94;
const FRAME: Duration = Duration::from_millis(16);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Edge {
    #[default]
    Top,
    Bottom,
}

/// The folder a thread is folded away in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Fold {
    Project(String),
    /// A reference group, by its key in `thread_ref_groups_collapsed`.
    RefGroup(String),
}

/// Where the paint walking the list found the thread the window shows.
pub(crate) struct CurrentRow {
    /// Its top in the list, unscrolled; for a folded one, where it would
    /// be once its folder opens.
    pub(crate) top: usize,
    /// The top of what stands for it on screen: the row itself, or the
    /// row of the folder it is folded in.
    pub(crate) anchor: usize,
    pub(crate) fold: Option<Fold>,
    pub(crate) icon: Option<ResolvedIcon>,
}

impl CurrentRow {
    /// A row at `top`; `folder` is the folder it is folded in, and the top
    /// of that folder's row.
    pub(crate) fn new(
        top: usize,
        folder: Option<(Fold, usize)>,
        icon: Option<ResolvedIcon>,
    ) -> Self {
        let (fold, anchor) = match folder {
            Some((fold, folder_top)) => (Some(fold), folder_top),
            None => (None, top),
        };
        Self {
            top,
            anchor,
            fold,
            icon,
        }
    }
}

/// The list the pill floats over, as this paint laid it out.
pub(crate) struct JumpList {
    pub(crate) x: usize,
    pub(crate) width: usize,
    /// The part of the list nothing covers: under the header, above the
    /// Settings row.
    pub(crate) top: usize,
    pub(crate) bottom: usize,
    pub(crate) scroll: f32,
    pub(crate) max_scroll: f32,
    pub(crate) row_height: usize,
}

/// One window's pill, and the scroll a click starts, between frames.
#[derive(Default)]
pub(crate) struct SidebarJump {
    edge: Edge,
    /// Whether the arrow points up, at the thread or its folder.
    up: bool,
    /// How far in it is, 0 to 1.
    shown: Option<Timeline>,
    showing: bool,
    /// How far the hover look has come, 0 to 1.
    hover: Option<Timeline>,
    hovered: bool,
    pressed_at: Option<Instant>,
    /// What it shows, kept so it can fade out showing it.
    icon: Option<ResolvedIcon>,
    /// The folder the last paint found the thread folded in, for a click.
    fold: Option<Fold>,
    /// A click asked for the thread: the next paint, which knows where its
    /// row is once its folder is open, starts the scroll.
    reveal: bool,
    scroll: Option<Timeline>,
    /// The offset the scroll last wrote. Anything else found there is the
    /// user scrolling meanwhile, and the hand wins.
    scroll_wrote: f32,
}

/// Where the pill goes, and whether its arrow points up, for a row centred
/// at `centre` over a list showing `top..bottom`; `anchor` is the centre of
/// what stands for the row on screen -- the row itself, or the row of the
/// folder it is `folded` in. `None` while the row is in sight.
fn place(centre: f32, anchor: f32, folded: bool, top: f32, bottom: f32) -> Option<(Edge, bool)> {
    if !folded && centre >= top && centre <= bottom {
        return None;
    }
    Some(if anchor < top {
        (Edge::Top, true)
    } else if anchor > bottom {
        (Edge::Bottom, false)
    } else if anchor < (top + bottom) / 2.0 {
        // A folder in sight: the far edge, clear of it, pointing back.
        (Edge::Bottom, true)
    } else {
        (Edge::Top, false)
    })
}

/// The offset that puts the row at `row_top` in the middle of a list
/// `viewport` high, clear of both edges, as far as the list goes.
fn scroll_target(row_top: usize, row_height: usize, viewport: f32, max_scroll: f32) -> f32 {
    (row_top as f32 + row_height as f32 / 2.0 - viewport / 2.0).clamp(0.0, max_scroll.max(0.0))
}

impl crate::TermWindow {
    /// Move the list on along a scroll a click started. Called before the
    /// paint reads the offset.
    pub(crate) fn advance_sidebar_jump_scroll(&mut self, now: Instant) {
        let jump = &mut self.sidebar_jump;
        let Some(scroll) = jump.scroll.as_mut() else {
            return;
        };
        if (self.workspace_sidebar_scroll_offset - jump.scroll_wrote).abs() > 0.5 {
            jump.scroll = None;
            return;
        }
        let running = scroll.advance(now);
        let offset = scroll.value(now);
        self.workspace_sidebar_scroll_offset = offset;
        jump.scroll_wrote = offset;
        if running {
            self.update_next_frame_time(Some(now + FRAME));
        } else {
            jump.scroll = None;
        }
    }

    /// The pill was clicked.
    pub(crate) fn activate_sidebar_jump(&mut self) {
        let jump = &mut self.sidebar_jump;
        if !jump.showing {
            return;
        }
        jump.pressed_at = Some(Instant::now());
        jump.reveal = true;
        if jump.fold.is_some() {
            // The row is to be scrolled to in the next frame, in the list as
            // it is with its folder open, so the folder does not unfold.
            self.folder_chevrons.land();
        }
        let jump = &mut self.sidebar_jump;
        match jump.fold.take() {
            Some(Fold::Project(project_id)) => {
                crate::workspace_threads::expand_project_threads(&project_id);
            }
            Some(Fold::RefGroup(key)) => {
                self.thread_ref_groups_collapsed.remove(&key);
            }
            None => {}
        }
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    /// Paint the pill over the list, after everything that covers the list:
    /// shown while `current` is out of sight, fading in and out.
    pub(crate) fn paint_sidebar_jump(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        current: Option<CurrentRow>,
        list: JumpList,
        chrome: &UiPalette,
    ) -> anyhow::Result<()> {
        let now = Instant::now();
        let reveal = std::mem::take(&mut self.sidebar_jump.reveal);
        // A swipe slides the list out from under it, and the pill would
        // stand still over the wrong Space. Scrolling the list up and down
        // is the gesture's other way and keeps it.
        if self.workspace_sidebar_swipe.is_sliding() || self.workspace_space_swipe_push_active {
            let jump = &mut self.sidebar_jump;
            jump.shown = None;
            jump.showing = false;
            return Ok(());
        }

        let (top, bottom) = (list.top as f32, list.bottom as f32);
        let row_height = list.row_height as f32;
        let mut want = None;
        if let Some(row) = current.as_ref() {
            // Centres on screen, against the uncovered list.
            let centre = top + row.top as f32 + row_height / 2.0 - list.scroll;
            let anchor = top + row.anchor as f32 + row_height / 2.0 - list.scroll;
            let place = place(centre, anchor, row.fold.is_some(), top, bottom);
            if reveal && row.fold.is_none() && place.is_some() {
                let from = self.workspace_sidebar_scroll_offset;
                let to = scroll_target(row.top, list.row_height, bottom - top, list.max_scroll);
                let duration = if (to - from).abs() > bottom - top {
                    anim::LONG
                } else {
                    anim::STANDARD
                };
                let jump = &mut self.sidebar_jump;
                jump.scroll = Some(Timeline::running(now, from, to, duration, Easing::OutCubic));
                jump.scroll_wrote = from;
                self.show_workspace_sidebar_scrollbar();
                self.update_next_frame_time(Some(now + FRAME));
            }
            let dragging = self
                .sidebar_row_drag
                .as_ref()
                .is_some_and(|drag| drag.active);
            if !dragging && self.sidebar_jump.scroll.is_none() {
                want = place;
            }
        }

        let jump = &mut self.sidebar_jump;
        if want.is_none() && jump.shown.is_none() {
            return Ok(());
        }
        if let Some((edge, up)) = want {
            if (jump.edge, jump.up) != (edge, up) {
                // Another place is another pill: it comes in from its edge.
                jump.edge = edge;
                jump.up = up;
                jump.shown = None;
                jump.showing = false;
            }
            jump.icon = current.as_ref().and_then(|row| row.icon.clone());
            jump.fold = current.as_ref().and_then(|row| row.fold.clone());
        }
        let shown = jump
            .shown
            .get_or_insert_with(|| Timeline::settled(now, 0.0));
        if want.is_some() != jump.showing {
            jump.showing = want.is_some();
            let (to, duration) = if jump.showing {
                (1.0, anim::SHORT)
            } else {
                (0.0, anim::MICRO)
            };
            shown.retarget(now, to, duration, Easing::OutCubic);
        }
        let mut animating = shown.advance(now);
        let shown_value = shown.value(now).clamp(0.0, 1.0);
        // A click outlasts the fade: it holds the pill's place until its
        // press is over (see the hit area below).
        let pressing = jump
            .pressed_at
            .is_some_and(|at| now.saturating_duration_since(at) < PRESS);
        if !jump.showing && !animating && !pressing {
            *jump = SidebarJump {
                scroll: jump.scroll.take(),
                scroll_wrote: jump.scroll_wrote,
                ..SidebarJump::default()
            };
            return Ok(());
        }
        let Some(icon) = jump.icon.clone() else {
            return Ok(());
        };
        let edge = jump.edge;
        let up = jump.up;
        let showing = jump.showing;

        let height = self.ui_px(HEIGHT) as f32;
        let icon_size = self.ui_px(ICON) as f32;
        let arrow_size = self.ui_px(ARROW) as f32;
        let pad = (height - icon_size) / 2.0;
        let width =
            pad + icon_size + self.ui_px(GAP) as f32 + arrow_size + self.ui_px(PAD_RIGHT) as f32;
        let x = (list.x as f32 + (list.width as f32 - width) / 2.0).round();
        let travel = (1.0 - shown_value) * self.ui_f32(TRAVEL);
        let y = match edge {
            Edge::Top => list.top as f32 + self.ui_px(MARGIN) as f32 - travel,
            Edge::Bottom => list.bottom as f32 - self.ui_px(MARGIN) as f32 - height + travel,
        }
        .round();
        let rest: RectF = euclid::rect(x, y, width, height);

        // Only one that is coming takes the pointer: one on its way out has
        // done its job.
        let hovered = showing
            && self.is_pointer_over_ui_rect(
                rest.origin.x as usize,
                rest.origin.y as usize,
                rest.size.width as usize,
                rest.size.height as usize,
            );
        let jump = &mut self.sidebar_jump;
        let hover = jump
            .hover
            .get_or_insert_with(|| Timeline::settled(now, 0.0));
        if hovered != jump.hovered {
            jump.hovered = hovered;
            hover.retarget(
                now,
                if hovered { 1.0 } else { 0.0 },
                anim::MICRO,
                Easing::Smooth,
            );
        }
        animating |= hover.advance(now);
        let hover_value = hover.value(now).clamp(0.0, 1.0);
        // How far in the click has it, 0 to 1 and back.
        let press = match jump.pressed_at {
            Some(at) if now.saturating_duration_since(at) < PRESS => {
                animating = true;
                let t = now.saturating_duration_since(at).as_secs_f32() / PRESS.as_secs_f32();
                (std::f32::consts::PI * t).sin()
            }
            Some(_) => {
                jump.pressed_at = None;
                0.0
            }
            None => 0.0,
        };

        let scale = 1.0 - (1.0 - PRESSED_SCALE) * press;
        let rect: RectF = euclid::rect(
            rest.center().x - width * scale / 2.0,
            rest.center().y - height * scale / 2.0,
            width * scale,
            height * scale,
        );
        let alpha = shown_value;
        // The selected row's grey, solid over the sidebar: rows pass under it.
        let base = over(chrome.sidebar_row_active_bg, chrome.workspace_sidebar_bg);
        let fill = crate::ui::color::mix(base, chrome.text, 0.07 * hover_value + 0.08 * press);
        let dark = chrome.is_dark();
        let shadow = LinearRgba::with_components(0.0, 0.0, 0.0, if dark { 0.5 } else { 0.22 });
        let arrow_color = crate::ui::color::mix(chrome.secondary_text, chrome.text, hover_value);
        let lean = hover_value * self.ui_f32(NUDGE);
        let (arrow, lean) = if up {
            (SvgIcon::ArrowUp, -lean)
        } else {
            (SvgIcon::ArrowDown, lean)
        };

        if alpha > 0.0 {
            let gl_state = self
                .render_state
                .as_ref()
                .context("sidebar jump render state")?;
            let ctx = DrawContext::new(gl_state, self.dimensions, &self.render_metrics);
            ctx.draw_elevated_surface(
                layers,
                2,
                rect,
                fill.mul_alpha(alpha),
                chrome.sidebar_row_active_border.mul_alpha(alpha),
                shadow.mul_alpha(alpha),
                rect.size.height / 2.0,
            )
            .context("sidebar jump pill")?;
            let icon_d = (icon_size * scale).round();
            let cy = rect.center().y;
            crate::thread_marks::draw_alone(
                &ctx,
                layers,
                &icon,
                (rect.origin.x + pad * scale).round(),
                (cy - icon_d / 2.0).round(),
                icon_d,
                dark,
                alpha,
            )
            .context("sidebar jump icon")?;
            let arrow_d = (arrow_size * scale).round();
            ctx.draw_svg_icon(
                layers,
                arrow,
                (rect.origin.x + (pad + icon_size + self.ui_px(GAP) as f32) * scale).round(),
                (cy - arrow_d / 2.0 + lean).round(),
                arrow_d,
                arrow_color.mul_alpha(alpha),
            )
            .context("sidebar jump arrow")?;
        }

        // Held for as long as it is drawn and through its press, even on its
        // way out: a click there -- a double click's second press -- would
        // otherwise land on the row under it. Only a pill that is coming
        // acts on it (`activate_sidebar_jump`).
        self.ui_items.push(UIItem {
            x: rest.origin.x as usize,
            y: rest.origin.y as usize,
            width: rest.size.width as usize,
            height: rest.size.height as usize,
            item_type: UIItemType::WorkspaceSidebarJump,
        });
        if animating {
            self.update_next_frame_time(Some(now + FRAME));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_in_sight_needs_no_pill() {
        assert_eq!(place(300.0, 300.0, false, 100.0, 500.0), None);
    }

    #[test]
    fn a_row_out_of_sight_is_pointed_at_from_its_side() {
        assert_eq!(
            place(40.0, 40.0, false, 100.0, 500.0),
            Some((Edge::Top, true))
        );
        assert_eq!(
            place(700.0, 700.0, false, 100.0, 500.0),
            Some((Edge::Bottom, false))
        );
    }

    #[test]
    fn a_folded_row_points_at_its_folder_from_clear_of_it() {
        // Its folder scrolled away: the folder's side.
        assert_eq!(
            place(300.0, 40.0, true, 100.0, 500.0),
            Some((Edge::Top, true))
        );
        assert_eq!(
            place(800.0, 700.0, true, 100.0, 500.0),
            Some((Edge::Bottom, false))
        );
        // Its folder in sight: the far edge, pointing back at it.
        assert_eq!(
            place(250.0, 200.0, true, 100.0, 500.0),
            Some((Edge::Bottom, true))
        );
        assert_eq!(
            place(450.0, 400.0, true, 100.0, 500.0),
            Some((Edge::Top, false))
        );
    }

    #[test]
    fn the_scroll_centres_the_row_as_far_as_the_list_goes() {
        assert_eq!(scroll_target(1000, 60, 400.0, 5000.0), 830.0);
        assert_eq!(scroll_target(50, 60, 400.0, 5000.0), 0.0);
        assert_eq!(scroll_target(1000, 60, 400.0, 600.0), 600.0);
    }
}
