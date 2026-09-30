use window::color::LinearRgba;
use window::Appearance;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct UiPalette {
    pub window_bg: LinearRgba,
    pub sidebar_bg: LinearRgba,
    pub workspace_sidebar_bg: LinearRgba,
    pub header_bg: LinearRgba,
    pub separator: LinearRgba,
    pub control_bg: LinearRgba,
    pub control_hover_bg: LinearRgba,
    pub control_pressed_bg: LinearRgba,
    pub control_border: LinearRgba,
    pub sidebar_button_bg: LinearRgba,
    pub sidebar_button_hover_bg: LinearRgba,
    /// The sidebar row ramp. These three are ordered on purpose --
    /// hover < pressed < active -- because a selected row is a state and the
    /// other two are momentary feedback; feedback that outweighs the state
    /// makes the row under the pointer look more current than the one that is.
    /// Write them as concrete colors, never as a white wash: a wash composites
    /// in linear space, so 7.5% white over the dark bar landed at rgb(80) and
    /// silently jumped the whole ramp.
    pub sidebar_row_hover_bg: LinearRgba,
    pub sidebar_row_pressed_bg: LinearRgba,
    pub sidebar_row_active_bg: LinearRgba,
    pub sidebar_row_active_border: LinearRgba,
    pub selected_bg: LinearRgba,
    /// The one saturated color in the chrome. Everything else is a neutral,
    /// so this is what a selected row, an active switch or a primary button
    /// uses to say "this one". Kept identical in shape across platforms --
    /// the whole UI is drawn from these tokens, so one edit moves every OS.
    pub accent: LinearRgba,
    pub accent_hover: LinearRgba,
    /// Text and glyphs sitting on top of `accent`.
    pub on_accent: LinearRgba,
    /// Irreversible actions. Used as a label/border tint rather than a fill,
    /// so a destructive button still reads as a button and not as an alert.
    pub danger: LinearRgba,
    /// The off half of a switch track. Distinct from `control_border`, which
    /// is a hairline colour and disappears when used as a filled track.
    pub track_off: LinearRgba,
    /// Fill for a grouped card floating on `window_bg`. Translucent on
    /// purpose: it picks up whatever the page paints behind it.
    pub card_bg: LinearRgba,
    /// Fill for a group of rows inside a card, the way System Settings
    /// groups them: a step off `card_bg`, below `control_bg` so the
    /// controls in the rows still stand out. A concrete colour, for the
    /// reason the sidebar row ramp gives.
    pub group_bg: LinearRgba,
    pub text: LinearRgba,
    pub secondary_text: LinearRgba,
    pub muted_text: LinearRgba,
    pub selected_text: LinearRgba,
    pub scrollbar_thumb: LinearRgba,
    pub spelling_error: LinearRgba,
    /// Whether these colours were derived from a terminal colour scheme
    /// rather than being the hand-tuned set.
    ///
    /// The menus tune their own card on top of the chrome with fixed neutral
    /// greys, which is right for the two hand-tuned palettes and wrong for a
    /// derived one: it would drop a cold grey card next to a warm sidebar.
    pub derived: bool,
    /// The appearance these colours were resolved for. Carried so a palette
    /// answers for itself: callers that tune it further (the context and
    /// command menus) would otherwise have to be handed the appearance
    /// alongside, and re-deriving it is not free.
    pub appearance: Appearance,
}

/// The chrome surfaces a derived palette moves onto the terminal's ground,
/// as one list so the span that fits them into the range and the walk that
/// moves them cannot disagree. The accent family and the text slots are
/// deliberately absent: accents are kept as designed, and text is moved and
/// then held to a contrast floor, which is a different rule.
const SURFACE_SLOTS: [fn(&mut UiPalette) -> &mut LinearRgba; 19] = [
    |p| &mut p.window_bg,
    |p| &mut p.sidebar_bg,
    |p| &mut p.workspace_sidebar_bg,
    |p| &mut p.header_bg,
    |p| &mut p.separator,
    |p| &mut p.control_bg,
    |p| &mut p.control_hover_bg,
    |p| &mut p.control_pressed_bg,
    |p| &mut p.control_border,
    |p| &mut p.sidebar_button_bg,
    |p| &mut p.sidebar_button_hover_bg,
    |p| &mut p.sidebar_row_hover_bg,
    |p| &mut p.sidebar_row_pressed_bg,
    |p| &mut p.sidebar_row_active_bg,
    |p| &mut p.sidebar_row_active_border,
    |p| &mut p.track_off,
    |p| &mut p.card_bg,
    |p| &mut p.group_bg,
    |p| &mut p.scrollbar_thumb,
];

impl UiPalette {
    /// Whether these are the dark colours. The high-contrast appearances
    /// collapse onto the two palettes, so this is the only distinction the
    /// chrome makes.
    pub(crate) fn is_dark(&self) -> bool {
        matches!(
            self.appearance,
            Appearance::Dark | Appearance::DarkHighContrast
        )
    }

    /// Whether a colour is one to write light text on or dark.
    ///
    /// The threshold is on perceptual lightness rather than on the WCAG
    /// relative luminance: mid greens read far lighter than their luminance
    /// says, and a scheme sitting near the line should break the way a person
    /// would call it.
    pub(crate) fn appearance_of(ground: LinearRgba) -> Appearance {
        if ground.to_oklaba()[0] < 0.5 {
            Appearance::Dark
        } else {
            Appearance::Light
        }
    }

    /// The chrome's colours rebuilt around a terminal colour scheme, so the
    /// interface reads as part of the same picture as the terminal.
    ///
    /// Not a fresh design: it takes the palette already tuned for this
    /// appearance and *moves* it onto `ground`, keeping every lightness
    /// relationship that palette encodes -- which surface sits above which,
    /// by how much, at what alpha -- and taking the scheme's tint. Inventing
    /// a new set of relationships per scheme is how a thousand schemes become
    /// a thousand chances to look wrong.
    ///
    /// The accent family is deliberately left alone. A scheme's own blue is
    /// frequently unreadable against its own background, and an accent is the
    /// one slot where being wrong is loud.
    pub(crate) fn for_scheme(ground: LinearRgba) -> Self {
        // The base has to be the side `ground` is on, and is therefore read
        // off `ground` rather than taken as an argument. Handing in the other
        // one inverts the design: the light palette states its hover as dark
        // ink at low alpha, so moved onto a dark ground it becomes black over
        // black -- a hover that is darker than what it is hovering over.
        let base = Self::for_appearance(Self::appearance_of(ground));
        let base_ground = base.window_bg;
        let [base_l, ..] = base_ground.to_oklaba();
        let [ground_l, ground_a, ground_b, _] = ground.to_oklaba();

        // Every surface keeps its distance from the ground it was drawn
        // against, and takes the ground's own tint, so a warm scheme gets warm
        // chrome rather than the same grey on a different backdrop.
        //
        // The whole ramp is slid to fit inside the range first. Clamping each
        // surface on its own instead is what a white or near-white scheme used
        // to get: every raised surface pinned to the same white, so a control,
        // its hover state and a card were one colour and hovering showed
        // nothing. The offsets are what carry the design, so they are kept
        // whole and the ground gives way -- a pure-white terminal gets a
        // faintly grey sidebar, which is the readable half of that trade.
        let surface_span = {
            let mut probe = base;
            SURFACE_SLOTS
                .iter()
                .map(|slot| slot(&mut probe).to_oklaba()[0] - base_l)
                .fold((0.0f32, 0.0f32), |(lo, hi), d| (lo.min(d), hi.max(d)))
        };
        let fit = {
            let (lo, hi) = surface_span;
            let over = (ground_l + hi - 1.0).max(0.0);
            let under = (0.0 - (ground_l + lo)).max(0.0);
            // A span wider than the whole range cannot be fitted by sliding;
            // no palette of ours is, and losing the top beats losing the
            // ordering of everything below it.
            under - over
        };
        let moved = |color: LinearRgba| -> LinearRgba {
            let [l, _, _, alpha] = color.to_oklaba();
            let shifted = (ground_l + fit + (l - base_l)).clamp(0.0, 1.0);
            let out = LinearRgba::from_oklaba(shifted, ground_a, ground_b, alpha);
            LinearRgba::with_components(
                out.0.clamp(0.0, 1.0),
                out.1.clamp(0.0, 1.0),
                out.2.clamp(0.0, 1.0),
                alpha,
            )
        };

        let mut palette = base;
        for slot in SURFACE_SLOTS {
            let value = slot(&mut palette);
            *value = moved(*value);
        }

        // Text is moved the same way and then held to a minimum contrast
        // against the surface it is read on. A scheme whose foreground is
        // close to its background would otherwise produce a sidebar whose
        // secondary text is a rumour.
        //
        // Against the *worst* of the surfaces it lands on, not just the
        // window's. The Spaces strip is recessed -- derived darker than the
        // window on a light scheme -- so text held to exactly its floor
        // against `window_bg` sits under that floor on the strip, which is
        // where `sidebar.rs` and `right_sidebar.rs` actually draw the muted
        // and secondary rows. Moving further from the hardest of them moves
        // further from the rest: they are all on the same side of the text.
        let surfaces = [
            palette.window_bg,
            palette.sidebar_bg,
            palette.workspace_sidebar_bg,
        ];
        let hardest = |text: LinearRgba| -> LinearRgba {
            let mut worst = surfaces[0];
            let mut worst_ratio = text.contrast_ratio(&worst);
            for surface in &surfaces[1..] {
                let ratio = text.contrast_ratio(surface);
                if ratio < worst_ratio {
                    worst = *surface;
                    worst_ratio = ratio;
                }
            }
            worst
        };
        palette.text = moved(base.text);
        if let Some(fixed) = palette.text.ensure_contrast_ratio(&hardest(palette.text), 7.0) {
            palette.text = fixed;
        }
        palette.secondary_text = moved(base.secondary_text);
        if let Some(fixed) = palette
            .secondary_text
            .ensure_contrast_ratio(&hardest(palette.secondary_text), 4.5)
        {
            palette.secondary_text = fixed;
        }
        palette.muted_text = moved(base.muted_text);
        if let Some(fixed) = palette
            .muted_text
            .ensure_contrast_ratio(&hardest(palette.muted_text), 3.0)
        {
            palette.muted_text = fixed;
        }

        palette.derived = true;
        palette
    }

    /// The raised tab, given that the strip beneath it is the terminal's own
    /// ground rather than a piece of sidebar.
    ///
    /// `control_bg` is near-white, which *is* the strip on a light scheme --
    /// the tab would disappear into it. The chrome's own surface is what the
    /// strip used to be, so using it here puts the two back on either side of
    /// each other, with the tab reading as raised. When the two are already
    /// the same colour (a dark interface, where the terminal is painted with
    /// the chrome's colour) nothing has moved and the old surface is still
    /// right.
    ///
    /// One method rather than one per strip: the window tabs and the pane nav
    /// tabs are the same control drawn twice, and they looked it until one of
    /// them was changed alone.
    pub(crate) fn active_tab_surface(&self) -> LinearRgba {
        if self.header_bg == self.sidebar_bg {
            self.control_bg
        } else {
            self.sidebar_bg
        }
    }

    /// The same palette tuned for a floating card -- the context menu and the
    /// command palette both sit on one, and both want the platform's menu
    /// greys rather than the sidebar's.
    ///
    /// One function because they were two copies of the same eight values and
    /// drifted the moment either was retuned.
    pub(crate) fn as_menu_card(mut self) -> Self {
        if self.derived {
            // The chrome was built from the terminal's scheme; the neutral
            // greys below would undo exactly the tint that was asked for. The
            // card reads as a card through its border and shadow instead.
            return self;
        }
        if self.is_dark() {
            self.control_bg = LinearRgba::with_srgba(30, 30, 32, 255);
            self.control_hover_bg = LinearRgba::with_srgba(255, 255, 255, 255).mul_alpha(0.08);
            self.control_border = LinearRgba::with_srgba(118, 118, 128, 255).mul_alpha(0.34);
            self.separator = LinearRgba::with_srgba(84, 84, 88, 255).mul_alpha(0.36);
            self.text = LinearRgba::with_srgba(242, 242, 247, 255);
            self.secondary_text = LinearRgba::with_srgba(226, 226, 232, 255);
            self.muted_text = LinearRgba::with_srgba(150, 150, 156, 255);
        } else {
            self.control_bg = LinearRgba::with_srgba(246, 246, 248, 255);
            self.control_hover_bg = LinearRgba::with_srgba(60, 60, 67, 255).mul_alpha(0.08);
            self.control_border = LinearRgba::with_srgba(60, 60, 67, 255).mul_alpha(0.22);
            self.separator = LinearRgba::with_srgba(60, 60, 67, 255).mul_alpha(0.20);
        }
        self
    }

    pub(crate) fn for_appearance(appearance: Appearance) -> Self {
        match appearance {
            Appearance::Light | Appearance::LightHighContrast => Self {
                derived: false,
                appearance,
                window_bg: rgb(238, 238, 242),
                sidebar_bg: rgb(238, 238, 242),
                workspace_sidebar_bg: rgb(226, 226, 232),
                header_bg: rgb(238, 238, 242),
                separator: rgba(60, 60, 67, 0.18),
                control_bg: rgba(255, 255, 255, 0.94),
                control_hover_bg: rgba(247, 247, 250, 0.98),
                control_pressed_bg: rgba(232, 242, 255, 0.98),
                control_border: rgba(60, 60, 67, 0.20),
                sidebar_button_bg: rgba(255, 255, 255, 0.72),
                sidebar_button_hover_bg: rgba(255, 255, 255, 0.94),
                sidebar_row_hover_bg: rgba(60, 60, 67, 0.08),
                sidebar_row_pressed_bg: rgba(60, 60, 67, 0.16),
                sidebar_row_active_bg: rgba(255, 255, 255, 0.78),
                sidebar_row_active_border: rgba(60, 60, 67, 0.18),
                selected_bg: rgb(0, 122, 255),
                accent: rgb(0, 122, 255),
                accent_hover: rgb(0, 106, 224),
                on_accent: rgb(255, 255, 255),
                danger: rgb(215, 38, 61),
                track_off: rgba(220, 220, 226, 1.0),
                card_bg: rgba(255, 255, 255, 0.72),
                group_bg: rgb(242, 242, 245),
                text: rgb(28, 28, 30),
                secondary_text: rgb(72, 72, 74),
                muted_text: rgb(142, 142, 147),
                selected_text: rgb(255, 255, 255),
                scrollbar_thumb: rgba(60, 60, 67, 0.32),
                spelling_error: rgb(215, 38, 61),
            },
            Appearance::Dark | Appearance::DarkHighContrast => Self {
                derived: false,
                appearance,
                window_bg: rgb(25, 25, 26),
                sidebar_bg: rgb(25, 25, 26),
                workspace_sidebar_bg: rgb(18, 18, 20),
                header_bg: rgb(25, 25, 26),
                separator: rgba(84, 84, 88, 0.22),
                control_bg: rgba(45, 45, 47, 0.98),
                control_hover_bg: rgba(55, 55, 57, 0.98),
                control_pressed_bg: rgba(66, 66, 69, 0.98),
                control_border: rgba(118, 118, 128, 0.28),
                sidebar_button_bg: rgba(36, 36, 38, 0.94),
                sidebar_button_hover_bg: rgba(48, 48, 50, 0.98),
                sidebar_row_hover_bg: rgba(42, 42, 44, 0.98),
                sidebar_row_pressed_bg: rgba(48, 48, 51, 0.98),
                sidebar_row_active_bg: rgba(54, 54, 57, 0.98),
                sidebar_row_active_border: rgba(118, 118, 128, 0.22),
                selected_bg: rgb(58, 58, 60),
                accent: rgb(10, 132, 255),
                accent_hover: rgb(50, 152, 255),
                on_accent: rgb(255, 255, 255),
                danger: rgb(255, 69, 58),
                track_off: rgba(78, 78, 82, 1.0),
                card_bg: rgba(30, 30, 32, 0.78),
                group_bg: rgb(37, 37, 39),
                text: rgb(242, 242, 247),
                secondary_text: rgb(199, 199, 204),
                muted_text: rgb(142, 142, 147),
                selected_text: rgb(255, 255, 255),
                scrollbar_thumb: rgba(142, 142, 147, 0.42),
                spelling_error: rgb(255, 69, 58),
            },
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct UiTokens {
    pub sidebar_min_width: f32,
    pub sidebar_max_width: f32,
    pub sidebar_default_width: f32,
    pub sidebar_padding: f32,
    pub row_height: f32,
    pub row_gap: f32,
    pub control_height: f32,
    pub control_radius: f32,
    /// Grouped-card corner radius.
    pub card_radius: f32,
    pub row_radius: f32,
    pub icon_size: f32,
    pub resize_handle_width: f32,
    pub scrollbar_width: f32,
    /// Gap between the scrollbar and the right edge of its area.
    pub scrollbar_inset: f32,
    /// Gap above and below the scrollbar track inside its area.
    pub scrollbar_margin_y: f32,
    /// Shortest the scrollbar thumb is allowed to get.
    pub scrollbar_min_thumb: f32,
}

impl Default for UiTokens {
    fn default() -> Self {
        Self {
            sidebar_min_width: 340.0,
            sidebar_max_width: 580.0,
            sidebar_default_width: 440.0,
            sidebar_padding: 28.0,
            row_height: 48.0,
            row_gap: 8.0,
            control_height: 56.0,
            control_radius: 12.0,
            card_radius: 36.0,
            row_radius: 9.0,
            icon_size: 26.0,
            resize_handle_width: 24.0,
            scrollbar_width: 5.0,
            scrollbar_inset: 4.0,
            scrollbar_margin_y: 8.0,
            scrollbar_min_thumb: 32.0,
        }
    }
}

impl UiTokens {
    /// Scale ThinkTerm's custom chrome from its original Retina pixel grid to
    /// the current window's backing scale. Text already follows the window
    /// DPI; applying the same ratio to controls keeps their size in points
    /// stable when a macOS window moves between Retina and non-Retina screens.
    pub(crate) fn for_dpi(dpi: usize) -> Self {
        let scale = ui_scale_for_dpi(dpi);
        let base = Self::default();
        Self {
            sidebar_min_width: base.sidebar_min_width * scale,
            sidebar_max_width: base.sidebar_max_width * scale,
            sidebar_default_width: base.sidebar_default_width * scale,
            sidebar_padding: base.sidebar_padding * scale,
            row_height: base.row_height * scale,
            row_gap: base.row_gap * scale,
            control_height: base.control_height * scale,
            control_radius: base.control_radius * scale,
            card_radius: base.card_radius * scale,
            row_radius: base.row_radius * scale,
            icon_size: base.icon_size * scale,
            resize_handle_width: base.resize_handle_width * scale,
            scrollbar_width: base.scrollbar_width * scale,
            scrollbar_inset: base.scrollbar_inset * scale,
            scrollbar_margin_y: base.scrollbar_margin_y * scale,
            scrollbar_min_thumb: base.scrollbar_min_thumb * scale,
        }
    }
}

/// ThinkTerm's custom chrome is authored in 2x macOS backing pixels on every
/// platform. Convert those values to the current monitor's backing-pixel
/// grid: on macOS a 2x surface reports dpi 144, so that is the design dpi;
/// elsewhere windows report logical dpi, so the design maps to 192 — a
/// 96dpi/100% display renders at 0.5, Windows 150% (144dpi) at 0.75 and
/// 200% (192dpi) at 1.0, all matching the macOS logical proportions.
pub(crate) fn ui_scale_for_dpi(dpi: usize) -> f32 {
    let design_dpi = if cfg!(target_os = "macos") {
        144.0
    } else {
        192.0
    };
    (dpi.max(1) as f32 / design_dpi).clamp(0.25, 4.0)
}

pub(crate) fn scale_ui_usize(value: usize, dpi: usize) -> usize {
    if value == 0 {
        0
    } else {
        ((value as f32 * ui_scale_for_dpi(dpi)).round() as usize).max(1)
    }
}

pub(crate) fn scale_ui_f32(value: f32, dpi: usize) -> f32 {
    value * ui_scale_for_dpi(dpi)
}

pub(crate) fn unscale_ui_usize(value: usize, dpi: usize) -> usize {
    if value == 0 {
        0
    } else {
        ((value as f32 / ui_scale_for_dpi(dpi)).round() as usize).max(1)
    }
}

pub(crate) fn rescale_ui_usize(value: usize, old_dpi: usize, new_dpi: usize) -> usize {
    if value == 0 {
        0
    } else {
        ((value as f32 * ui_scale_for_dpi(new_dpi) / ui_scale_for_dpi(old_dpi)).round() as usize)
            .max(1)
    }
}

fn rgb(red: u8, green: u8, blue: u8) -> LinearRgba {
    rgba(red, green, blue, 1.0)
}

fn rgba(red: u8, green: u8, blue: u8, alpha: f32) -> LinearRgba {
    let mut color = LinearRgba::with_srgba(red, green, blue, 255);
    color.3 = alpha;
    color
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Gruvbox dark hard: a near-black ground. The surfaces above it must stay
    /// apart, or the sidebar, the cards and the controls all merge into one
    /// slab.
    #[test]
    fn a_near_black_scheme_keeps_its_surfaces_apart() {
        let palette = UiPalette::for_scheme(rgb(29, 32, 33));
        let l = |c: LinearRgba| c.to_oklaba()[0];
        assert!(
            l(palette.control_bg) > l(palette.window_bg) + 0.02,
            "a control has to read as raised off the ground"
        );
        assert!(
            l(palette.window_bg) > l(palette.workspace_sidebar_bg),
            "the Spaces strip is recessed, not raised"
        );
        assert!(
            l(palette.control_pressed_bg) > l(palette.control_hover_bg),
            "pressed reads deeper than hover"
        );
    }

    /// The muted and secondary rows are drawn on the Spaces strip, which is
    /// recessed from the window. Holding them to their floor against the
    /// window alone left them under it where they are actually read.
    #[test]
    fn text_clears_its_floor_on_the_recessed_strip_too() {
        for (name, ground) in [
            ("Solarized Light", rgb(253, 246, 227)),
            ("3024 Day", rgb(247, 247, 247)),
            ("gruvbox dark", rgb(29, 32, 33)),
            ("white", rgb(255, 255, 255)),
        ] {
            let p = UiPalette::for_scheme(ground);
            for surface in [p.window_bg, p.sidebar_bg, p.workspace_sidebar_bg] {
                for (slot, colour, floor) in [
                    ("text", p.text, 7.0f32),
                    ("secondary_text", p.secondary_text, 4.5),
                    ("muted_text", p.muted_text, 3.0),
                ] {
                    let ratio = colour.contrast_ratio(&surface);
                    assert!(
                        ratio >= floor - 0.05,
                        "{} {}: {} against a surface it is drawn on, floor is {}",
                        name,
                        slot,
                        ratio,
                        floor
                    );
                }
            }
        }
    }

    /// The ends of the range are where a per-slot clamp used to destroy the
    /// design: on a white or near-white scheme every raised surface pinned to
    /// the same white, so a control, its hover state and a card were one
    /// colour and hovering showed nothing. Pure white and pure black are both
    /// real scheme backgrounds, and light schemes generally sit close enough
    /// to the top to lose the top of the ramp.
    #[test]
    fn a_scheme_at_either_end_of_the_range_keeps_its_surfaces_apart() {
        let l = |c: LinearRgba| c.to_oklaba()[0];
        for (name, ground) in [
            ("white", rgb(255, 255, 255)),
            ("3024 Day", rgb(247, 247, 247)),
            ("Solarized Light", rgb(253, 246, 227)),
            ("Horizon Light", rgb(253, 240, 237)),
            ("black", rgb(0, 0, 0)),
        ] {
            let palette = UiPalette::for_scheme(ground);
            assert!(
                (l(palette.control_hover_bg) - l(palette.control_bg)).abs() > 0.01,
                "{name}: a hover the user cannot see is not a hover \
                 (control {:.4}, hover {:.4})",
                l(palette.control_bg),
                l(palette.control_hover_bg),
            );
            assert!(
                l(palette.window_bg) > l(palette.workspace_sidebar_bg) + 0.01,
                "{name}: the Spaces strip is recessed, not level \
                 (window {:.4}, strip {:.4})",
                l(palette.window_bg),
                l(palette.workspace_sidebar_bg),
            );
            assert!(
                l(palette.control_bg) > l(palette.window_bg) + 0.01,
                "{name}: a control has to read as raised off the ground \
                 (window {:.4}, control {:.4})",
                l(palette.window_bg),
                l(palette.control_bg),
            );
        }
    }

    /// Solarized Light's ground and foreground are close together. Moving the
    /// text onto that ground without a floor would leave the secondary and
    /// muted rows barely visible.
    #[test]
    fn a_low_contrast_scheme_still_has_legible_text() {
        let palette = UiPalette::for_scheme(rgb(253, 246, 227));
        assert!(
            palette.text.contrast_ratio(&palette.window_bg) >= 7.0,
            "primary text: {}",
            palette.text.contrast_ratio(&palette.window_bg)
        );
        assert!(
            palette.secondary_text.contrast_ratio(&palette.window_bg) >= 4.5,
            "secondary text: {}",
            palette.secondary_text.contrast_ratio(&palette.window_bg)
        );
        assert!(
            palette.muted_text.contrast_ratio(&palette.window_bg) >= 3.0,
            "muted text: {}",
            palette.muted_text.contrast_ratio(&palette.window_bg)
        );
    }

    /// The same floor has to hold at the other extreme, where the ground is
    /// pure black and a naive shift would clamp every text colour together.
    #[test]
    fn a_pure_black_scheme_still_has_legible_text() {
        let palette = UiPalette::for_scheme(rgb(0, 0, 0));
        assert!(palette.text.contrast_ratio(&palette.window_bg) >= 7.0);
        assert!(palette.secondary_text.contrast_ratio(&palette.window_bg) >= 4.5);
        assert!(palette.muted_text.contrast_ratio(&palette.window_bg) >= 3.0);
    }

    /// The menus tune their card with fixed neutral greys, which is right for
    /// the hand-tuned palettes and would undo the tint on a derived one. The
    /// flag is how they tell the two apart.
    #[test]
    fn a_derived_palette_says_so_and_a_hand_tuned_one_does_not() {
        assert!(UiPalette::for_scheme(rgb(29, 32, 33)).derived);
        assert!(!UiPalette::for_appearance(Appearance::Dark).derived);
        assert!(!UiPalette::for_appearance(Appearance::Light).derived);
    }

    /// The base has to be the side the ground is on. Handed the other one, the
    /// light palette's hover -- dark ink at low alpha -- lands on a dark
    /// ground as black over black, and hover becomes darker than the thing it
    /// is hovering over.
    #[test]
    fn the_base_palette_is_chosen_by_the_ground_not_by_the_caller() {
        let dark_ground = UiPalette::for_scheme(rgb(29, 32, 33));
        assert!(dark_ground.is_dark(), "a dark scheme picks the dark base");
        let light_ground = UiPalette::for_scheme(rgb(253, 246, 227));
        assert!(!light_ground.is_dark(), "a light scheme picks the light base");

        let l = |c: LinearRgba| c.to_oklaba()[0];
        assert!(
            l(light_ground.sidebar_row_hover_bg) < l(light_ground.window_bg),
            "on a light ground a hover row is darker than the ground"
        );
        assert!(
            l(dark_ground.sidebar_row_hover_bg) > l(dark_ground.window_bg),
            "on a dark ground it is lighter"
        );
    }

    /// A scheme's own blue is often unreadable on its own background, so the
    /// accent family is not derived. If that ever changes it should be a
    /// decision, not a surprise.
    #[test]
    fn the_accent_family_is_not_derived() {
        let base = UiPalette::for_appearance(Appearance::Dark);
        let derived = UiPalette::for_scheme(rgb(29, 32, 33));
        assert_eq!(derived.accent, base.accent);
        assert_eq!(derived.accent_hover, base.accent_hover);
        assert_eq!(derived.on_accent, base.on_accent);
        assert_eq!(derived.danger, base.danger);
        assert_eq!(derived.selected_bg, base.selected_bg);
    }

    /// Several slots are translucent on purpose so they pick up what is behind
    /// them; a derivation that flattened them would paint hairlines as slabs.
    #[test]
    fn translucent_slots_keep_their_alpha() {
        let base = UiPalette::for_appearance(Appearance::Dark);
        let derived = UiPalette::for_scheme(rgb(29, 32, 33));
        assert_eq!(derived.separator.3, base.separator.3);
        assert_eq!(derived.card_bg.3, base.card_bg.3);
        assert_eq!(derived.scrollbar_thumb.3, base.scrollbar_thumb.3);
        assert_eq!(derived.control_border.3, base.control_border.3);
    }

    /// Derived colours have to be paintable. An extreme lightness paired with
    /// a strong chroma can leave the sRGB gamut, and a channel outside 0..=1
    /// shows up as a blown-out block.
    #[test]
    fn every_derived_colour_stays_inside_the_gamut() {
        for ground in [
            rgb(0, 0, 0),
            rgb(40, 20, 60),
            rgb(255, 255, 255),
            rgb(253, 246, 227),
        ] {
            let p = UiPalette::for_scheme(ground);
            for (name, c) in [
                ("window_bg", p.window_bg),
                ("control_bg", p.control_bg),
                ("card_bg", p.card_bg),
                ("text", p.text),
                ("muted_text", p.muted_text),
                ("separator", p.separator),
            ] {
                for (i, ch) in [c.0, c.1, c.2, c.3].iter().enumerate() {
                    assert!(
                        (0.0..=1.0).contains(ch),
                        "{} channel {} is {}",
                        name,
                        i,
                        ch
                    );
                }
            }
        }
    }

    fn design_dpi() -> usize {
        if cfg!(target_os = "macos") {
            144
        } else {
            192
        }
    }

    #[test]
    fn ui_pixels_follow_monitor_dpi() {
        let design_dpi = design_dpi();
        assert_eq!(ui_scale_for_dpi(design_dpi), 1.0);
        assert_eq!(scale_ui_usize(40, design_dpi / 2), 20);
        assert_eq!(scale_ui_usize(40, design_dpi * 2), 80);
    }

    #[test]
    fn ui_widths_round_trip_through_design_pixels() {
        let design_dpi = design_dpi();
        let monitor_dpi = design_dpi / 2;
        let scaled = scale_ui_usize(380, monitor_dpi);
        assert_eq!(unscale_ui_usize(scaled, monitor_dpi), 380);
        assert_eq!(rescale_ui_usize(80, design_dpi, monitor_dpi), 40);
    }

    /// The chrome is resolved once per configuration now rather than once per
    /// draw, so a palette has to answer for the appearance it was built for:
    /// the menus tune themselves off this rather than being handed one.
    #[test]
    fn a_palette_reports_the_appearance_it_was_built_for() {
        for appearance in [
            Appearance::Light,
            Appearance::LightHighContrast,
            Appearance::Dark,
            Appearance::DarkHighContrast,
        ] {
            assert_eq!(UiPalette::for_appearance(appearance).appearance, appearance);
        }
    }

    /// The two high-contrast appearances collapse onto the two palettes, so
    /// `is_dark` has to answer for all four rather than for the two it was
    /// written against.
    #[test]
    fn the_high_contrast_appearances_pick_the_same_side() {
        assert!(UiPalette::for_appearance(Appearance::Dark).is_dark());
        assert!(UiPalette::for_appearance(Appearance::DarkHighContrast).is_dark());
        assert!(!UiPalette::for_appearance(Appearance::Light).is_dark());
        assert!(!UiPalette::for_appearance(Appearance::LightHighContrast).is_dark());
    }

    /// A pin on the values themselves. Caching the palette means a slot that
    /// shifted while the chrome was being rewired would not show up as a
    /// failure anywhere else -- the terminal's own colours are untouched, and
    /// nothing else asserts on these.
    #[test]
    fn the_two_palettes_keep_their_colours() {
        let dark = UiPalette::for_appearance(Appearance::Dark);
        assert_eq!(dark.window_bg, rgb(25, 25, 26));
        assert_eq!(dark.workspace_sidebar_bg, rgb(18, 18, 20));
        assert_eq!(dark.text, rgb(242, 242, 247));
        assert_eq!(dark.card_bg, rgba(30, 30, 32, 0.78));

        let light = UiPalette::for_appearance(Appearance::Light);
        assert_eq!(light.window_bg, rgb(238, 238, 242));
        assert_eq!(light.workspace_sidebar_bg, rgb(226, 226, 232));
        assert_eq!(light.text, rgb(28, 28, 30));
        assert_eq!(light.accent, rgb(0, 122, 255));

        // The same grey in both, which is easy to "fix" by accident.
        assert_eq!(dark.muted_text, light.muted_text);
    }
}


