//! The update capsule: a pill floating in the window's bottom-right corner
//! while a newer release is known, which then follows the install through
//! to the restart. Dismissed, it flies into the dot on the settings gear.

use crate::settings_window::UpdateCapsuleStatus;
use crate::termwindow::render::RenderMetrics;
use crate::termwindow::ui::icons::SvgIcon;
use crate::termwindow::{UIItem, UIItemType};
use crate::ui::anim::{Easing, Timeline};
use crate::ui::draw::DrawContext;
use anyhow::Context;
use std::cell::Cell;
use std::time::{Duration, Instant};
use window::color::LinearRgba;
use window::WindowOps;

/// Above the terminal and the chrome, under the content views' transition
/// layers: a view arriving, or the terminal flying into its card, passes
/// over it. A z-index of its own, because the quad layers within one are a
/// single order for the whole window: sharing one with another surface
/// would draw each one's text through the other's background.
const ZINDEX: i8 = 7;

/// The sizes below are in points: two of the chrome's design pixels,
/// which are a Retina display's pixels.
const POINT: f32 = 2.0;
const HEIGHT: f32 = 32.0;
const ICON: f32 = 20.0;
const PAD_LEFT: f32 = 6.0;
const PAD_RIGHT: f32 = 12.0;
const GAP: f32 = 8.0;
const WORD_GAP: f32 = 6.0;
const CLOSE: f32 = 16.0;
const MARGIN: f32 = 10.0;
/// The progress ring around the icon while the update downloads.
const RING_RADIUS: f32 = 10.5;
const RING_WIDTH: f32 = 2.2;

const ARRIVE: Duration = Duration::from_millis(520);
/// The sweep of light across a newly arrived capsule: twice, then still.
const SWEEP_DELAY: Duration = Duration::from_millis(600);
const SWEEP: Duration = Duration::from_millis(1500);
const SWEEPS: u32 = 2;
const RESIZE: Duration = Duration::from_millis(380);
const SHAKE: Duration = Duration::from_millis(420);
/// Folding into the icon, then the icon's flight to the gear.
const FOLD: Duration = Duration::from_millis(220);
const FLIGHT: Duration = Duration::from_millis(680);
const FRAME: Duration = Duration::from_millis(16);
/// How long a click shows as pressed.
const PRESS: Duration = Duration::from_millis(160);
/// How long after it changes what it says before a click on it counts: a
/// click aimed at what it said a moment ago must not install or restart.
const SETTLE: Duration = Duration::from_millis(350);

thread_local! {
    /// Dismissed in any window: it stays in the gear's dot until the next
    /// launch.
    static DISMISSED: Cell<bool> = const { Cell::new(false) };
}

/// Whether the capsule has gone into the gear's dot.
pub(crate) fn dismissed() -> bool {
    DISMISSED.with(Cell::get)
}

/// The settings gear's dot, when the capsule has gone into it: red when the
/// install failed.
pub(crate) fn gear_dot() -> Option<bool> {
    if !dismissed() {
        return None;
    }
    let status = crate::settings_window::update_capsule_status()?;
    Some(status == UpdateCapsuleStatus::Failed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Available,
    Installing,
    Restart,
    Installed,
    Failed,
}

impl Kind {
    fn of(status: &UpdateCapsuleStatus) -> Self {
        match status {
            UpdateCapsuleStatus::Available { .. } => Self::Available,
            UpdateCapsuleStatus::Installing { .. } => Self::Installing,
            UpdateCapsuleStatus::Installed { restart: true } => Self::Restart,
            UpdateCapsuleStatus::Installed { restart: false } => Self::Installed,
            UpdateCapsuleStatus::Failed => Self::Failed,
        }
    }
}

/// One window's capsule between frames.
#[derive(Default)]
pub(crate) struct UpdateCapsule {
    /// When this window first painted it: the arrival and the sweep time
    /// from here.
    shown_at: Option<Instant>,
    kind: Option<Kind>,
    changed_at: Option<Instant>,
    /// The width without the close button, eased on a change of words.
    width: Option<Timeline>,
    width_target: f32,
    /// How far the close button has opened, 0 to 1.
    close: Option<Timeline>,
    close_open: bool,
    /// What was last painted, for the flight to start from.
    painted: Option<Painted>,
    flight: Option<Flight>,
    /// The gear dot's centre and diameter: set by the sidebar as it paints,
    /// and read (then cleared) by the capsule later in the same frame.
    pub(crate) gear_dot_slot: Option<(f32, f32, f32)>,
    gear_dot: Option<(f32, f32, f32)>,
    /// Whether the last frame was part of a movement still under way.
    animating: bool,
    pressed_at: Option<Instant>,
}

impl UpdateCapsule {
    /// Its movements are short and few, and one stopped half way would stay
    /// on screen like that: they finish in an unfocused window too. The
    /// turning ring is not one of them; it keeps the spinners' clock.
    pub(crate) fn owes_frames(&self) -> bool {
        self.animating || self.owes_frames_while_hidden()
    }

    /// The dismissal's flight lands on a change every window shows, so it
    /// has to finish even in a window nobody can see.
    pub(crate) fn owes_frames_while_hidden(&self) -> bool {
        self.flight.is_some()
    }
}

#[derive(Clone, Copy)]
struct Painted {
    rect: euclid::Rect<f32, window::PixelUnit>,
    fill: LinearRgba,
    edge: LinearRgba,
    shadow: LinearRgba,
    icon_centre: (f32, f32),
    icon_color: LinearRgba,
}

struct Flight {
    started: Instant,
    from: Painted,
    to: Option<(f32, f32, f32)>,
}

impl crate::TermWindow {
    /// Whether the capsule painted this frame. The download cards stack above
    /// it when both share the bottom-right corner.
    pub(crate) fn update_capsule_visible(&self) -> bool {
        self.update_capsule.painted.is_some()
    }

    pub(crate) fn paint_update_capsule(&mut self) -> anyhow::Result<()> {
        let now = Instant::now();
        let gear_dot = self.update_capsule.gear_dot_slot.take();
        self.update_capsule.gear_dot = gear_dot;
        if self.update_capsule.flight.is_some() {
            return self.paint_update_capsule_flight(now);
        }
        let status = crate::settings_window::update_capsule_status();
        // Out of the way while a menu, the palette or a dialog is up, and
        // while the right sidebar is slid out over the corner.
        let hidden = dismissed()
            || self.context_menu.is_some()
            || self.command_palette.is_some()
            || self.get_modal().is_some()
            || (self.right_sidebar_collapsed && self.right_sidebar_presented());
        let Some(status) = status.filter(|_| !hidden) else {
            let capsule = &mut self.update_capsule;
            if status_gone(&capsule.kind, hidden) {
                *capsule = UpdateCapsule {
                    gear_dot: capsule.gear_dot,
                    ..UpdateCapsule::default()
                };
            }
            capsule.painted = None;
            capsule.animating = false;
            return Ok(());
        };

        let settings = crate::native_settings::load_shared();
        let font_size = crate::native_settings::home_font_size(&settings);
        let main_font = self
            .fonts
            .title_font_with_size_and_weight(font_size, 500)
            .context("update capsule font")?;
        let sub_font = self
            .fonts
            .title_font_with_size_and_weight(font_size, 400)
            .context("update capsule font")?;
        let metrics = RenderMetrics::with_font_metrics(&main_font.metrics());
        let line = metrics.cell_size.height as f32;

        let kind = Kind::of(&status);
        let (main, sub, sub_room) = match &status {
            UpdateCapsuleStatus::Available { version, new_build } => (
                crate::i18n::tr(if *new_build {
                    "update-capsule-new-build"
                } else {
                    "update-capsule-available"
                }),
                version.clone(),
                None,
            ),
            UpdateCapsuleStatus::Installing {
                fraction: Some(fraction),
            } => (
                crate::i18n::tr("update-capsule-downloading"),
                format!("{}%", (fraction * 100.0).floor() as u32),
                // The percentage changes every report; room for the widest
                // keeps the capsule from twitching as it counts.
                Some("100%"),
            ),
            UpdateCapsuleStatus::Installing { fraction: None } => (
                crate::i18n::tr("update-capsule-installing"),
                String::new(),
                None,
            ),
            UpdateCapsuleStatus::Installed { restart: true } => (
                crate::i18n::tr("update-capsule-restart"),
                String::new(),
                None,
            ),
            UpdateCapsuleStatus::Installed { restart: false } => (
                crate::i18n::tr("update-capsule-installed"),
                String::new(),
                None,
            ),
            UpdateCapsuleStatus::Failed => (
                crate::i18n::tr("update-capsule-failed"),
                crate::i18n::tr("update-capsule-retry"),
                None,
            ),
        };

        let main_width = self
            .cached_ui_text_advance(&main_font, &metrics, &main)?
            .ceil();
        let sub_width = match sub_room {
            Some(room) => self.cached_ui_text_advance(&sub_font, &metrics, room)?,
            None => self.cached_ui_text_advance(&sub_font, &metrics, &sub)?,
        }
        .ceil();
        let scale = self.ui_f32(1.0);
        let px = move |value: f32| value * POINT * scale;
        let natural = px(PAD_LEFT + ICON + GAP)
            + main_width
            + if sub_width > 0.0 {
                px(WORD_GAP) + sub_width
            } else {
                0.0
            }
            + px(PAD_RIGHT);

        // Where it sits: the same distance from the bottom and the right of
        // the window's terminal area, left of the right sidebar; above the
        // quote line when that is on, as it is drawn in that corner.
        let (_, padding_top) = self.padding_left_top();
        let border = self.get_os_border();
        let (top_tab_height, bottom_tab_height) = self.pane_area_insets();
        let content_bottom =
            self.dimensions.pixel_height as f32 - border.bottom.get() as f32 - bottom_tab_height;
        let grid_top = border.top.get() as f32 + top_tab_height + padding_top;
        let grid_bottom = grid_top + self.terminal_size.pixel_height as f32;
        let floor = if settings.terminal.bottom_quote_enabled {
            grid_bottom
        } else {
            content_bottom
        };

        let hovered = self
            .update_capsule
            .painted
            .is_some_and(|painted| pointer_over(self, painted.rect));

        let capsule = &mut self.update_capsule;
        let shown_at = *capsule.shown_at.get_or_insert(now);
        if capsule.kind != Some(kind) {
            if capsule.kind.is_some() {
                capsule.changed_at = Some(now);
            }
            capsule.kind = Some(kind);
        }
        // Under the pointer it only grows: shrinking would pull its edge out
        // from under the pointer, which then closes the close button and
        // shrinks it again. It takes its own size once the pointer leaves.
        let natural = if hovered && capsule.width.is_some() {
            natural.max(capsule.width_target)
        } else {
            natural
        };
        match capsule.width.as_mut() {
            Some(width) if (capsule.width_target - natural).abs() > 0.5 => {
                width.retarget(now, natural, RESIZE, Easing::OutCubic);
                capsule.width_target = natural;
            }
            Some(_) => {}
            None => {
                capsule.width = Some(Timeline::settled(now, natural));
                capsule.width_target = natural;
            }
        }
        // Open in every state: one shutting as a click starts the download
        // would shrink the capsule under the pointer that is still on it.
        // Closed during the download, the install carries on and the dot
        // waits on the gear.
        let open = hovered;
        if capsule.close.is_none() {
            capsule.close = Some(Timeline::settled(now, 0.0));
        }
        if open != capsule.close_open {
            capsule.close_open = open;
            if let Some(close) = capsule.close.as_mut() {
                close.retarget(
                    now,
                    if open { 1.0 } else { 0.0 },
                    Duration::from_millis(240),
                    Easing::OutCubic,
                );
            }
        }
        let mut animating = false;
        if let Some(width) = capsule.width.as_mut() {
            animating |= width.advance(now);
        }
        if let Some(close) = capsule.close.as_mut() {
            animating |= close.advance(now);
        }
        let base_width = capsule.width.as_ref().map_or(natural, |w| w.value(now));
        let close_open = capsule
            .close
            .as_ref()
            .map_or(0.0, |c| c.value(now))
            .clamp(0.0, 1.0);
        let changed_at = capsule.changed_at;
        let pressed = match capsule.pressed_at {
            Some(at) if now.duration_since(at) < PRESS => {
                animating = true;
                true
            }
            _ => false,
        };

        let height = px(HEIGHT);
        let width = base_width + px(CLOSE + GAP) * close_open;
        let right = self.dimensions.pixel_width as f32
            - border.right.get() as f32
            - self.right_sidebar_width() as f32
            - px(MARGIN);
        let bottom = floor - px(MARGIN);

        // Arrival: up from below, overshooting a touch, fading in.
        let arrive = (now.duration_since(shown_at).as_secs_f32() / ARRIVE.as_secs_f32()).min(1.0);
        animating |= arrive < 1.0;
        let rise = (1.0 - back_out(arrive)) * px(18.0);
        let alpha = Easing::Smooth.apply((arrive * 1.6).min(1.0));
        // A failure shakes it once.
        let shake = match (kind, changed_at) {
            (Kind::Failed, Some(at)) if now.duration_since(at) < SHAKE => {
                animating = true;
                shake_offset(now.duration_since(at).as_secs_f32() / SHAKE.as_secs_f32()) * px(1.0)
            }
            _ => 0.0,
        };
        let rect = euclid::rect(
            (right - width + shake).round(),
            (bottom - height + rise).round(),
            width.round(),
            height,
        );

        let chrome = self.chrome();
        let mut card = chrome.as_menu_card();
        crate::native_settings::apply_ui_colors(&mut card, self.config.ui_colors.as_ref());
        let dark = card.is_dark();
        let white = LinearRgba::with_srgba(255, 255, 255, 255);
        let (fill, edge, text, muted) = if kind == Kind::Restart {
            (
                if pressed {
                    crate::ui::color::mix(chrome.accent, LinearRgba::with_srgba(0, 0, 0, 255), 0.18)
                } else if hovered {
                    chrome.accent_hover
                } else {
                    chrome.accent
                },
                LinearRgba::with_components(0.0, 0.0, 0.0, 0.0),
                white,
                white.mul_alpha(0.72),
            )
        } else {
            (
                // The menus' hover colour is a wash meant to lie over their
                // card; on its own it would let the terminal show through.
                if pressed {
                    crate::ui::color::mix(card.control_bg, card.text, 0.16)
                } else if hovered {
                    crate::ui::color::mix(card.control_bg, card.text, 0.08)
                } else {
                    card.control_bg
                },
                card.control_border,
                card.text,
                card.muted_text,
            )
        };
        let icon_bg = match kind {
            Kind::Available | Kind::Installed => chrome.accent,
            Kind::Installing => fill,
            Kind::Restart => white.mul_alpha(0.22),
            Kind::Failed => chrome.danger,
        };
        let sub_color = if kind == Kind::Failed {
            chrome.accent
        } else {
            muted
        };

        // The sweep, while it runs: its centre across the capsule, 0 to 1.
        let sweep = if kind == Kind::Available {
            let since = now.duration_since(shown_at);
            let end = SWEEP_DELAY + SWEEP * SWEEPS;
            if since < end {
                animating = true;
            }
            since
                .checked_sub(SWEEP_DELAY)
                .filter(|after| *after < SWEEP * SWEEPS)
                .map(|after| {
                    let t = (after.as_secs_f32() % SWEEP.as_secs_f32()) / SWEEP.as_secs_f32();
                    Easing::Smooth.apply(t)
                })
        } else {
            None
        };

        let gl_state = self
            .render_state
            .as_ref()
            .context("update capsule render state")?;
        let layer = gl_state
            .layer_for_zindex(ZINDEX)
            .context("update capsule layer")?;
        let mut layers = layer.quad_allocator();
        let ctx = DrawContext::new(gl_state, self.dimensions, &metrics);
        let radius = height / 2.0;
        let shadow = LinearRgba::with_components(0.0, 0.0, 0.0, if dark { 0.58 } else { 0.43 });
        ctx.draw_elevated_surface(
            &mut layers,
            0,
            rect,
            fill.mul_alpha(alpha),
            edge.mul_alpha(alpha),
            shadow.mul_alpha(alpha),
            radius,
        )
        .context("update capsule surface")?;

        if let Some(at) = sweep {
            let band = rect.size.width * 0.45;
            let centre = rect.origin.x - band / 2.0 + (rect.size.width + band) * at;
            let peak = if dark {
                white.mul_alpha(0.16)
            } else {
                chrome.accent.mul_alpha(0.14)
            };
            paint_sweep(&ctx, &mut layers, rect, centre, band, peak.mul_alpha(alpha))?;
        }

        let icon_size = px(ICON);
        let icon_cx = rect.origin.x + px(PAD_LEFT) + icon_size / 2.0;
        let icon_cy = rect.origin.y + height / 2.0;
        ctx.draw_disc(
            &mut layers,
            1,
            icon_cx,
            icon_cy,
            icon_size,
            icon_bg.mul_alpha(alpha),
        )
        .context("update capsule icon")?;
        let glyph = px(12.0);
        match &status {
            UpdateCapsuleStatus::Installing { fraction } => {
                let turn = match fraction {
                    Some(_) => 0.0,
                    // Without a fraction a quarter of the ring turns, on
                    // the spinners' clock: it asks for no frames of its own.
                    None => {
                        self.request_spinner_frame();
                        let frames = crate::termwindow::ui::status_icon::spinner_frames();
                        (frames % SPIN_STEPS) as f32 / SPIN_STEPS as f32
                    }
                };
                paint_ring(
                    &ctx,
                    &mut layers,
                    (icon_cx, icon_cy),
                    px(RING_RADIUS),
                    px(RING_WIDTH),
                    *fraction,
                    turn,
                    muted.mul_alpha(0.35 * alpha),
                    chrome.accent.mul_alpha(alpha),
                    fill.mul_alpha(alpha),
                )?;
                ctx.draw_svg_icon(
                    &mut layers,
                    SvgIcon::ArrowDown,
                    icon_cx - glyph / 2.0,
                    icon_cy - glyph / 2.0,
                    glyph,
                    chrome.accent.mul_alpha(alpha),
                )?;
            }
            UpdateCapsuleStatus::Failed => {
                // "!": a bar and a dot.
                let bar = px(2.2);
                self.fill_rounded_rectangle(
                    &mut layers,
                    2,
                    euclid::rect(icon_cx - bar / 2.0, icon_cy - px(5.5), bar, px(6.5)),
                    white.mul_alpha(alpha),
                    bar / 2.0,
                )?;
                ctx.draw_disc(
                    &mut layers,
                    2,
                    icon_cx,
                    icon_cy + px(4.0),
                    px(2.6),
                    white.mul_alpha(alpha),
                )?;
            }
            UpdateCapsuleStatus::Available { .. } | UpdateCapsuleStatus::Installed { .. } => {
                let icon = if kind == Kind::Restart {
                    SvgIcon::RotateCw
                } else {
                    SvgIcon::ArrowDown
                };
                ctx.draw_svg_icon(
                    &mut layers,
                    icon,
                    icon_cx - glyph / 2.0,
                    icon_cy - glyph / 2.0,
                    glyph,
                    white.mul_alpha(alpha),
                )?;
            }
        }

        // The words, faded in again after a change.
        let words_alpha = alpha
            * changed_at.map_or(1.0, |at| {
                let t = now.duration_since(at).as_secs_f32() / 0.18;
                animating |= t < 1.0;
                Easing::Smooth.apply(t.min(1.0))
            });
        let text_x = rect.origin.x + px(PAD_LEFT + ICON + GAP);
        let text_y = (icon_cy - line / 2.0).round();
        self.paint_agent_text(
            &mut layers,
            &main_font,
            metrics,
            &main,
            text_x.round() as usize,
            text_y as usize,
            // Room for the whole word: the capsule is sized to it, and a limit
            // that rounding can shave a pixel off fades its last letter.
            (main_width + px(2.0)) as usize,
            text.mul_alpha(words_alpha),
        )?;
        if !sub.is_empty() {
            let sub_x = text_x + main_width + px(WORD_GAP);
            self.paint_agent_text(
                &mut layers,
                &sub_font,
                metrics,
                &sub,
                sub_x.round() as usize,
                text_y as usize,
                (sub_width + px(2.0)) as usize,
                sub_color.mul_alpha(words_alpha),
            )?;
        }

        let close_size = px(13.0);
        let close_rect = euclid::rect(
            rect.max_x() - px(PAD_RIGHT) - px(CLOSE),
            icon_cy - px(CLOSE) / 2.0,
            px(CLOSE),
            px(CLOSE),
        );
        let close_shown = close_open > 0.01;
        let close_hovered = close_shown && pointer_over(self, close_rect);
        if close_shown {
            ctx.draw_svg_icon(
                &mut layers,
                SvgIcon::X,
                close_rect.origin.x + (px(CLOSE) - close_size) / 2.0,
                icon_cy - close_size / 2.0,
                close_size,
                (if close_hovered { text } else { muted }).mul_alpha(close_open * alpha),
            )?;
        }
        drop(layers);

        self.update_capsule.painted = Some(Painted {
            rect,
            fill,
            edge,
            shadow,
            icon_centre: (icon_cx, icon_cy),
            icon_color: match kind {
                Kind::Installing | Kind::Restart => chrome.accent,
                _ => icon_bg,
            },
        });
        // Clickable once it has arrived: until then a click is meant for
        // whatever was in the corner, and goes there.
        if arrive >= 1.0 {
            self.ui_items.push(ui_item(rect, UIItemType::UpdateCapsule));
            // The close button takes its clicks from the moment it shows.
            if close_shown {
                self.ui_items
                    .push(ui_item(close_rect, UIItemType::UpdateCapsuleDismiss));
            }
        }
        self.update_capsule.animating = animating;
        if animating {
            self.update_next_frame_time(Some(now + FRAME));
        }
        Ok(())
    }

    /// The close button: fold into the icon and fly it into the gear.
    pub(crate) fn dismiss_update_capsule(&mut self) {
        let capsule = &mut self.update_capsule;
        let Some(painted) = capsule.painted else {
            DISMISSED.with(|dismissed| dismissed.set(true));
            crate::frontend::front_end().invalidate_all_windows();
            return;
        };
        capsule.flight = Some(Flight {
            started: Instant::now(),
            from: painted,
            to: capsule.gear_dot,
        });
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    /// Whether a click on the capsule would do anything: nothing does while
    /// the update installs.
    pub(crate) fn update_capsule_clickable(&self) -> bool {
        self.update_capsule.kind != Some(Kind::Installing)
    }

    /// What the capsule's click does in what it shows.
    pub(crate) fn activate_update_capsule(&mut self) {
        let now = Instant::now();
        let status = crate::settings_window::update_capsule_status();
        let capsule = &self.update_capsule;
        // It acts on what was seen: not on a state that came since the last
        // paint, nor on one only just painted.
        let seen = status.as_ref().map(Kind::of) == capsule.kind;
        let settled = capsule
            .changed_at
            .is_none_or(|at| now.duration_since(at) >= SETTLE);
        let Some(status) = status.filter(|_| seen && settled) else {
            return;
        };
        if Kind::of(&status) == Kind::Installing {
            return;
        }
        self.update_capsule.pressed_at = Some(now);
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
        let acted = match status {
            UpdateCapsuleStatus::Available { .. } | UpdateCapsuleStatus::Failed
                if crate::settings_window::installs_updates_itself() =>
            {
                crate::settings_window::start_update_install()
            }
            UpdateCapsuleStatus::Installed { restart: true } => {
                crate::settings_window::restart_into_update()
            }
            // A copy a package manager owns, or one whose restart would end
            // terminals: the Update page says what to do instead.
            _ => false,
        };
        if !acted {
            crate::settings_window::show_update_page_from(
                self.mux_window_id,
                &self.active_space_id,
            );
        }
    }

    fn paint_update_capsule_flight(&mut self, now: Instant) -> anyhow::Result<()> {
        let Some(flight) = self.update_capsule.flight.as_ref() else {
            return Ok(());
        };
        let elapsed = now.duration_since(flight.started);
        let (from, to) = (flight.from, flight.to);
        let scale = self.ui_f32(1.0);
        let px = move |value: f32| value * POINT * scale;
        let settings = crate::native_settings::load_shared();
        let font = self
            .fonts
            .title_font_with_size(crate::native_settings::home_font_size(&settings))
            .context("update capsule font")?;
        let metrics = RenderMetrics::with_font_metrics(&font.metrics());
        let gl_state = self
            .render_state
            .as_ref()
            .context("update capsule render state")?;
        let layer = gl_state
            .layer_for_zindex(ZINDEX)
            .context("update capsule layer")?;
        let mut layers = layer.quad_allocator();
        let ctx = DrawContext::new(gl_state, self.dimensions, &metrics);

        let icon = px(ICON);
        let (cx, cy) = from.icon_centre;
        if elapsed >= FOLD + FLIGHT {
            // Landed. The sidebar painted this frame before the capsule
            // was dismissed, still without its dot: this frame the disc
            // stands in for it, and from the next the sidebar draws it.
            if let Some((x, y, diameter)) = to {
                ctx.draw_disc(&mut layers, 1, x, y, diameter, from.icon_color)?;
            }
            drop(layers);
            self.update_capsule = UpdateCapsule {
                gear_dot: self.update_capsule.gear_dot,
                ..UpdateCapsule::default()
            };
            DISMISSED.with(|dismissed| dismissed.set(true));
            crate::frontend::front_end().invalidate_all_windows();
            return Ok(());
        }
        if elapsed < FOLD {
            // The surface closes in on the icon and fades away.
            let t = Easing::OutCubic.apply(elapsed.as_secs_f32() / FOLD.as_secs_f32());
            let rect = from.rect;
            let side = rect.size.height;
            let folded: euclid::Rect<f32, window::PixelUnit> = euclid::rect(
                rect.origin.x + (cx - side / 2.0 - rect.origin.x) * t,
                rect.origin.y,
                rect.size.width + (side - rect.size.width) * t,
                side,
            );
            ctx.draw_elevated_surface(
                &mut layers,
                0,
                folded,
                from.fill.mul_alpha(1.0 - t),
                from.edge.mul_alpha(1.0 - t),
                from.shadow.mul_alpha(1.0 - t),
                side / 2.0,
            )?;
            ctx.draw_disc(&mut layers, 1, cx, cy, icon, from.icon_color)?;
        } else {
            let t = (elapsed - FOLD).as_secs_f32() / FLIGHT.as_secs_f32();
            let (x, y, diameter, alpha) = match to {
                Some((to_x, to_y, dot)) => {
                    let along = Easing::Smooth.apply(t);
                    // A toss: up and over, then down into the dot.
                    let lift = px(90.0) * (std::f32::consts::PI * along).sin();
                    let diameter = icon + (dot - icon) * Easing::OutCubic.apply(t);
                    (
                        cx + (to_x - cx) * along,
                        cy + (to_y - cy) * along - lift,
                        diameter,
                        1.0,
                    )
                }
                // No gear on screen: it shrinks away where it is.
                None => (cx, cy, icon * (1.0 - t), 1.0 - t),
            };
            ctx.draw_disc(
                &mut layers,
                1,
                x,
                y,
                diameter,
                from.icon_color.mul_alpha(alpha),
            )?;
        }
        drop(layers);
        self.update_next_frame_time(Some(now + FRAME));
        Ok(())
    }
}

/// The spinners' frames to one turn of the ring.
const SPIN_STEPS: u128 = 12;

fn ui_item(rect: euclid::Rect<f32, window::PixelUnit>, item_type: UIItemType) -> UIItem {
    UIItem {
        x: rect.origin.x.max(0.0) as usize,
        y: rect.origin.y.max(0.0) as usize,
        width: rect.size.width as usize,
        height: rect.size.height as usize,
        item_type,
    }
}

fn pointer_over(window: &crate::TermWindow, rect: euclid::Rect<f32, window::PixelUnit>) -> bool {
    window.is_pointer_over_ui_rect(
        rect.origin.x.max(0.0) as usize,
        rect.origin.y.max(0.0) as usize,
        rect.size.width as usize,
        rect.size.height as usize,
    )
}

/// Whether a capsule that is not painted this frame should start over:
/// it went away rather than being hidden for a moment under a menu.
fn status_gone(kind: &Option<Kind>, hidden: bool) -> bool {
    kind.is_some() && !hidden
}

/// Ease-out with an overshoot of about a tenth.
fn back_out(t: f32) -> f32 {
    let (c1, c3) = (1.2_f32, 2.2_f32);
    let u = t - 1.0;
    1.0 + c3 * u * u * u + c1 * u * u
}

/// The shake's offset, in points, at `t` from 0 to 1.
fn shake_offset(t: f32) -> f32 {
    let keys = [
        (0.0, 0.0),
        (0.15, -1.0),
        (0.3, 3.0),
        (0.45, -4.0),
        (0.55, -4.0),
        (0.7, 3.0),
        (0.85, -1.0),
        (1.0, 0.0),
    ];
    for pair in keys.windows(2) {
        let ((t0, v0), (t1, v1)) = (pair[0], pair[1]);
        if t <= t1 {
            let f = if t1 > t0 { (t - t0) / (t1 - t0) } else { 1.0 };
            return v0 + (v1 - v0) * f;
        }
    }
    0.0
}

/// A soft vertical band of light centred at `centre`, `band` wide, cut to
/// the capsule's rounded ends.
fn paint_sweep(
    ctx: &DrawContext,
    layers: &mut crate::quad::TripleLayerQuadAllocator,
    rect: euclid::Rect<f32, window::PixelUnit>,
    centre: f32,
    band: f32,
    peak: LinearRgba,
) -> anyhow::Result<()> {
    let radius = rect.size.height / 2.0;
    let cy = rect.origin.y + radius;
    let left_cap = rect.origin.x + radius;
    let right_cap = rect.max_x() - radius;
    let slice = ctx.px(2.0).max(1.0);
    let mut x = (centre - band / 2.0).max(rect.origin.x);
    let end = (centre + band / 2.0).min(rect.max_x());
    while x < end {
        let mid = x + slice / 2.0;
        let off = if mid < left_cap {
            left_cap - mid
        } else if mid > right_cap {
            mid - right_cap
        } else {
            0.0
        };
        if off < radius {
            let half = (radius * radius - off * off).sqrt();
            let d = (mid - centre) / (band / 2.0);
            let strength = (1.0 - d * d).max(0.0).powi(2);
            ctx.draw_rect(
                layers,
                1,
                x,
                cy - half,
                slice.min(end - x),
                half * 2.0,
                peak.mul_alpha(strength),
            )?;
        }
        x += slice;
    }
    Ok(())
}

/// The ring around the icon: a faint track, the downloaded part over it
/// from twelve o'clock, then the surface's colour inside to cut the ring's
/// inner edge clean. Without a fraction, a quarter of it at `turn` (0 to 1).
#[allow(clippy::too_many_arguments)]
fn paint_ring(
    ctx: &DrawContext,
    layers: &mut crate::quad::TripleLayerQuadAllocator,
    (cx, cy): (f32, f32),
    radius: f32,
    width: f32,
    fraction: Option<f32>,
    turn: f32,
    track: LinearRgba,
    arc: LinearRgba,
    inside: LinearRgba,
) -> anyhow::Result<()> {
    use std::f32::consts::TAU;
    ctx.draw_disc(layers, 1, cx, cy, (radius + width / 2.0) * 2.0, track)?;
    let (start, sweep) = match fraction {
        Some(fraction) => (0.0, fraction * TAU),
        None => (turn * TAU, TAU / 4.0),
    };
    if sweep > 0.0 {
        let steps = ((radius * sweep) / ctx.px(0.75).max(0.5)).ceil().max(1.0) as usize;
        for step in 0..=steps {
            let angle = start + sweep * step as f32 / steps as f32 - TAU / 4.0;
            ctx.draw_disc(
                layers,
                1,
                cx + radius * angle.cos(),
                cy + radius * angle.sin(),
                width,
                arc,
            )?;
        }
    }
    ctx.draw_disc(layers, 1, cx, cy, (radius - width / 2.0) * 2.0, inside)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_arrival_overshoots_a_little_and_settles() {
        assert!(back_out(0.0).abs() < 1e-6);
        assert!((back_out(1.0) - 1.0).abs() < 1e-6);
        let peak = (0..=100)
            .map(|i| back_out(i as f32 / 100.0))
            .fold(0.0, f32::max);
        assert!(peak > 1.02 && peak < 1.15, "{}", peak);
    }

    #[test]
    fn the_shake_starts_and_ends_at_rest() {
        assert_eq!(shake_offset(0.0), 0.0);
        assert_eq!(shake_offset(1.0), 0.0);
        assert!(shake_offset(0.45).abs() > 3.0);
    }

    #[test]
    fn the_capsule_reads_each_status_as_its_kind() {
        let version = || "1.2.3".to_string();
        assert_eq!(
            Kind::of(&UpdateCapsuleStatus::Available {
                version: version(),
                new_build: false,
            }),
            Kind::Available
        );
        assert_eq!(
            Kind::of(&UpdateCapsuleStatus::Installing { fraction: None }),
            Kind::Installing
        );
        assert_eq!(
            Kind::of(&UpdateCapsuleStatus::Installed { restart: true }),
            Kind::Restart
        );
        assert_eq!(
            Kind::of(&UpdateCapsuleStatus::Installed { restart: false }),
            Kind::Installed
        );
        assert_eq!(Kind::of(&UpdateCapsuleStatus::Failed), Kind::Failed);
    }
}
