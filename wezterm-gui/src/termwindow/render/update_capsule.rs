//! The update notice: while a newer release is known it is one of the
//! things on the dynamic status (see `dynamic_status`), and follows the
//! install through to the restart. Dismissed, it goes into a dot on the
//! settings gear until the next launch.

use crate::settings_window::UpdateCapsuleStatus;
use crate::termwindow::render::dynamic_status::{CardContent, Mark, Tone};
use crate::termwindow::render::transfer_card::CardAction;
use crate::termwindow::ui::icons::SvgIcon;
use crate::termwindow::UIItemType;
use crate::ui::draw::DrawContext;
use std::cell::Cell;
use std::time::{Duration, Instant};
use window::color::LinearRgba;
use window::WindowOps;

/// A click acts only on a state that has been on screen this long: one
/// meant for "Install" must not restart because the install finished under
/// the pointer.
const SETTLE: Duration = Duration::from_millis(350);

/// What the notice names it by, under the state it is in.
const PRODUCT: &str = "ThinkTerm";

thread_local! {
    /// Dismissed in any window: it stays in the gear's dot until the next
    /// launch.
    static DISMISSED: Cell<bool> = const { Cell::new(false) };
}

/// Whether the notice has gone into the gear's dot.
pub(crate) fn dismissed() -> bool {
    DISMISSED.with(Cell::get)
}

/// The settings gear's dot, once the notice has gone into it: red when the
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

/// One window's notice between frames: the state it last showed, and since
/// when, for a click to act on what was seen.
#[derive(Default)]
pub(crate) struct UpdateCapsule {
    kind: Option<Kind>,
    changed_at: Option<Instant>,
}

impl UpdateCapsule {
    /// Note the state on screen this frame, or that there is none.
    pub(crate) fn note(&mut self, now: Instant, status: Option<&UpdateCapsuleStatus>) {
        let kind = status.map(Kind::of);
        if kind != self.kind {
            self.changed_at = self.kind.is_some().then_some(now);
            self.kind = kind;
        }
    }
}

/// Where the notice ranks on the dynamic status, as its other things do:
/// a failed install with the errors, one waiting for its restart with what
/// has just finished, the download with what is under way, and a release
/// only known about after everything else.
pub(crate) fn rank(status: &UpdateCapsuleStatus) -> u8 {
    match Kind::of(status) {
        Kind::Failed => 1,
        Kind::Restart | Kind::Installed => 2,
        Kind::Installing => 3,
        Kind::Available => 4,
    }
}

/// Which state it is in, as far as its arrival goes; distinct from every
/// other thing's.
pub(crate) fn phase(status: &UpdateCapsuleStatus) -> u8 {
    8 + Kind::of(status) as u8
}

/// Whether it arriving in this state deserves its moment as a card: an
/// install that has just finished or failed. A release turning up, or the
/// download a click just started, does not.
pub(crate) fn is_news(status: &UpdateCapsuleStatus) -> bool {
    matches!(
        Kind::of(status),
        Kind::Restart | Kind::Installed | Kind::Failed
    )
}

/// What the notice says and offers in `status`.
pub(crate) fn content(status: &UpdateCapsuleStatus) -> CardContent {
    let act = |icon: SvgIcon| CardAction {
        icon,
        primary: true,
        item: UIItemType::UpdateCapsule,
    };
    // Dismissing works in every state: closed during the download, the
    // install carries on and the dot waits on the gear.
    let close = CardAction {
        icon: SvgIcon::X,
        primary: false,
        item: UIItemType::UpdateCapsuleDismiss,
    };
    let product = PRODUCT.to_string();
    match status {
        UpdateCapsuleStatus::Available { version, new_build } => CardContent {
            quiet: false,
            mark: Mark::Icon(SvgIcon::ArrowDown),
            tone: Tone::Progress,
            name: crate::i18n::tr(if *new_build {
                "update-capsule-new-build"
            } else {
                "update-capsule-available"
            }),
            detail: format!("{PRODUCT} {version}"),
            short: version.clone(),
            bar: None,
            actions: vec![act(SvgIcon::ArrowDown), close],
        },
        UpdateCapsuleStatus::Installing { fraction } => CardContent {
            quiet: false,
            mark: Mark::Icon(SvgIcon::ArrowDown),
            tone: Tone::Progress,
            name: crate::i18n::tr(if fraction.is_some() {
                "update-capsule-downloading"
            } else {
                "update-capsule-installing"
            }),
            detail: match fraction {
                Some(fraction) => format!("{PRODUCT} · {}%", (fraction * 100.0).floor() as u32),
                None => product,
            },
            short: fraction
                .map(|fraction| format!("{}%", (fraction * 100.0).floor() as u32))
                .unwrap_or_default(),
            bar: Some(*fraction),
            actions: vec![close],
        },
        UpdateCapsuleStatus::Installed { restart: true } => CardContent {
            quiet: false,
            mark: Mark::Icon(SvgIcon::RotateCw),
            tone: Tone::Done,
            name: crate::i18n::tr("update-capsule-restart"),
            detail: product,
            short: String::new(),
            bar: None,
            actions: vec![act(SvgIcon::RotateCw), close],
        },
        // A restart here would end terminals: the Update page says what
        // to do instead.
        UpdateCapsuleStatus::Installed { restart: false } => CardContent {
            quiet: false,
            mark: Mark::Icon(SvgIcon::Check),
            tone: Tone::Done,
            name: crate::i18n::tr("update-capsule-installed"),
            detail: product,
            short: String::new(),
            bar: None,
            actions: vec![act(SvgIcon::ArrowRight), close],
        },
        UpdateCapsuleStatus::Failed => CardContent {
            quiet: false,
            mark: Mark::Icon(SvgIcon::CircleAlert),
            tone: Tone::Failed,
            name: crate::i18n::tr("update-capsule-failed"),
            detail: product,
            short: String::new(),
            bar: None,
            actions: vec![act(SvgIcon::RotateCw), close],
        },
    }
}

/// The hover tag on the notice's main button, as what a click on it does
/// in the state it shows.
pub(crate) fn action_tooltip() -> &'static str {
    let installs = crate::settings_window::installs_updates_itself();
    match crate::settings_window::update_capsule_status() {
        Some(UpdateCapsuleStatus::Available { .. }) if installs => "settings-update-install",
        Some(UpdateCapsuleStatus::Failed) if installs => "right-retry",
        Some(UpdateCapsuleStatus::Installed { restart: true }) => "settings-restart-app",
        _ => "settings-section-update",
    }
}

impl crate::TermWindow {
    /// The close button: it goes into the gear's dot, in every window.
    pub(crate) fn dismiss_update_capsule(&mut self) {
        DISMISSED.with(|dismissed| dismissed.set(true));
        crate::frontend::front_end().invalidate_all_windows();
    }

    /// Whether a click on the notice would do anything: nothing does while
    /// the update installs.
    pub(crate) fn update_capsule_clickable(&self) -> bool {
        self.update_capsule.kind != Some(Kind::Installing)
    }

    /// What the notice's main button does in what it shows.
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
}

/// A ring around an icon: a faint track, the done part over it from twelve
/// o'clock, then the surface's colour inside to cut the ring's inner edge
/// clean. Without a fraction, a quarter of it at `turn` (0 to 1).
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_ring(
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
    fn the_notice_reads_each_status_as_its_kind() {
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

    #[test]
    fn a_known_release_gives_way_to_everything_else() {
        let available = UpdateCapsuleStatus::Available {
            version: "1.2.3".to_string(),
            new_build: false,
        };
        assert_eq!(rank(&available), 4);
        assert!(!is_news(&available));
        assert_eq!(rank(&UpdateCapsuleStatus::Failed), 1);
        assert!(is_news(&UpdateCapsuleStatus::Installed { restart: true }));
    }

    #[test]
    fn a_click_waits_for_a_changed_state_to_settle() {
        let now = Instant::now();
        let mut capsule = UpdateCapsule::default();
        capsule.note(now, Some(&UpdateCapsuleStatus::Installing { fraction: None }));
        assert_eq!(capsule.changed_at, None);
        capsule.note(now, Some(&UpdateCapsuleStatus::Installed { restart: true }));
        assert_eq!(capsule.kind, Some(Kind::Restart));
        assert_eq!(capsule.changed_at, Some(now));
    }
}
