//! Sharing a pane with other accounts on its server's machine, and the
//! panes they share with ours.
//!
//! A terminal in this window mirrors a pane on a mux server -- the local
//! session host, or a host reached with ThinkTerm Connect -- and a share
//! is that server's: every call here goes to the server the pane lives on.
//! A pane with no server behind it has no share button.
//!
//! The share button opens a panel under it. Before the pane is shared, the
//! panel picks who may watch from the accounts and groups of the server's
//! machine, and how; once it is, the panel shows who watches and who asks
//! to type, with the buttons that answer them.
//!
//! A pane watching another account's share runs the viewer program (see
//! `cli/share_watch.rs`), which says where the watch stands in a user
//! variable; the pane's bar shows it as a pill, the button for asking to
//! type.
//!
//! The window asks its servers after their shares every couple of seconds
//! while it holds some, and now and then otherwise, to see one started
//! from the command line; and after what others share with it only while
//! the sidebar listing them is open.

use crate::quad::{HeapQuadAllocator, QuadClipRect, TripleLayerQuadAllocator};
use crate::termwindow::ui::icons::SvgIcon;
use crate::termwindow::{PaneNavAction, TermWindow, TermWindowNotif, UIItem, UIItemType};
use crate::ui::{
    apply_wheel, contains, draw_button, draw_text_input, rect, ButtonSpec,
    ButtonVariant, ControlState, DrawContext, InputCaret, InteractionState, ScrollState,
    ShapedText, TextInputSpec, TextInputState, UiContext, UiPalette, UiTokens, WidgetKind,
};
use crate::utilsprites::RenderMetrics;
use anyhow::Context as _;
use codec::{
    ShareAudience, ShareCandidatesResponse, ShareDeny, ShareGrant, ShareInfo, ShareKick,
    ShareRevoke, ShareStart, ShareStop, ShareViewer,
};
use config::keyassignment::SpawnTabDomain;
use config::TermConfig;
use mux::domain::DomainId;
use mux::pane::{Pane, PaneId};
use mux::window::WindowId as MuxWindowId;
use mux::Mux;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use termwiz::input::KeyCode;
use wezterm_client::client::Client;
use wezterm_client::domain::ClientDomain;
use wezterm_client::pane::ClientPane;
use wezterm_font::LoadedFont;
use wezterm_term::TerminalConfiguration;
use window::color::LinearRgba;
use window::{Modifiers, MouseCursor, MouseEvent, MouseEventKind, MousePress, RectF, WindowOps};

/// How often a window holding shares asks after them, and how often one
/// holding none does.
const OWNED_EVERY: Duration = Duration::from_secs(2);
const OWNED_IDLE_EVERY: Duration = Duration::from_secs(30);
/// How often an open sidebar asks what is shared with it.
const INCOMING_EVERY: Duration = Duration::from_secs(10);
/// How often sharing turned off asks again after the shares it could not
/// stop yet.
const STOP_AGAIN_EVERY: Duration = Duration::from_secs(10);
/// How long a viewer who asked may type once let.
const HALF_HOUR: u64 = 30 * 60;
/// When a share ends by itself, as the panel offers it: half an hour at
/// most, which the server holds to as well.
const ENDS: [(Option<u64>, &str); 2] = [
    (Some(15 * 60), "share-panel-ends-15m"),
    (Some(HALF_HOUR), "share-panel-ends-30m"),
];
/// The user variable the viewer program says where its watch stands in.
const WATCH_VAR: &str = "thinkterm_share";

/// Above the tooltips and the command palette, under the recording
/// overlay: the panel is opened over the others and closes when one of
/// them takes the window (see `paint_share_panel`). A z-index of its own,
/// for the reason the dynamic status's gives.
const PANEL_ZINDEX: i8 = 12;

// Geometry in design pixels (the 2x macOS grid), converted with `ctx.px`
// where it is used, as the command palette's and the menus' are.
const CARD_WIDTH: f32 = 640.0;
const CARD_PAD: f32 = 32.0;
const CARD_RADIUS: f32 = 34.0;
/// Room kept to the window's edges, and below the button.
const CARD_MARGIN: f32 = 16.0;
const CARD_DROP: f32 = 10.0;
const TITLE_GAP: f32 = 10.0;
const SECTION_GAP: f32 = 30.0;
const LABEL_GAP: f32 = 12.0;
const ROW_EXTRA: f32 = 24.0;
/// Rows of accounts shown before the list scrolls.
const LIST_ROWS: usize = 6;
const LIST_PAD: f32 = 8.0;
const LIST_INSET: f32 = 18.0;
const LIST_RADIUS: f32 = 18.0;
/// From this many accounts and groups on, a field finds one by name.
const SEARCH_FROM: usize = 8;
const CHECK_SIZE: f32 = 30.0;
const CHECK_RADIUS: f32 = 8.0;
const ROW_ICON: f32 = 28.0;
const BUTTON_H: f32 = 54.0;
const SMALL_BUTTON_H: f32 = 44.0;
/// Wider than its label by this: the house size, as `widgets` explains.
const BUTTON_ROOM: f32 = 56.0;
const BUTTON_GAP: f32 = 14.0;
const BOX_PAD: f32 = 22.0;
const BOX_RADIUS: f32 = 20.0;
const INPUT_H: f32 = 54.0;
/// The panel's text against the menus': the same body size, and a title
/// a step up from it.
const BODY_SCALE: f64 = 13.0 / 15.0;
const TITLE_SCALE: f64 = 1.25;

/// A pane another account shares with ours.
#[derive(Clone, Debug)]
pub(crate) struct IncomingRow {
    pub owner: String,
    pub share_id: u64,
    pub title: String,
    /// The workspace it is watched in (see `view_workspace`).
    pub workspace: String,
}

thread_local! {
    /// Requests to type already told, so each is told once, however many
    /// windows hear of it: by server, share and viewer.
    static TOLD: std::cell::RefCell<HashSet<(DomainId, u64, u64)>> =
        std::cell::RefCell::new(HashSet::new());
    /// Sharing turned off, for the whole program: its servers are every
    /// window's.
    static CLEARING: std::cell::RefCell<Clearing> =
        std::cell::RefCell::new(Clearing::default());
}

/// Sharing turned off clears of their shares the servers the program's
/// windows knew to hold some, as sharing was on: a share is started from
/// a window's button, so those are this desktop's. A server that cannot be
/// reached is asked again until it can. A desktop that never had sharing
/// on knows of none and leaves every server alone: another desktop's
/// shares are not its to stop. Turned on again, all of it is let go.
#[derive(Default)]
struct Clearing {
    /// The servers yet to clear.
    left: HashSet<DomainId>,
    /// Being asked right now.
    asking: HashSet<DomainId>,
    asked: Option<Instant>,
    /// Each told once, until sharing is turned on again.
    told_stopped: bool,
    told_unreachable: bool,
}

/// What clearing a server comes to that is worth telling.
#[derive(Debug, PartialEq, Eq)]
enum Tell {
    /// Shares were stopped.
    Stopped,
    /// A server holding shares cannot be reached: they run on until it can.
    Unreachable,
}

impl Clearing {
    /// Sharing is off: `known` are the servers a window knew to hold
    /// shares. Told each tick: a server new to clear is asked at once, and
    /// the asking keeps its pace otherwise.
    fn turned_off(&mut self, known: &HashSet<DomainId>) {
        for domain_id in known {
            if self.left.insert(*domain_id) {
                self.asked = None;
            }
        }
    }

    /// The servers to ask now: every `STOP_AGAIN_EVERY`, none still being
    /// asked, none that `exists` no longer.
    fn due(&mut self, exists: impl Fn(DomainId) -> bool) -> Vec<DomainId> {
        self.left.retain(|domain_id| exists(*domain_id));
        let due = self.asked.is_none_or(|at| at.elapsed() >= STOP_AGAIN_EVERY);
        if self.left.is_empty() || !due {
            return vec![];
        }
        self.asked = Some(Instant::now());
        let servers: Vec<DomainId> = self.left.iter().copied().collect();
        servers
            .into_iter()
            .filter(|domain_id| self.asking.insert(*domain_id))
            .collect()
    }

    /// What asking `domain_id` came to: how many shares it stopped, or why
    /// it could not. What to tell of it, once.
    fn answered(&mut self, domain_id: DomainId, outcome: anyhow::Result<usize>) -> Option<Tell> {
        self.asking.remove(&domain_id);
        if !self.left.contains(&domain_id) {
            return None;
        }
        match outcome {
            Ok(stopped) => {
                self.left.remove(&domain_id);
                (stopped > 0 && !std::mem::replace(&mut self.told_stopped, true))
                    .then_some(Tell::Stopped)
            }
            Err(err) => {
                log::debug!("clearing domain {domain_id} of its shares: {err:#}");
                (!std::mem::replace(&mut self.told_unreachable, true)).then_some(Tell::Unreachable)
            }
        }
    }

    /// Sharing is on again.
    fn turned_on(&mut self) {
        *self = Self::default();
    }
}

/// Stop every share `client`'s server holds: how many it stopped.
async fn clear(client: Client) -> anyhow::Result<usize> {
    let held = client.share_list().await?.shares;
    let mut stopped = 0;
    for share in &held {
        match client
            .share_stop(ShareStop {
                share_id: share.share_id,
            })
            .await
        {
            Ok(_) => stopped += 1,
            Err(err) => log::debug!("stopping share {}: {err:#}", share.share_id),
        }
    }
    // A share can end by itself, or by another's asking, between the list
    // and the stop: one that would not stop may be gone anyway.
    if stopped < held.len() {
        let left = client.share_list().await?.shares.len();
        anyhow::ensure!(left == 0, "{left} shares would not stop");
    }
    Ok(stopped)
}

/// Clear the servers due for it (see `Clearing`), telling once that shares
/// were stopped, or that a server holding some cannot be reached.
fn clear_servers() {
    let mux = Mux::get();
    let due = CLEARING.with(|clearing| {
        clearing
            .borrow_mut()
            .due(|domain_id| mux.get_domain(domain_id).is_some())
    });
    for domain_id in due {
        let client = share_client(domain_id);
        promise::spawn::spawn(async move {
            let outcome = match client {
                Ok(client) => clear(client).await,
                Err(err) => Err(err),
            };
            let tell =
                CLEARING.with(|clearing| clearing.borrow_mut().answered(domain_id, outcome));
            let (title, body) = match tell {
                Some(Tell::Stopped) => ("share-stopped-off-title", "share-stopped-off-body"),
                Some(Tell::Unreachable) => ("share-stop-pending-title", "share-stop-pending-body"),
                None => return,
            };
            wezterm_toast_notification::persistent_toast_notification(
                &crate::i18n::tr(title),
                &crate::i18n::tr(body),
            );
        })
        .detach();
    }
}

#[derive(Default)]
pub(crate) struct ShareUi {
    /// The shares of this window's servers, as last asked, by domain.
    owned: HashMap<DomainId, Vec<ShareInfo>>,
    owned_asked: Option<Instant>,
    /// What other accounts share with the account of the window's server,
    /// as last asked, and where that was asked.
    incoming: Vec<IncomingRow>,
    /// The accounts whose ThinkTerm is another version, asked with it:
    /// what they share cannot be watched.
    other_version: Vec<String>,
    incoming_domain: Option<DomainId>,
    incoming_asked: Option<Instant>,
    /// The server asked right now, if any.
    incoming_asking: Option<DomainId>,
    watch_program: Option<PathBuf>,
    /// The share button's panel, while it is open.
    panel: Option<SharePanel>,
    /// Where the watch in each of the window's viewer panes stands, as its
    /// viewer last said. Dropped with the pane (see `share_tick`).
    watching: HashMap<PaneId, WatchState>,
    /// The mux window of the shared pane this window went to watch: when it
    /// closes under the window, the window goes back to its thread.
    view_shown: Option<MuxWindowId>,
    /// Watches being opened, by workspace, so a second click waits for the
    /// first instead of opening another.
    opening: HashSet<String>,
    /// Who each server's machine has to share with, as last asked: the
    /// panel opens on it at once and asks again behind it. One list per
    /// server, gone when sharing is turned off.
    candidates: HashMap<DomainId, Vec<Candidate>>,
    candidates_asking: HashSet<DomainId>,
    /// Servers asked ahead of the panel's first opening, once each.
    candidates_readied: HashSet<DomainId>,
    /// The panel's fonts were loaded ahead of its first opening.
    fonts_ready: bool,
}

/// What a viewer program publishes of its watch.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
struct WatchState {
    state: String,
    owner: String,
    why: Option<String>,
}

/// How a viewer pane's bar shows its watch: a pill, which presses as the
/// viewer's `^] r` does.
pub(crate) struct SharePill {
    pub label: String,
    pub icon: SvgIcon,
    /// Lit with the accent: the viewer may type.
    pub lit: bool,
    /// Whether pressing it does anything yet.
    pub live: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PanelAction {
    /// An account or group, by its place in the candidates.
    Pick(usize),
    Search,
    Ends(usize),
    Cancel,
    Start,
    Grant(u64),
    Deny(u64),
    TakeBack,
    Remove(u64),
    Stop,
    Close,
}

/// An account or a group the pane can be shared with.
#[derive(Clone, PartialEq, Eq)]
struct Candidate {
    name: String,
    group: bool,
    me: bool,
}

impl Candidate {
    fn label(&self) -> String {
        if self.me {
            tr_with("share-panel-you", &[("name", self.name.clone())])
        } else if self.group {
            tr_with("share-panel-group", &[("name", self.name.clone())])
        } else {
            self.name.clone()
        }
    }
}

enum Candidates {
    Asking,
    Failed,
    Ready(Vec<Candidate>),
}

/// The share button's panel.
pub(crate) struct SharePanel {
    /// The pane, as this window knows it, and where it lives.
    pane_id: PaneId,
    domain_id: DomainId,
    remote_pane_id: PaneId,
    candidates: Candidates,
    /// Beside `candidates`: which are picked.
    picked: Vec<bool>,
    search: TextInputState,
    list: ScrollState,
    list_area: RectF,
    /// The whole panel, scrolled when it is taller than the window.
    body: ScrollState,
    /// Which of `ENDS`.
    ends: usize,
    /// What went wrong last, in the panel's own words.
    error: Option<String>,
    /// A call to the server is under way.
    busy: bool,
    widgets: UiContext<PanelAction>,
    interaction: InteractionState<PanelAction>,
    card: RectF,
    /// What the panel's words shaped to: bounded, and dropped with the
    /// panel.
    shaped: ShapedText,
}

impl SharePanel {
    fn new(pane_id: PaneId, domain_id: DomainId, remote_pane_id: PaneId) -> Self {
        Self {
            pane_id,
            domain_id,
            remote_pane_id,
            candidates: Candidates::Asking,
            picked: vec![],
            search: TextInputState::new(),
            list: ScrollState::new(),
            list_area: rect(0.0, 0.0, 0.0, 0.0),
            body: ScrollState::new(),
            ends: ENDS.len() - 1,
            error: None,
            busy: false,
            widgets: UiContext::default(),
            interaction: InteractionState::default(),
            card: rect(0.0, 0.0, 0.0, 0.0),
            shaped: ShapedText::default(),
        }
    }

    /// The accounts and groups to pick from, as the server last said: the
    /// ones picked before stay picked.
    fn take_candidates(&mut self, list: Vec<Candidate>) {
        let picked: HashSet<(String, bool)> = match &self.candidates {
            Candidates::Ready(old) => old
                .iter()
                .zip(&self.picked)
                .filter(|(_, picked)| **picked)
                .map(|(candidate, _)| (candidate.name.clone(), candidate.group))
                .collect(),
            _ => HashSet::new(),
        };
        self.picked = list
            .iter()
            .map(|candidate| picked.contains(&(candidate.name.clone(), candidate.group)))
            .collect();
        self.candidates = Candidates::Ready(list);
    }

    fn searching(&self) -> bool {
        matches!(&self.candidates, Candidates::Ready(list) if list.len() >= SEARCH_FROM)
    }

    fn audience(&self) -> Vec<ShareAudience> {
        let Candidates::Ready(list) = &self.candidates else {
            return vec![];
        };
        list.iter()
            .zip(&self.picked)
            .filter(|(_, picked)| **picked)
            .map(|(candidate, _)| match candidate.group {
                true => ShareAudience::Group(candidate.name.clone()),
                false => ShareAudience::User(candidate.name.clone()),
            })
            .collect()
    }

    fn can_start(&self) -> bool {
        !self.busy && self.picked.iter().any(|picked| *picked)
    }

    fn state(&self, action: PanelAction, enabled: bool) -> ControlState {
        if !enabled || (self.busy && !matches!(action, PanelAction::Cancel | PanelAction::Close))
        {
            ControlState::Disabled
        } else if self.interaction.pressed == Some(action) {
            ControlState::Pressed
        } else if self.interaction.hovered == Some(action) {
            ControlState::Hovered
        } else {
            ControlState::Normal
        }
    }
}

/// What the panel paints with.
struct Look<'a> {
    ctx: &'a DrawContext<'a>,
    title_ctx: &'a DrawContext<'a>,
    body: &'a Rc<LoadedFont>,
    strong: &'a Rc<LoadedFont>,
    title: &'a Rc<LoadedFont>,
    palette: UiPalette,
    /// The panel's insets: the list of accounts, a choice's track, a
    /// request; and the row under the pointer in one.
    inset: LinearRgba,
    inset_hover: LinearRgba,
    /// The column the content stands in.
    x: f32,
    width: f32,
    line: f32,
    title_line: f32,
    /// What its words shaped to, kept while the panel is open: each frame
    /// draws the same ones (see `SharePanel::shaped`).
    shaped: std::cell::RefCell<ShapedText>,
}

impl Look<'_> {
    fn px(&self, value: f32) -> f32 {
        self.ctx.px(value)
    }

    /// `text` at `x`, `y` in `font`, as `DrawContext::draw_text` draws it.
    #[allow(clippy::too_many_arguments)]
    fn text(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        font: &Rc<LoadedFont>,
        x: f32,
        y: f32,
        text: &str,
        color: LinearRgba,
        max_width: f32,
    ) -> anyhow::Result<()> {
        self.ctx.draw_text_shaped_on_layer(
            layers,
            1,
            font,
            x,
            y,
            text,
            color,
            max_width,
            &mut self.shaped.borrow_mut(),
        )
    }

    /// `text` in the title's font, on the title's line.
    fn title_text(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        x: f32,
        y: f32,
        text: &str,
        color: LinearRgba,
        max_width: f32,
    ) -> anyhow::Result<()> {
        self.title_ctx.draw_text_shaped_on_layer(
            layers,
            1,
            self.title,
            x,
            y,
            text,
            color,
            max_width,
            &mut self.shaped.borrow_mut(),
        )
    }

    fn measure(&self, font: &Rc<LoadedFont>, text: &str) -> f32 {
        self.ctx
            .measure_text_width_shaped(font, text, &mut self.shaped.borrow_mut())
    }

    /// `text` in lines of `width`, at most `max` of them.
    fn wrap(&self, font: &Rc<LoadedFont>, text: &str, width: f32, max: usize) -> Vec<String> {
        crate::termwindow::ui::right_sidebar::wrap_snippet_text_for_width(
            text,
            max,
            false,
            |segment| self.measure(font, segment) / width.max(1.0),
        )
    }

    /// `text` wrapped across the column from `y`; where the next thing
    /// goes.
    fn paragraph(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        font: &Rc<LoadedFont>,
        text: &str,
        y: f32,
        color: LinearRgba,
        max: usize,
    ) -> anyhow::Result<f32> {
        let mut y = y;
        for line in self.wrap(font, text, self.width, max) {
            self.text(layers, font, self.x, y, &line, color, self.width)?;
            y += self.line;
        }
        Ok(y)
    }

    fn button_width(&self, label: &str) -> f32 {
        self.measure(self.body, label) + self.px(BUTTON_ROOM)
    }
}

/// Whether sharing is turned on (Settings → Share & Sync). Off, nothing
/// here shows or asks anything of a server, but a watch already open goes
/// on.
pub(crate) fn sharing_enabled() -> bool {
    crate::native_settings::load_shared().sharing.enabled
}

/// Where `pane` lives: its server's domain, and its id there. None off a
/// mux server, and on this machine's own when that is Windows, which has
/// no share socket yet: a window there shares the panes of the Linux and
/// Mac servers it is connected to, and hears what is shared on those.
fn share_target(pane: &Arc<dyn Pane>) -> Option<(DomainId, PaneId)> {
    let mirror = pane.downcast_ref::<ClientPane>()?;
    let domain_id = pane.domain_id();
    if cfg!(windows) && crate::local_sessions::is_host_domain_id(domain_id) {
        return None;
    }
    Some((domain_id, mirror.remote_pane_id))
}

fn share_client(domain_id: DomainId) -> anyhow::Result<Client> {
    Ok(ClientDomain::get_client_inner_for_domain(domain_id)?
        .client
        .clone())
}

/// The workspace a share is watched in: one per share, on the server that
/// listed it, so the window goes to it as to a thread, and comes back to
/// the same one.
fn view_workspace(domain_name: &str, owner: &str, share_id: u64) -> String {
    format!(
        "{}{domain_name}/{owner}/{share_id}",
        crate::workspace_threads::SHARE_VIEW_WORKSPACE_PREFIX
    )
}

fn tr_with(id: &str, named: &[(&str, String)]) -> String {
    let mut args = fluent_bundle::FluentArgs::new();
    for (name, value) in named {
        args.set(*name, value.clone());
    }
    crate::i18n::tr_args(id, &args)
}

/// The server's reason, without the layers of context around it.
fn short_error(err: &anyhow::Error) -> String {
    let text = err.root_cause().to_string();
    text.strip_prefix("Error: ").unwrap_or(&text).to_string()
}

/// `at`, seconds since the Unix epoch, as the clock here reads it.
fn clock(at: u64) -> String {
    use chrono::TimeZone as _;
    chrono::Local
        .timestamp_opt(at as i64, 0)
        .single()
        .map(|time| time.format("%H:%M").to_string())
        .unwrap_or_default()
}

/// The picks a server offers, in the order they are shown: groups, then
/// the other accounts, then the person's own, which a share always admits
/// and which is there for watching from another login of it.
fn candidates_from(response: ShareCandidatesResponse) -> Vec<Candidate> {
    let me = response.me;
    let mut groups = response.groups;
    groups.sort();
    groups.dedup();
    let mut accounts: Vec<String> = response
        .accounts
        .into_iter()
        .filter(|name| *name != me)
        .collect();
    accounts.sort();
    accounts.dedup();
    let mut list: Vec<Candidate> = groups
        .into_iter()
        .map(|name| Candidate {
            name,
            group: true,
            me: false,
        })
        .collect();
    list.extend(accounts.into_iter().map(|name| Candidate {
        name,
        group: false,
        me: false,
    }));
    list.push(Candidate {
        name: me,
        group: false,
        me: true,
    });
    list
}

fn audience_label(who: &ShareAudience) -> String {
    match who {
        ShareAudience::User(name) => name.clone(),
        ShareAudience::Group(name) => tr_with("share-panel-group", &[("name", name.clone())]),
    }
}

/// A box that ticks. Unticked, only its outline: a filled one reads as a
/// dark block on the list.
fn draw_check(
    ctx: &DrawContext,
    layers: &mut TripleLayerQuadAllocator,
    palette: UiPalette,
    x: f32,
    y: f32,
    size: f32,
    on: bool,
) -> anyhow::Result<()> {
    let radius = ctx.px(CHECK_RADIUS);
    if on {
        ctx.draw_rounded_rect(layers, 0, x, y, size, size, palette.accent, radius)?;
        let glyph = (size * 0.7).round();
        ctx.draw_svg_icon(
            layers,
            SvgIcon::Check,
            x + (size - glyph) / 2.0,
            y + (size - glyph) / 2.0,
            glyph,
            palette.on_accent,
        )
    } else {
        ctx.draw_rounded_frame(
            layers,
            0,
            x,
            y,
            size,
            size,
            LinearRgba::TRANSPARENT,
            palette.muted_text.mul_alpha(0.8),
            radius,
        )
    }
}

impl SharePanel {
    /// The panel of a pane not yet shared: who, and how. Returns where it
    /// ends.
    fn paint_start(
        &mut self,
        look: &Look,
        layers: &mut TripleLayerQuadAllocator,
        y: f32,
    ) -> anyhow::Result<f32> {
        use crate::i18n::tr;
        let palette = look.palette;
        let (x, w) = (look.x, look.width);
        let mut y = y;
        look.title_text(
            layers,
            x,
            y,
            &tr("share-panel-title"),
            palette.text,
            w,
        )?;
        y += look.title_line + look.px(TITLE_GAP);
        y = look.paragraph(
            layers,
            look.body,
            &tr("share-panel-lead"),
            y,
            palette.secondary_text,
            4,
        )?;

        y += look.px(SECTION_GAP);
        look.text(
            layers,
            look.strong,
            x,
            y,
            &tr("share-panel-who"),
            palette.text,
            w,
        )?;
        y += look.line + look.px(LABEL_GAP);
        if matches!(self.candidates, Candidates::Asking) {
            look.text(
                layers,
                look.body,
                x,
                y,
                &tr("share-panel-loading"),
                palette.muted_text,
                w,
            )?;
            y += look.line;
        }
        // Failed, there is nothing to show; the error below says why.
        y = self.paint_candidates(look, layers, y)?;

        y += look.px(SECTION_GAP);
        look.text(
            layers,
            look.strong,
            x,
            y,
            &tr("share-panel-ends"),
            palette.text,
            w,
        )?;
        y += look.line + look.px(LABEL_GAP);
        y = self.paint_ends(look, layers, y)?;

        y = self.paint_error(look, layers, y)?;
        y += look.px(SECTION_GAP);
        let can_start = self.can_start();
        self.paint_buttons(
            look,
            layers,
            look.x + look.width,
            y,
            look.px(BUTTON_H),
            &[
                (PanelAction::Cancel, tr("share-panel-cancel"), false, true),
                (PanelAction::Start, tr("share-panel-start"), true, can_start),
            ],
            palette,
        )?;
        Ok(y + look.px(BUTTON_H))
    }

    /// The accounts and groups to pick from, with a field finding one when
    /// there are many.
    fn paint_candidates(
        &mut self,
        look: &Look,
        layers: &mut TripleLayerQuadAllocator,
        y: f32,
    ) -> anyhow::Result<f32> {
        use crate::i18n::tr;
        let ctx = look.ctx;
        let palette = look.palette;
        let (x, w) = (look.x, look.width);
        let mut y = y;
        let searching = self.searching();
        let Candidates::Ready(list) = &self.candidates else {
            return Ok(y);
        };

        if searching {
            let h = look.px(INPUT_H);
            let glyph = look.line.clamp(look.px(18.0), look.px(24.0));
            let text_pad = look.px(16.0) + glyph + look.px(10.0);
            let tokens = UiTokens {
                control_radius: h / 2.0,
                ..UiTokens::for_dpi(ctx.dimensions.dpi)
            };
            let text = self.search.text().to_string();
            draw_text_input(
                ctx,
                layers,
                look.body,
                &mut self.widgets,
                &self.interaction,
                palette,
                tokens,
                text_pad,
                true,
                TextInputSpec {
                    placeholder: &tr("share-panel-search"),
                    text: &text,
                    rect: rect(x, y, w, h),
                    focused: true,
                    selected_all: false,
                    action: PanelAction::Search,
                },
                Some(InputCaret {
                    cursor: self.search.cursor,
                    selection: None,
                }),
            )?;
            ctx.draw_svg_icon(
                layers,
                SvgIcon::Search,
                x + look.px(16.0),
                y + (h - glyph) / 2.0,
                glyph,
                palette.muted_text,
            )?;
            y += h + look.px(LABEL_GAP);
        }

        let needle = self.search.text().to_lowercase();
        let rows: Vec<usize> = list
            .iter()
            .enumerate()
            .filter(|(_, candidate)| {
                needle.is_empty() || candidate.name.to_lowercase().contains(&needle)
            })
            .map(|(index, _)| index)
            .collect();
        let row_h = look.line + look.px(ROW_EXTRA);
        let pad = look.px(LIST_PAD);
        let shown = rows.len().clamp(1, LIST_ROWS);
        let viewport = rect(x, y + pad, w, shown as f32 * row_h);
        let box_h = viewport.size.height + pad * 2.0;
        ctx.draw_rounded_rect(
            layers,
            0,
            x,
            y,
            w,
            box_h,
            look.inset,
            look.px(LIST_RADIUS),
        )?;
        self.list
            .set_extents(viewport.size.height, rows.len() as f32 * row_h);
        self.list_area = viewport;

        if rows.is_empty() {
            look.text(
                layers,
                look.body,
                x + look.px(LIST_INSET),
                viewport.origin.y + (row_h - look.line) / 2.0,
                &tr("share-panel-no-match"),
                palette.muted_text,
                w - look.px(LIST_INSET) * 2.0,
            )?;
            return Ok(y + box_h);
        }

        // Drawn aside and cut to the list, which scrolls under its edges.
        let mut held = HeapQuadAllocator::default();
        let first_widget = self.widgets.len();
        {
            let mut row_layers = TripleLayerQuadAllocator::Heap(&mut held);
            for (place, &index) in rows.iter().enumerate() {
                let row_y = viewport.origin.y + place as f32 * row_h - self.list.offset;
                if row_y + row_h <= viewport.min_y() || row_y >= viewport.max_y() {
                    continue;
                }
                let action = PanelAction::Pick(index);
                self.widgets
                    .push(rect(x, row_y, w, row_h), WidgetKind::SidebarRow, action);
                if self.interaction.hovered == Some(action) {
                    let inset = look.px(6.0);
                    ctx.draw_rounded_rect(
                        &mut row_layers,
                        0,
                        x + inset,
                        row_y,
                        w - inset * 2.0,
                        row_h,
                        look.inset_hover,
                        look.px(12.0),
                    )?;
                }
                let check = look.px(CHECK_SIZE);
                let check_x = x + look.px(LIST_INSET);
                draw_check(
                    ctx,
                    &mut row_layers,
                    palette,
                    check_x,
                    row_y + (row_h - check) / 2.0,
                    check,
                    self.picked.get(index).copied().unwrap_or(false),
                )?;
                let icon = look.px(ROW_ICON);
                let icon_x = check_x + check + look.px(16.0);
                let candidate = &list[index];
                ctx.draw_svg_icon(
                    &mut row_layers,
                    if candidate.group {
                        SvgIcon::Users
                    } else {
                        SvgIcon::User
                    },
                    icon_x,
                    row_y + (row_h - icon) / 2.0,
                    icon,
                    palette.secondary_text,
                )?;
                let text_x = icon_x + icon + look.px(12.0);
                look.text(
                    &mut row_layers,
                    look.body,
                    text_x,
                    row_y + (row_h - look.line) / 2.0,
                    &candidate.label(),
                    palette.text,
                    (x + w - look.px(LIST_INSET) - text_x).max(0.0),
                )?;
            }
        }
        let clip = QuadClipRect::from_top_left_pixels(
            viewport.min_x(),
            viewport.min_y(),
            viewport.max_x(),
            viewport.max_y(),
            &ctx.dimensions,
        );
        held.apply_to_clipped(layers, clip, 1.0)?;
        self.widgets.clip_since(first_widget, viewport);
        Ok(y + box_h)
    }

    /// When the share ends by itself: one of `ENDS`, as a segmented
    /// control.
    fn paint_ends(
        &mut self,
        look: &Look,
        layers: &mut TripleLayerQuadAllocator,
        y: f32,
    ) -> anyhow::Result<f32> {
        let ctx = look.ctx;
        let palette = look.palette;
        let (x, w) = (look.x, look.width);
        let h = look.px(BUTTON_H);
        ctx.draw_rounded_rect(layers, 0, x, y, w, h, look.inset, h / 2.0)?;
        let inset = look.px(4.0);
        let segment_w = (w - inset * 2.0) / ENDS.len() as f32;
        let segment_h = h - inset * 2.0;
        for (index, (_, key)) in ENDS.iter().enumerate() {
            let segment_x = x + inset + index as f32 * segment_w;
            let action = PanelAction::Ends(index);
            self.widgets.push(
                rect(segment_x, y + inset, segment_w, segment_h),
                WidgetKind::Button,
                action,
            );
            let on = self.ends == index;
            if on {
                // Raised off the track in grey, as a segmented control
                // shows its pick: the accent is the Start button's.
                ctx.draw_rounded_frame(
                    layers,
                    0,
                    segment_x,
                    y + inset,
                    segment_w,
                    segment_h,
                    crate::ui::color::lightness_step(look.inset, 0.12),
                    palette.separator,
                    segment_h / 2.0,
                )?;
            } else if self.interaction.hovered == Some(action) {
                ctx.draw_rounded_rect(
                    layers,
                    0,
                    segment_x,
                    y + inset,
                    segment_w,
                    segment_h,
                    look.inset_hover,
                    segment_h / 2.0,
                )?;
            }
            let label = crate::i18n::tr(key);
            let room = (segment_w - look.px(16.0)).max(0.0);
            let text_w = look.measure(look.body, &label).min(room);
            look.text(
                layers,
                look.body,
                segment_x + (segment_w - text_w) / 2.0,
                y + (h - look.line) / 2.0,
                &label,
                if on {
                    palette.text
                } else {
                    palette.secondary_text
                },
                room,
            )?;
        }
        Ok(y + h)
    }

    fn paint_error(
        &self,
        look: &Look,
        layers: &mut TripleLayerQuadAllocator,
        y: f32,
    ) -> anyhow::Result<f32> {
        let Some(error) = &self.error else {
            return Ok(y);
        };
        let text = tr_with("share-panel-failed", &[("reason", error.clone())]);
        look.paragraph(
            layers,
            look.body,
            &text,
            y + look.px(SECTION_GAP),
            look.palette.danger,
            4,
        )
    }

    /// A row of buttons ending at `right`, in the order given: each its
    /// action, label, whether it is the primary one and whether it can be
    /// pressed.
    #[allow(clippy::too_many_arguments)]
    fn paint_buttons(
        &mut self,
        look: &Look,
        layers: &mut TripleLayerQuadAllocator,
        right: f32,
        y: f32,
        height: f32,
        buttons: &[(PanelAction, String, bool, bool)],
        palette: UiPalette,
    ) -> anyhow::Result<()> {
        let gap = look.px(BUTTON_GAP);
        let total: f32 = buttons
            .iter()
            .map(|(_, label, _, _)| look.button_width(label))
            .sum::<f32>()
            + gap * buttons.len().saturating_sub(1) as f32;
        let mut x = right - total;
        for (action, label, primary, enabled) in buttons {
            let width = look.button_width(label);
            let state = self.state(*action, *enabled);
            draw_button(
                look.ctx,
                layers,
                look.body,
                &mut self.widgets,
                palette,
                ButtonSpec {
                    label,
                    action: *action,
                    rect: rect(x, y, width, height),
                    state,
                    kind: WidgetKind::Button,
                    // Grey until it can be pressed: a dimmed blue still
                    // outshines the button beside it.
                    variant: if *primary && *enabled {
                        ButtonVariant::Primary
                    } else {
                        ButtonVariant::Secondary
                    },
                },
            )?;
            x += width + gap;
        }
        Ok(())
    }

    /// The panel of a shared pane: with whom, who asks to type, who types
    /// and who watches.
    fn paint_manage(
        &mut self,
        look: &Look,
        layers: &mut TripleLayerQuadAllocator,
        y: f32,
        share: &ShareInfo,
    ) -> anyhow::Result<f32> {
        use crate::i18n::tr;
        let ctx = look.ctx;
        let palette = look.palette;
        let (x, w) = (look.x, look.width);
        let mut y = y;
        look.title_text(
            layers,
            x,
            y,
            &tr("share-panel-sharing"),
            palette.text,
            w,
        )?;
        y += look.title_line + look.px(TITLE_GAP);
        let who = share
            .audience
            .iter()
            .map(audience_label)
            .collect::<Vec<_>>()
            .join(&tr("share-panel-sep"));
        let mut summary = tr_with("share-panel-with", &[("who", who)]);
        if let Some(expires) = share.expires {
            summary.push_str(" · ");
            summary.push_str(&tr_with("share-panel-ends-at", &[("time", clock(expires))]));
        }
        y = look.paragraph(layers, look.body, &summary, y, palette.secondary_text, 3)?;

        for asking in &share.requests {
            y += look.px(SECTION_GAP);
            y = self.paint_request(look, layers, y, asking)?;
        }

        if let Some(controller) = &share.controller {
            y += look.px(SECTION_GAP);
            let text = match controller.until {
                Some(until) => tr_with(
                    "share-panel-typing-until",
                    &[("name", controller.name.clone()), ("time", clock(until))],
                ),
                None => tr_with("share-panel-typing", &[("name", controller.name.clone())]),
            };
            let label = tr("share-panel-take-back");
            let button_w = look.button_width(&label);
            let h = look.px(SMALL_BUTTON_H);
            let icon = look.px(ROW_ICON);
            ctx.draw_svg_icon(
                layers,
                SvgIcon::Keyboard,
                x,
                y + (h - icon) / 2.0,
                icon,
                palette.accent,
            )?;
            let text_x = x + icon + look.px(12.0);
            look.text(
                layers,
                look.strong,
                text_x,
                y + (h - look.line) / 2.0,
                &text,
                palette.text,
                (x + w - button_w - look.px(16.0) - text_x).max(0.0),
            )?;
            self.paint_buttons(
                look,
                layers,
                x + w,
                y,
                h,
                &[(PanelAction::TakeBack, label, false, true)],
                palette,
            )?;
            y += h;
        }

        y += look.px(SECTION_GAP);
        look.text(
            layers,
            look.strong,
            x,
            y,
            &tr_with(
                "share-panel-watching",
                &[("count", share.viewers.len().to_string())],
            ),
            palette.text,
            w,
        )?;
        y += look.line + look.px(LABEL_GAP);
        if share.viewers.is_empty() {
            look.text(
                layers,
                look.body,
                x,
                y,
                &tr("share-panel-nobody"),
                palette.muted_text,
                w,
            )?;
            y += look.line;
        }
        let remove = tr("share-panel-remove");
        let remove_w = look.button_width(&remove);
        let h = look.px(SMALL_BUTTON_H);
        for viewer in &share.viewers {
            let icon = look.px(ROW_ICON);
            ctx.draw_svg_icon(
                layers,
                SvgIcon::User,
                x,
                y + (h - icon) / 2.0,
                icon,
                palette.secondary_text,
            )?;
            let text_x = x + icon + look.px(12.0);
            look.text(
                layers,
                look.body,
                text_x,
                y + (h - look.line) / 2.0,
                &viewer.name,
                palette.text,
                (x + w - remove_w - look.px(16.0) - text_x).max(0.0),
            )?;
            self.paint_buttons(
                look,
                layers,
                x + w,
                y,
                h,
                &[(
                    PanelAction::Remove(viewer.viewer_id),
                    remove.clone(),
                    false,
                    true,
                )],
                palette,
            )?;
            y += h + look.px(8.0);
        }

        y = self.paint_error(look, layers, y)?;
        y += look.px(SECTION_GAP);
        let h = look.px(BUTTON_H);
        // Ending it reads as what it is, while staying a button.
        let mut warning = palette;
        warning.text = palette.danger;
        let stop = tr("share-panel-stop");
        self.paint_buttons(
            look,
            layers,
            x + look.button_width(&stop),
            y,
            h,
            &[(PanelAction::Stop, stop, false, true)],
            warning,
        )?;
        self.paint_buttons(
            look,
            layers,
            x + w,
            y,
            h,
            &[(PanelAction::Close, tr("share-panel-close"), false, true)],
            palette,
        )?;
        Ok(y + h)
    }

    /// A viewer asking to type, with the two answers.
    fn paint_request(
        &mut self,
        look: &Look,
        layers: &mut TripleLayerQuadAllocator,
        y: f32,
        asking: &ShareViewer,
    ) -> anyhow::Result<f32> {
        use crate::i18n::tr;
        let ctx = look.ctx;
        let palette = look.palette;
        let pad = look.px(BOX_PAD);
        let (x, w) = (look.x, look.width);
        let inner = w - pad * 2.0;
        let name = [("name", asking.name.clone())];
        let head = look.wrap(look.strong, &tr_with("share-panel-asks", &name), inner, 2);
        let note = look.wrap(
            look.body,
            &tr_with("share-panel-asks-note", &name),
            inner,
            3,
        );
        let button_h = look.px(SMALL_BUTTON_H);
        let box_h = pad
            + (head.len() + note.len()) as f32 * look.line
            + look.px(6.0)
            + look.px(16.0)
            + button_h
            + pad;
        ctx.draw_rounded_rect(
            layers,
            0,
            x,
            y,
            w,
            box_h,
            look.inset,
            look.px(BOX_RADIUS),
        )?;
        let mut line_y = y + pad;
        for line in &head {
            look.text(layers, look.strong, x + pad, line_y, line, palette.text, inner)?;
            line_y += look.line;
        }
        line_y += look.px(6.0);
        for line in &note {
            look.text(
                layers,
                look.body,
                x + pad,
                line_y,
                line,
                palette.secondary_text,
                inner,
            )?;
            line_y += look.line;
        }
        line_y += look.px(16.0);
        self.paint_buttons(
            look,
            layers,
            x + w - pad,
            line_y,
            button_h,
            &[
                (
                    PanelAction::Deny(asking.viewer_id),
                    tr("share-panel-deny"),
                    false,
                    true,
                ),
                (
                    PanelAction::Grant(asking.viewer_id),
                    tr("share-panel-grant"),
                    true,
                    true,
                ),
            ],
            palette,
        )?;
        Ok(y + box_h)
    }
}

impl TermWindow {
    /// Whether `pane` can be shared: sharing is on, and the pane lives on a
    /// mux server that shares (see `share_target`).
    pub(crate) fn pane_can_share(&self, pane: &Arc<dyn Pane>) -> bool {
        sharing_enabled() && share_target(pane).is_some() && !self.shows_a_watch()
    }

    /// Whether the window shows a watch of another account's share: what
    /// its panes show is that account's to share, not ours. Told by the
    /// workspace, so a window that has not heard from the viewer yet --
    /// one just restarted -- knows it too.
    fn shows_a_watch(&self) -> bool {
        Mux::get().get_window(self.mux_window_id).is_some_and(|window| {
            crate::workspace_threads::is_share_view_workspace(window.get_workspace())
        })
    }

    /// Whether `pane` watches a pane another account shares: its bar has
    /// the watch's pill where the share button would be.
    pub(crate) fn pane_is_watching_share(&self, pane_id: PaneId) -> bool {
        self.share.watching.contains_key(&pane_id)
    }

    /// The share `pane` is in, if this window knows of one.
    fn share_of(&self, pane: &Arc<dyn Pane>) -> Option<(DomainId, &ShareInfo)> {
        let (domain_id, remote) = share_target(pane)?;
        self.share
            .owned
            .get(&domain_id)?
            .iter()
            .find(|share| share.pane_id == remote)
            .map(|share| (domain_id, share))
    }

    /// How `pane`'s share button shows: `None` when it is not shared,
    /// `Some(asking)` when it is, `asking` while a viewer waits to type.
    pub(crate) fn pane_share_state(&self, pane: &Arc<dyn Pane>) -> Option<bool> {
        self.share_of(pane)
            .map(|(_, share)| !share.requests.is_empty())
    }

    pub(crate) fn share_panel_open(&self) -> bool {
        self.share.panel.is_some()
    }

    /// The share button was pressed: open its panel, or close it.
    pub(crate) fn toggle_share_panel(&mut self, pane_id: PaneId) {
        if self.share.panel.take().is_some() || !sharing_enabled() {
            self.invalidate_window();
            return;
        }
        let Some(pane) = Mux::get().get_pane(pane_id) else {
            return;
        };
        if self.shows_a_watch() {
            return;
        }
        let Some((domain_id, remote_pane_id)) = share_target(&pane) else {
            return;
        };
        let mut panel = SharePanel::new(pane_id, domain_id, remote_pane_id);
        if let Some(list) = self.share.candidates.get(&domain_id) {
            panel.take_candidates(list.clone());
        }
        self.share.panel = Some(panel);
        // Asked now, so the panel opens on what is true.
        self.share.owned_asked = None;
        self.ask_after_shares();
        self.ask_share_candidates(domain_id);
        self.invalidate_window();
    }

    /// The panel's fonts: the menus' size for its words, a step up for its
    /// title.
    fn share_panel_fonts(
        &self,
    ) -> anyhow::Result<(Rc<LoadedFont>, Rc<LoadedFont>, Rc<LoadedFont>)> {
        let native = crate::native_settings::load_shared();
        let size = crate::native_settings::home_font_size(&native) * BODY_SCALE;
        Ok((
            self.fonts
                .title_font_with_size_and_weight(size, 400)
                .context("share panel font")?,
            self.fonts
                .title_font_with_size_and_weight(size, 600)
                .context("share panel strong font")?,
            self.fonts
                .title_font_with_size_and_weight(size * TITLE_SCALE, 600)
                .context("share panel title font")?,
        ))
    }

    pub(crate) fn close_share_panel(&mut self) {
        if self.share.panel.take().is_some() {
            self.ui_items
                .retain(|item| !matches!(item.item_type, UIItemType::SharePanel));
            self.invalidate_window();
        }
    }

    /// Where `pane_id`'s share button stands this frame.
    fn share_button_rect(&self, pane_id: PaneId) -> Option<RectF> {
        self.ui_items
            .iter()
            .rev()
            .find(|item| {
                matches!(
                    item.item_type,
                    UIItemType::PaneNav {
                        pane_id: id,
                        action: PaneNavAction::Share,
                        ..
                    } if id == pane_id
                )
            })
            .map(|item| {
                rect(
                    item.x as f32,
                    item.y as f32,
                    item.width as f32,
                    item.height as f32,
                )
            })
    }

    pub(crate) fn paint_share_panel(&mut self) -> anyhow::Result<()> {
        let Some(pane_id) = self.share.panel.as_ref().map(|panel| panel.pane_id) else {
            return Ok(());
        };
        // Whatever takes the window over, or takes the pane out of sight,
        // takes the panel with it.
        let Some(button) = self.share_button_rect(pane_id).filter(|_| {
            self.command_palette.is_none()
                && self.modal.borrow().is_none()
                && !self.content_view_foreground()
        }) else {
            self.close_share_panel();
            return Ok(());
        };
        let share = Mux::get()
            .get_pane(pane_id)
            .and_then(|pane| self.share_of(&pane).map(|(_, share)| share.clone()));
        let Some(mut panel) = self.share.panel.take() else {
            return Ok(());
        };
        let result = self.paint_share_panel_with(&mut panel, button, share.as_ref());
        self.share.panel = Some(panel);
        result
    }

    fn paint_share_panel_with(
        &mut self,
        panel: &mut SharePanel,
        button: RectF,
        share: Option<&ShareInfo>,
    ) -> anyhow::Result<()> {
        panel.widgets.clear();
        // What routes every pointer event to the panel, and keeps a click
        // from reaching what is under it.
        self.ui_items
            .retain(|item| !matches!(item.item_type, UIItemType::SharePanel));
        self.ui_items.push(UIItem {
            x: 0,
            y: 0,
            width: self.dimensions.pixel_width,
            height: self.dimensions.pixel_height,
            item_type: UIItemType::SharePanel,
        });

        let now = Instant::now();
        let list_moving = panel.list.advance_animation(now);
        let body_moving = panel.body.advance_animation(now);
        if list_moving || body_moving {
            self.update_next_frame_time(Some(now + Duration::from_millis(8)));
        }

        let (body, strong, title) = self.share_panel_fonts()?;
        let body_metrics = RenderMetrics::with_font_metrics(&body.metrics());
        let title_metrics = RenderMetrics::with_font_metrics(&title.metrics());
        let mut palette = crate::termwindow::ui::context_menu::context_menu_palette(self.chrome());
        crate::native_settings::apply_ui_colors(&mut palette, self.config.ui_colors.as_ref());

        let gl_state = self.render_state.as_ref().context("share panel render state")?;
        let layer = gl_state
            .layer_for_zindex(PANEL_ZINDEX)
            .context("share panel layer")?;
        let mut layers = layer.quad_allocator();
        let ctx = DrawContext::new(gl_state, self.dimensions, &body_metrics);
        let title_ctx = ctx.with_metrics(&title_metrics);

        let window_w = self.dimensions.pixel_width as f32;
        let window_h = self.dimensions.pixel_height as f32;
        let margin = ctx.px(CARD_MARGIN);
        let card_w = ctx.px(CARD_WIDTH).min(window_w - margin * 2.0).max(1.0);
        // Under the button, its right edge with the button's.
        let card_x = (button.max_x() - card_w)
            .min(window_w - margin - card_w)
            .max(margin);
        let top = button.max_y() + ctx.px(CARD_DROP);
        let pad = ctx.px(CARD_PAD);
        // A step off the card in perceived lightness, the way the card
        // lifts off the window: a grey washed over it at an alpha lands far
        // lighter, as linear light adds up.
        let toward = if palette.is_dark() { 1.0 } else { -1.0 };
        let look = Look {
            ctx: &ctx,
            title_ctx: &title_ctx,
            body: &body,
            strong: &strong,
            title: &title,
            palette,
            inset: crate::ui::color::lightness_step(palette.control_bg, 0.05 * toward),
            inset_hover: crate::ui::color::lightness_step(palette.control_bg, 0.10 * toward),
            x: card_x + pad,
            width: card_w - pad * 2.0,
            line: body_metrics.cell_size.height as f32,
            title_line: title_metrics.cell_size.height as f32,
            shaped: std::cell::RefCell::new(std::mem::take(&mut panel.shaped)),
        };

        // Laid out from the top, scrolled, before its height is known; then
        // moved up as far as it takes to stay in the window, and cut to the
        // window when it is taller, to be scrolled through.
        let scrolled = panel.body.offset;
        let content_top = top + pad - scrolled;
        let mut held = HeapQuadAllocator::default();
        let bottom = {
            let mut held_layers = TripleLayerQuadAllocator::Heap(&mut held);
            match share {
                None => panel.paint_start(&look, &mut held_layers, content_top),
                Some(share) => panel.paint_manage(&look, &mut held_layers, content_top, share),
            }
        };
        panel.shaped = look.shaped.into_inner();
        let bottom = bottom?;
        let content_h = bottom - content_top;
        let card_h = (content_h + pad * 2.0).min((window_h - margin * 2.0).max(pad * 4.0));
        let card_top = top.min(window_h - margin - card_h).max(margin);
        let shift = card_top - top;
        let viewport = rect(card_x, card_top + pad, card_w, card_h - pad * 2.0);
        panel.body.set_extents(viewport.size.height, content_h);
        if (panel.body.offset - scrolled).abs() > 0.5 {
            // It got shorter under its scroll: drawn again where it now is.
            self.invalidate_window();
        }
        let card = rect(card_x, card_top, card_w, card_h);
        let shadow = if palette.is_dark() { 0.58 } else { 0.43 };
        ctx.draw_elevated_surface(
            &mut layers,
            0,
            card,
            palette.control_bg,
            palette.control_border,
            LinearRgba::with_components(0.0, 0.0, 0.0, shadow),
            ctx.px(CARD_RADIUS),
        )?;
        let clip = QuadClipRect::from_top_left_pixels(
            viewport.min_x(),
            viewport.min_y(),
            viewport.max_x(),
            viewport.max_y(),
            &self.dimensions,
        );
        held.apply_to_clipped_at(&mut layers, 0.0, shift, clip, 1.0)?;
        panel.widgets.shift(0.0, shift);
        panel.widgets.clip_since(0, viewport);
        panel.list_area = panel
            .list_area
            .translate(euclid::vec2(0.0, shift))
            .intersection(&viewport)
            .unwrap_or_else(|| rect(0.0, 0.0, 0.0, 0.0));
        panel.card = card;
        Ok(())
    }

    /// The panel owns the pointer while open, as the command palette does.
    pub(crate) fn mouse_event_share_panel(
        &mut self,
        event: &MouseEvent,
        context: &dyn WindowOps,
    ) -> bool {
        let scale = self.ui_f32(1.0);
        let Some(panel) = self.share.panel.as_mut() else {
            return false;
        };
        let x = event.coords.x as f32;
        let y = event.coords.y as f32;
        let hit = panel.widgets.hit_test(x, y).map(|target| target.action);
        let mut close = false;
        let mut act = None;
        match event.kind {
            MouseEventKind::Move => {
                let cursor = match hit {
                    Some(PanelAction::Search) => MouseCursor::Text,
                    Some(_) => MouseCursor::Hand,
                    None => MouseCursor::Arrow,
                };
                context.set_cursor(Some(cursor));
                if panel.interaction.hovered != hit {
                    panel.interaction.hovered = hit;
                    context.invalidate();
                }
            }
            MouseEventKind::Press(press) => {
                if hit.is_none() && !contains(panel.card, x, y) {
                    // A click away closes it, and goes no further.
                    close = true;
                } else if press == MousePress::Left {
                    panel.interaction.pressed = hit;
                    context.invalidate();
                }
            }
            MouseEventKind::Release(MousePress::Left) => {
                let pressed = panel.interaction.pressed.take();
                context.invalidate();
                act = hit.filter(|_| hit == pressed);
            }
            MouseEventKind::VertWheel(_) => {
                // The list of accounts scrolls under the pointer, and the
                // panel itself elsewhere on it, when it is taller than the
                // window.
                let scroll = if contains(panel.list_area, x, y) && panel.list.has_overflow() {
                    Some(&mut panel.list)
                } else if contains(panel.card, x, y) {
                    Some(&mut panel.body)
                } else {
                    None
                };
                if scroll.is_some_and(|scroll| apply_wheel(event, scroll, scale)) {
                    context.invalidate();
                }
            }
            _ => {}
        }
        if close {
            self.close_share_panel();
        }
        if let Some(action) = act {
            self.share_panel_act(action);
        }
        true
    }

    /// The panel takes the keyboard while open: Escape closes it, Enter
    /// shares, and typing finds an account.
    pub(crate) fn share_panel_key(&mut self, key: KeyCode, mods: Modifiers) {
        let Some(panel) = self.share.panel.as_mut() else {
            return;
        };
        let pane_id = panel.pane_id;
        let searching = panel.searching();
        let plain = !mods.intersects(Modifiers::CTRL | Modifiers::SUPER | Modifiers::ALT);
        match key {
            KeyCode::Escape => self.close_share_panel(),
            KeyCode::Enter => {
                let starting = Mux::get()
                    .get_pane(pane_id)
                    .is_some_and(|pane| self.share_of(&pane).is_none());
                if starting {
                    self.share_panel_act(PanelAction::Start);
                }
            }
            KeyCode::Backspace if searching => {
                panel.search.caret_backspace();
                panel.list.reset();
                self.invalidate_window();
            }
            KeyCode::Char(c) if searching && plain && !c.is_control() => {
                panel.search.caret_insert(&c.to_string(), false);
                panel.list.reset();
                self.invalidate_window();
            }
            _ => {}
        }
    }

    /// Text composed by an input method, for the field finding an account.
    pub(crate) fn share_panel_text(&mut self, text: &str) {
        let Some(panel) = self.share.panel.as_mut() else {
            return;
        };
        let text: String = text.chars().filter(|c| !c.is_control()).collect();
        if panel.searching() && !text.is_empty() {
            panel.search.caret_insert(&text, false);
            panel.list.reset();
            self.invalidate_window();
        }
    }

    fn share_panel_act(&mut self, action: PanelAction) {
        let Some(panel) = self.share.panel.as_mut() else {
            return;
        };
        if panel.busy && !matches!(action, PanelAction::Cancel | PanelAction::Close) {
            return;
        }
        let pane_id = panel.pane_id;
        match action {
            PanelAction::Pick(index) => {
                if let Some(picked) = panel.picked.get_mut(index) {
                    *picked = !*picked;
                }
            }
            PanelAction::Ends(index) => panel.ends = index.min(ENDS.len() - 1),
            PanelAction::Search => {}
            PanelAction::Cancel | PanelAction::Close => self.close_share_panel(),
            PanelAction::Start => {
                if !panel.can_start() {
                    return;
                }
                let start = ShareStart {
                    pane_id: panel.remote_pane_id,
                    audience: panel.audience(),
                    history: false,
                    expires_in_secs: ENDS[panel.ends].0,
                    from_desktop: true,
                };
                let domain_id = panel.domain_id;
                self.share_call(domain_id, pane_id, move |client| async move {
                    client.share_start(start).await.map(|_| ())
                });
            }
            PanelAction::Grant(_)
            | PanelAction::Deny(_)
            | PanelAction::TakeBack
            | PanelAction::Remove(_)
            | PanelAction::Stop => {
                let Some(pane) = Mux::get().get_pane(pane_id) else {
                    return;
                };
                let Some((domain_id, share_id)) = self
                    .share_of(&pane)
                    .map(|(domain_id, share)| (domain_id, share.share_id))
                else {
                    return;
                };
                match action {
                    PanelAction::Grant(viewer_id) => {
                        self.share_call(domain_id, pane_id, move |client| async move {
                            client
                                .share_grant(ShareGrant {
                                    share_id,
                                    viewer_id,
                                    for_secs: Some(HALF_HOUR),
                                    from_desktop: true,
                                })
                                .await
                                .map(|_| ())
                        })
                    }
                    PanelAction::Deny(viewer_id) => {
                        self.share_call(domain_id, pane_id, move |client| async move {
                            client
                                .share_deny(ShareDeny {
                                    share_id,
                                    viewer_id,
                                })
                                .await
                                .map(|_| ())
                        })
                    }
                    PanelAction::Remove(viewer_id) => {
                        self.share_call(domain_id, pane_id, move |client| async move {
                            client
                                .share_kick(ShareKick {
                                    share_id,
                                    viewer_id,
                                })
                                .await
                                .map(|_| ())
                        })
                    }
                    PanelAction::TakeBack => {
                        self.share_call(domain_id, pane_id, move |client| async move {
                            client
                                .share_revoke(ShareRevoke { share_id })
                                .await
                                .map(|_| ())
                        })
                    }
                    // The panel stays until the share is gone: a stop that
                    // fails is said there, and the share is still shown.
                    _ => self.share_call_closing(domain_id, pane_id, true, move |client| async move {
                        client.share_stop(ShareStop { share_id }).await.map(|_| ())
                    }),
                }
            }
        }
        self.invalidate_window();
    }

    /// Run `call` on `domain_id`'s server, then ask it for its shares, so
    /// the panel and the button show what it did. A failure is said in the
    /// panel of the pane it was for.
    fn share_call<F, Fut>(&mut self, domain_id: DomainId, pane_id: PaneId, call: F)
    where
        F: FnOnce(Client) -> Fut + 'static,
        Fut: std::future::Future<Output = anyhow::Result<()>> + 'static,
    {
        self.share_call_closing(domain_id, pane_id, false, call)
    }

    /// `share_call`, closing the pane's panel once `call` has done what it
    /// was for when `close` says so; one that failed keeps it open, to say
    /// so.
    fn share_call_closing<F, Fut>(
        &mut self,
        domain_id: DomainId,
        pane_id: PaneId,
        close: bool,
        call: F,
    ) where
        F: FnOnce(Client) -> Fut + 'static,
        Fut: std::future::Future<Output = anyhow::Result<()>> + 'static,
    {
        let Some(window) = self.window.clone() else {
            return;
        };
        let client = match share_client(domain_id) {
            Ok(client) => client,
            Err(err) => return self.share_failed(pane_id, &err),
        };
        if let Some(panel) = self.share.panel.as_mut() {
            panel.busy = true;
            panel.error = None;
        }
        promise::spawn::spawn(async move {
            let result = call(client.clone()).await;
            let shares = client.share_list().await.map(|response| response.shares);
            window.notify(TermWindowNotif::Apply(Box::new(move |tw| {
                if let Some(panel) = tw.share.panel.as_mut().filter(|p| p.pane_id == pane_id) {
                    panel.busy = false;
                }
                match shares {
                    Ok(shares) => tw.note_shares(domain_id, shares),
                    Err(err) => log::debug!("asking domain {domain_id} for its shares: {err:#}"),
                }
                match result {
                    Err(err) => tw.share_failed(pane_id, &err),
                    Ok(())
                        if close
                            && tw.share.panel.as_ref().is_some_and(|p| p.pane_id == pane_id) =>
                    {
                        tw.close_share_panel()
                    }
                    Ok(()) => {}
                }
                tw.invalidate_window();
            })));
        })
        .detach();
    }

    /// Something about sharing `pane_id` went wrong: say so in its panel,
    /// and in the log.
    fn share_failed(&mut self, pane_id: PaneId, err: &anyhow::Error) {
        log::error!("sharing pane {pane_id}: {err:#}");
        if let Some(panel) = self.share.panel.as_mut().filter(|p| p.pane_id == pane_id) {
            panel.error = Some(short_error(err));
        }
        self.invalidate_window();
    }

    /// Ask `domain_id`'s server who its panes can be shared with, for the
    /// panel and for the next time it opens. One ask at a time per server.
    fn ask_share_candidates(&mut self, domain_id: DomainId) {
        let Some(window) = self.window.clone() else {
            return;
        };
        let client = match share_client(domain_id) {
            Ok(client) => client,
            Err(err) => {
                if let Some(panel) = self.share.panel.as_mut().filter(|p| p.domain_id == domain_id) {
                    if matches!(panel.candidates, Candidates::Asking) {
                        panel.candidates = Candidates::Failed;
                        let pane_id = panel.pane_id;
                        self.share_failed(pane_id, &err);
                    }
                }
                return;
            }
        };
        if !self.share.candidates_asking.insert(domain_id) {
            return;
        }
        promise::spawn::spawn(async move {
            let result = client.share_candidates().await;
            window.notify(TermWindowNotif::Apply(Box::new(move |tw| {
                tw.share.candidates_asking.remove(&domain_id);
                let panel = tw
                    .share
                    .panel
                    .as_mut()
                    .filter(|panel| panel.domain_id == domain_id);
                match result {
                    Ok(response) => {
                        let list = candidates_from(response);
                        if let Some(panel) = panel {
                            let changed = !matches!(&panel.candidates, Candidates::Ready(old) if *old == list);
                            if changed {
                                panel.take_candidates(list.clone());
                                tw.invalidate_window();
                            }
                        }
                        tw.share.candidates.insert(domain_id, list);
                    }
                    Err(err) => match panel {
                        // What the panel shows stays; only one with nothing
                        // to show says why.
                        Some(panel) if matches!(panel.candidates, Candidates::Asking) => {
                            panel.candidates = Candidates::Failed;
                            let pane_id = panel.pane_id;
                            tw.share_failed(pane_id, &err);
                        }
                        _ => log::debug!("asking domain {domain_id} who to share with: {err:#}"),
                    },
                }
            })));
        })
        .detach();
    }

    /// Ahead of the share button's first press: who the active pane's
    /// server can share with, and the panel's fonts, so the panel opens
    /// whole at once.
    fn ready_share_panel(&mut self) {
        if !self.share.fonts_ready {
            self.share.fonts_ready = true;
            if let Err(err) = self.share_panel_fonts() {
                log::debug!("loading the share panel's fonts: {err:#}");
            }
        }
        let Some((domain_id, _)) = self
            .get_active_pane_or_overlay()
            .and_then(|pane| share_target(&pane))
        else {
            return;
        };
        if self.share.candidates_readied.insert(domain_id) {
            self.ask_share_candidates(domain_id);
        }
    }

    /// The servers this window's panes live on, and those holding shares it
    /// knows of: a share goes on when the window shows a thread of another
    /// server, and a request to type in it must still be heard.
    fn share_domains(&self) -> HashSet<DomainId> {
        let mut domains: HashSet<DomainId> = self.share.owned.keys().copied().collect();
        let mux = Mux::get();
        if let Some(window) = mux.get_window(self.mux_window_id) {
            domains.extend(
                window
                    .iter()
                    .flat_map(|tab| tab.iter_panes_ignoring_zoom())
                    .filter_map(|pos| share_target(&pos.pane).map(|(domain_id, _)| domain_id)),
            );
        }
        domains
    }

    /// Ask each of the window's servers for its shares, and tell of any new
    /// request to type.
    fn ask_after_shares(&mut self) {
        let Some(window) = self.window.clone() else {
            return;
        };
        self.share.owned_asked = Some(Instant::now());
        for domain_id in self.share_domains() {
            let Ok(client) = share_client(domain_id) else {
                continue;
            };
            let window = window.clone();
            promise::spawn::spawn(async move {
                let shares = match client.share_list().await {
                    Ok(response) => response.shares,
                    Err(err) => {
                        log::debug!("asking domain {domain_id} for its shares: {err:#}");
                        return;
                    }
                };
                window.notify(TermWindowNotif::Apply(Box::new(move |tw| {
                    tw.note_shares(domain_id, shares);
                })));
            })
            .detach();
        }
    }

    fn note_shares(&mut self, domain_id: DomainId, shares: Vec<ShareInfo>) {
        let mut asking_now = HashSet::new();
        for share in &shares {
            for asking in &share.requests {
                let key = (domain_id, share.share_id, asking.viewer_id);
                asking_now.insert(key);
                if TOLD.with(|told| told.borrow_mut().insert(key)) {
                    wezterm_toast_notification::persistent_toast_notification(
                        &crate::i18n::tr("share-asks-title"),
                        &tr_with("share-asks-body", &[("name", asking.name.clone())]),
                    );
                }
            }
        }
        TOLD.with(|told| {
            told.borrow_mut()
                .retain(|key| key.0 != domain_id || asking_now.contains(key))
        });
        let changed = self.share.owned.get(&domain_id) != Some(&shares);
        if shares.is_empty() {
            self.share.owned.remove(&domain_id);
        } else {
            self.share.owned.insert(domain_id, shares);
        }
        if changed {
            self.invalidate_window();
        }
    }

    /// A pane's viewer said where its watch stands.
    pub(crate) fn note_share_watch(&mut self, pane_id: PaneId, name: &str, value: &str) {
        if name != WATCH_VAR {
            return;
        }
        // Only the viewer a window opened, in a workspace of its own, speaks
        // for a watch: any other program could set the variable, and a
        // watch that ended is sent the key that closes it (see
        // `share_tick`). Kept out of sight too: the window comes back to it.
        let mux = Mux::get();
        let ours = mux
            .resolve_pane_id(pane_id)
            .is_some_and(|(_, window_id, _)| {
                mux.get_window(window_id).is_some_and(|window| {
                    crate::workspace_threads::is_share_view_workspace(window.get_workspace())
                })
            });
        if !ours {
            return;
        }
        let state = serde_json::from_str::<WatchState>(value).ok().map(|mut state| {
            // Shown in the pane's bar: a name, not a paragraph.
            state.owner = state.owner.chars().filter(|c| !c.is_control()).take(64).collect();
            state
        });
        let changed = match state {
            Some(state) => self.share.watching.insert(pane_id, state.clone()) != Some(state),
            None => self.share.watching.remove(&pane_id).is_some(),
        };
        if changed {
            self.invalidate_window();
        }
    }

    /// What a viewer pane's bar shows of its watch.
    pub(crate) fn share_pill(&self, pane_id: PaneId) -> Option<SharePill> {
        use crate::i18n::tr;
        let watch = self.share.watching.get(&pane_id)?;
        let owner = || [("owner", watch.owner.clone())];
        let pill = |label: String, icon, lit, live| SharePill {
            label,
            icon,
            lit,
            live,
        };
        Some(match watch.state.as_str() {
            "connecting" => pill(tr("share-pill-connecting"), SvgIcon::LoaderCircle, false, false),
            "watch" => pill(tr("share-pill-watch"), SvgIcon::Eye, false, true),
            "asking" => pill(tr("share-pill-asking"), SvgIcon::Eye, false, true),
            "typing" => pill(tr("share-pill-typing"), SvgIcon::Keyboard, true, true),
            "ended" => {
                let label = match watch.why.as_deref() {
                    Some(codec::share_end::STOPPED) => tr_with("share-pill-stopped", &owner()),
                    Some(codec::share_end::REMOVED) => tr_with("share-pill-removed", &owner()),
                    Some(codec::share_end::EXPIRED) => tr("share-pill-expired"),
                    _ => tr("share-pill-closed"),
                };
                pill(label, SvgIcon::CircleStop, false, true)
            }
            _ => return None,
        })
    }

    /// The pill was pressed: what the viewer's `^] r` does, which asks to
    /// type, takes the asking back or gives typing back; or, once the watch
    /// ended, the key that closes the viewer.
    pub(crate) fn press_share_pill(&mut self, pane_id: PaneId) {
        let Some(watch) = self.share.watching.get(&pane_id) else {
            return;
        };
        let keys: &[u8] = match watch.state.as_str() {
            "connecting" => return,
            "ended" => b"\r",
            _ => b"\x1dr",
        };
        let Some(pane) = Mux::get().get_pane(pane_id) else {
            return;
        };
        let written = pane.writer().write_all(keys);
        if let Err(err) = written {
            log::error!("telling the viewer in pane {pane_id}: {err:#}");
        }
    }

    /// Ask what others share with the account of the window's server.
    /// The server whose machine the window's list of panes shared with us
    /// is about: the one the window is on.
    fn incoming_target(&self) -> Option<DomainId> {
        let pane = self.get_active_pane_or_overlay()?;
        share_target(&pane).map(|(domain_id, _)| domain_id)
    }

    /// Whether the list is due to be asked for afresh: every
    /// `INCOMING_EVERY`, and at once on another server than the list's.
    fn incoming_due(&self) -> bool {
        let moved = self.incoming_target().is_some_and(|target| {
            self.share.incoming_domain != Some(target) && self.share.incoming_asking != Some(target)
        });
        moved
            || self
                .share
                .incoming_asked
                .is_none_or(|at| at.elapsed() >= INCOMING_EVERY)
    }

    fn ask_what_is_shared_with_us(&mut self) {
        let Some(window) = self.window.clone() else {
            return;
        };
        let Some(domain_id) = self.incoming_target() else {
            return;
        };
        // Another server's list is not this one's: gone at once, not shown
        // here until the answer comes.
        if self.share.incoming_domain.is_some_and(|listed| listed != domain_id) {
            self.share.incoming.clear();
            self.share.other_version.clear();
            self.share.incoming_domain = None;
            self.invalidate_window();
        }
        let Ok(client) = share_client(domain_id) else {
            return;
        };
        let domain_name = Mux::get()
            .get_domain(domain_id)
            .map(|domain| domain.domain_name().to_string())
            .unwrap_or_default();
        self.share.incoming_asked = Some(Instant::now());
        self.share.incoming_asking = Some(domain_id);
        promise::spawn::spawn(async move {
            let response = match client.share_discover().await {
                Ok(response) => response,
                Err(err) => {
                    log::debug!("asking domain {domain_id} what is shared with us: {err:#}");
                    window.notify(TermWindowNotif::Apply(Box::new(move |tw| {
                        if tw.share.incoming_asking == Some(domain_id) {
                            tw.share.incoming_asking = None;
                        }
                    })));
                    return;
                }
            };
            let rows: Vec<IncomingRow> = response
                .owners
                .iter()
                .flat_map(|owner| {
                    let domain_name = &domain_name;
                    // The title is another account's program's to set.
                    owner.shares.iter().map(move |share| IncomingRow {
                        owner: owner.owner.clone(),
                        share_id: share.share_id,
                        title: share.title.chars().filter(|c| !c.is_control()).collect(),
                        workspace: view_workspace(domain_name, &owner.owner, share.share_id),
                    })
                })
                .collect();
            let watch_program = response.watch_program;
            // Account names as the server's machine gives them; drawn
            // without control characters all the same.
            let other_version: Vec<String> = response
                .other_version
                .iter()
                .map(|owner| owner.chars().filter(|c| !c.is_control()).collect())
                .collect();
            window.notify(TermWindowNotif::Apply(Box::new(move |tw| {
                if tw.share.incoming_asking == Some(domain_id) {
                    tw.share.incoming_asking = None;
                }
                // An answer from a server the window has left since asking
                // is not its list, nor one to open a watch from.
                if tw.incoming_target() != Some(domain_id) {
                    return;
                }
                let changed = tw.share.incoming.len() != rows.len()
                    || tw
                        .share
                        .incoming
                        .iter()
                        .zip(&rows)
                        .any(|(a, b)| {
                            a.owner != b.owner
                                || a.share_id != b.share_id
                                || a.title != b.title
                                || a.workspace != b.workspace
                        })
                    || tw.share.other_version != other_version;
                tw.share.incoming = rows;
                tw.share.other_version = other_version;
                tw.share.incoming_domain = Some(domain_id);
                tw.share.watch_program = watch_program;
                if changed {
                    tw.invalidate_window();
                }
            })));
        })
        .detach();
    }

    /// From the window's heartbeat: ask after the shares while there are
    /// any, and after what is shared with us while the sidebar is open;
    /// with sharing off, stop what turning it off left running; forget the
    /// watches of panes that are gone.
    pub(crate) fn share_tick(&mut self) {
        let due = |asked: Option<Instant>, every: Duration| {
            asked.is_none_or(|at| at.elapsed() >= every)
        };
        let every = if self.share.owned.is_empty() {
            OWNED_IDLE_EVERY
        } else {
            OWNED_EVERY
        };
        if !sharing_enabled() {
            self.share_turned_off();
            clear_servers();
        } else {
            CLEARING.with(|clearing| clearing.borrow_mut().turned_on());
            self.ready_share_panel();
            if due(self.share.owned_asked, every) {
                self.ask_after_shares();
            }
            if self.workspace_sidebar_shows_shares() && self.incoming_due() {
                self.ask_what_is_shared_with_us();
            }
        }
        if !self.share.watching.is_empty() {
            let mux = Mux::get();
            self.share
                .watching
                .retain(|pane_id, _| mux.get_pane(*pane_id).is_some());
            // A watch that ended out of sight has nothing left to show, and
            // its row went with the share: the key its viewer waits for
            // closes it.
            let ended: Vec<PaneId> = self
                .share
                .watching
                .iter()
                .filter(|(_, watch)| watch.state == "ended")
                .map(|(pane_id, _)| *pane_id)
                .collect();
            for pane_id in ended {
                let out_of_sight = mux.resolve_pane_id(pane_id).is_some_and(|(_, window_id, _)| {
                    !crate::frontend::front_end().has_mux_window(window_id)
                });
                if out_of_sight {
                    self.press_share_pill(pane_id);
                }
            }
        }
    }

    /// Sharing is off: the shares the window knew of stop (see
    /// `Clearing`), and what it knew of shares is forgotten, so it is
    /// asked afresh when sharing is turned on again.
    fn share_turned_off(&mut self) {
        self.close_share_panel();
        let had_incoming =
            !self.share.incoming.is_empty() || !self.share.other_version.is_empty();
        self.share.incoming.clear();
        self.share.other_version.clear();
        self.share.incoming_domain = None;
        self.share.incoming_asking = None;
        self.share.incoming_asked = None;
        self.share.owned_asked = None;
        TOLD.with(|told| told.borrow_mut().clear());
        self.share.candidates.clear();
        self.share.candidates_readied.clear();
        let known: HashSet<DomainId> = std::mem::take(&mut self.share.owned)
            .into_iter()
            .filter(|(_, shares)| !shares.is_empty())
            .map(|(domain_id, _)| domain_id)
            .collect();
        CLEARING.with(|clearing| clearing.borrow_mut().turned_off(&known));
        if had_incoming || !known.is_empty() {
            self.invalidate_window();
        }
    }

    /// Whether the sidebar that lists the panes shared with us is on screen.
    fn workspace_sidebar_shows_shares(&self) -> bool {
        self.workspace_sidebar_presented_width() > 0
    }

    /// The panes shared with us, for the sidebar: none while sharing is
    /// off.
    pub(crate) fn shared_with_us(&self) -> &[IncomingRow] {
        if !sharing_enabled() {
            return &[];
        }
        &self.share.incoming
    }

    /// For the sidebar, under the panes shared with us: a line for each
    /// account whose ThinkTerm is another version, which may share one
    /// that cannot be watched. None while sharing is off.
    pub(crate) fn shared_with_us_other_version(&self) -> Vec<String> {
        if !sharing_enabled() {
            return vec![];
        }
        self.share
            .other_version
            .iter()
            .map(|owner| tr_with("sidebar-shared-other-version", &[("owner", owner.clone())]))
            .collect()
    }

    /// The sidebar's rows under "Shared with me": an account of another
    /// version takes two, whose and what to do.
    pub(crate) fn shared_with_us_rows(&self) -> usize {
        if !sharing_enabled() {
            return 0;
        }
        self.share.incoming.len() + self.share.other_version.len() * 2
    }

    /// Watch a pane shared with us: in a workspace of its own, on the
    /// server that listed it, which the window goes to as it goes to a
    /// thread. The viewer there shows where the watch stands in the pane's
    /// bar. Watched already, the window goes back to it.
    pub(crate) fn watch_shared_pane(&mut self, index: usize) {
        let Some(row) = self.share.incoming.get(index).cloned() else {
            return;
        };
        if self.show_share_view(&row.workspace) || self.share.opening.contains(&row.workspace) {
            return;
        }
        let Some(window) = self.window.clone() else {
            return;
        };
        let program = self
            .share
            .watch_program
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|| "thinkterm".to_string());
        let mut command = portable_pty::CommandBuilder::from_argv(vec![
            program.into(),
            "cli".into(),
            "share".into(),
            "watch".into(),
            format!("{}/{}", row.owner, row.share_id).into(),
        ]);
        command.env("THINKTERM_SHARE_UI", "pane-bar");
        command.env(
            "THINKTERM_SHARE_LOADING",
            tr_with("share-view-loading", &[("owner", row.owner.clone())]),
        );
        let domain = match self.share.incoming_domain {
            Some(domain_id) => SpawnTabDomain::DomainId(domain_id),
            None => SpawnTabDomain::CurrentPaneDomain,
        };
        let current_pane = self.get_active_pane_or_overlay().map(|pane| pane.pane_id());
        let size = self.terminal_size;
        let term_config: Arc<dyn TerminalConfiguration> =
            Arc::new(TermConfig::with_config(self.config.clone()));
        let workspace = row.workspace;
        self.share.opening.insert(workspace.clone());
        self.invalidate_window();
        promise::spawn::spawn(async move {
            // A window of its own in the watch's workspace, which is not
            // the active one: nothing opens a second window for it, and
            // this one goes to it once it is there.
            let result = Mux::get()
                .spawn_tab_or_window(
                    None,
                    domain,
                    Some(command),
                    None,
                    size,
                    current_pane,
                    workspace.clone(),
                    None,
                )
                .await;
            if let Ok((_, pane, _)) = &result {
                pane.set_config(term_config);
            }
            window.notify(TermWindowNotif::Apply(Box::new(move |tw| {
                tw.share.opening.remove(&workspace);
                match result {
                    Ok(_) => {
                        tw.show_share_view(&workspace);
                    }
                    Err(err) => {
                        log::error!("watching a shared pane: {err:#}");
                        tw.invalidate_window();
                    }
                }
            })));
        })
        .detach();
    }

    /// Go to the workspace a share is watched in, when it is open; whether
    /// it was.
    fn show_share_view(&mut self, workspace: &str) -> bool {
        let Some(target) = crate::workspace_threads::window_to_show_in_workspace(workspace) else {
            return false;
        };
        if target != self.mux_window_id {
            // Leaving a thread as a switch to another does.
            self.snapshot_active_workspace_thread_layout();
            self.set_content_view_active(false);
            if !self.adopt_workspace_in_this_window(workspace) {
                return false;
            }
        }
        self.share.view_shown = Some(target);
        self.invalidate_window();
        true
    }

    /// Whether the watch in `workspace` is being opened, for the sidebar to
    /// show its row picked from the click on.
    pub(crate) fn share_view_opening(&self, workspace: &str) -> bool {
        self.share.opening.contains(workspace)
    }

    pub(crate) fn any_share_view_opening(&self) -> bool {
        !self.share.opening.is_empty()
    }

    /// Whether the mux window this window lost was a shared pane it was
    /// watching: then it goes back to its thread rather than on to another.
    pub(crate) fn share_view_closed(&mut self) -> bool {
        self.share.view_shown.take() == Some(self.mux_window_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_come_first_and_the_owner_last() {
        let list = candidates_from(ShareCandidatesResponse {
            me: "fox".into(),
            accounts: vec!["owl".into(), "fox".into(), "bee".into(), "owl".into()],
            groups: vec!["team3".into(), "team1".into()],
        });
        let names: Vec<(&str, bool, bool)> = list
            .iter()
            .map(|c| (c.name.as_str(), c.group, c.me))
            .collect();
        assert_eq!(
            names,
            vec![
                ("team1", true, false),
                ("team3", true, false),
                ("bee", false, false),
                ("owl", false, false),
                ("fox", false, true),
            ]
        );
    }

    #[test]
    fn sharing_off_stops_the_shares_known_until_gone() {
        let mut clearing = Clearing::default();
        // Windows knew of shares on 7 and 8.
        clearing.turned_off(&[7, 8].into());
        // Both asked at once, and not again while those asks are out.
        let mut asks = clearing.due(|_| true);
        asks.sort();
        assert_eq!(asks, vec![7, 8]);
        clearing.asked = None;
        assert!(clearing.due(|_| true).is_empty());

        // 7's are stopped, and that is told.
        assert_eq!(clearing.answered(7, Ok(1)), Some(Tell::Stopped));
        // 8 cannot be reached: told of, as its shares run on.
        assert_eq!(
            clearing.answered(8, Err(anyhow::anyhow!("not connected"))),
            Some(Tell::Unreachable)
        );
        // Sharing staying off says so each tick, with nothing new: the
        // asking keeps its pace.
        clearing.turned_off(&HashSet::new());
        assert!(clearing.due(|_| true).is_empty());
        clearing.asked = Some(Instant::now() - STOP_AGAIN_EVERY);
        assert_eq!(clearing.due(|_| true), vec![8]);
        // Each told once.
        assert_eq!(
            clearing.answered(8, Err(anyhow::anyhow!("not connected"))),
            None
        );
        clearing.asked = None;
        assert_eq!(clearing.due(|_| true), vec![8]);
        assert_eq!(clearing.answered(8, Ok(2)), None);
        assert!(clearing.left.is_empty());
    }

    #[test]
    fn a_desktop_that_never_shared_stops_nobodys_shares() {
        // Sharing off from the start: no window asked after shares, so
        // none are known, and another desktop's on the same servers run on.
        let mut clearing = Clearing::default();
        clearing.turned_off(&HashSet::new());
        assert!(clearing.due(|_| true).is_empty());
        assert!(clearing.left.is_empty());
    }

    #[test]
    fn turning_sharing_on_lets_the_clearing_go() {
        let mut clearing = Clearing::default();
        clearing.turned_off(&[7, 8].into());
        // With its server.
        assert_eq!(clearing.due(|domain_id| domain_id == 8), vec![8]);
        assert!(!clearing.left.contains(&7));
        // And all of it when sharing is on again; an ask still out comes
        // back to nothing.
        clearing.turned_on();
        assert!(clearing.left.is_empty());
        assert_eq!(clearing.answered(8, Ok(1)), None);
        assert!(clearing.asking.is_empty());
    }

    #[test]
    fn the_picked_are_shared_with() {
        let mut panel = SharePanel::new(1, 0, 7);
        panel.candidates = Candidates::Ready(candidates_from(ShareCandidatesResponse {
            me: "fox".into(),
            accounts: vec!["owl".into()],
            groups: vec!["team3".into()],
        }));
        panel.picked = vec![true, false, false];
        assert!(panel.can_start());
        assert_eq!(panel.audience(), vec![ShareAudience::Group("team3".into())]);
        panel.picked = vec![false, false, false];
        assert!(!panel.can_start());
        panel.picked = vec![false, true, true];
        panel.busy = true;
        assert!(!panel.can_start());
        assert_eq!(
            panel.audience(),
            vec![
                ShareAudience::User("owl".into()),
                ShareAudience::User("fox".into()),
            ]
        );
    }
}
