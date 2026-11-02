//! In-window remote hosts manager: a full-content-area "page" (left host list
//! + right edit form) that replaces the terminal area when toggled from the
//! sidebar globe button. Named "Remote Hosts" in the UI because it also
//! covers Mosh and ThinkTerm Connect; the code stays `ssh_hosts` since every
//! mode still dials out over SSH. Draws with the shared `ui::draw`/`ui::widgets`
//! infrastructure. The view owns its state + hit-testing and exposes a tiny
//! [`SshViewOutcome`] so `TermWindow` integration stays minimal.

use crate::quad::{HeapQuadAllocator, QuadClipRect, TripleLayerQuadAllocator};
use crate::ssh_hosts::{
    self, HostIcon, ShellIntegrationFailure, SshHostEntry, SshHostSource, SshHostSpec, SshNetwork,
};
use crate::tailnet::{
    Device, Machine as TailnetMachine, State as TailnetState, View as TailnetView,
};
use crate::termwindow::content_view::{
    ContentView, ContentViewResponse, ContentViewTypography, RemoteHostCommand,
};
use crate::termwindow::ui::icons::{distro_to_icon, BrandIcon, SvgIcon};
use crate::termwindow::ui::right_sidebar::wrap_snippet_text_for_width;
use crate::termwindow::TermWindow;
use crate::ui::anim::Easing;
use crate::ui::{
    apply_wheel, card_grid, card_is_warm, card_rect, char_index_for_x, contains, draw_button,
    draw_button_on_layer, draw_icon_button, draw_scrollbar, draw_text_input,
    draw_text_input_on_layer, draw_toggle, rect, row_visible, shared_grid_columns, ButtonSpec,
    ButtonVariant, CardGrid, ControlState, DrawContext, EditModifiers, InputCaret,
    InteractionState, RowAlign, ScrollState, TextInputSpec, TextInputState, UiContext, UiPalette,
    UiTokens, WidgetKind,
};
use crate::utilsprites::RenderMetrics;
use crate::workspace_threads;
use fluent_bundle::FluentArgs;
use std::rc::Rc;
use std::time::{Duration, Instant};
use wezterm_font::LoadedFont;
use wezterm_term::{KeyCode, KeyModifiers};
use window::color::LinearRgba;
use window::{
    MouseButtons, MouseCursor, MouseEvent, MouseEventKind as WMEK, MousePress, RectF, Window,
};

pub(crate) const SSH_HOSTS_CONTENT_VIEW_KEY: &str = "ssh-hosts";

const FIELD_NAME: usize = 0;
const FIELD_HOST: usize = 1;
const FIELD_PORT: usize = 2;
const FIELD_OTHER_ADDRESSES: usize = 3;
const FIELD_USER: usize = 4;
const FIELD_PASSWORD: usize = 5;
const FIELD_IDENTITY: usize = 6;
const FIELD_WORKSPACE: usize = 7;
const FIELD_MOSH_SERVER: usize = 8;
const BASE_FIELD_COUNT: usize = 8;
const FIELD_COUNT: usize = 9;

/// Where focus lands when the Advanced section is collapsed. Rows inside it
/// stop being drawn, so a focus still pointing at one would silently edit an
/// invisible field; anything else keeps its place.
fn focus_after_collapsing_advanced(focus: Focus) -> Focus {
    match focus {
        Focus::OptionKey(_) | Focus::OptionValue(_) => Focus::Field(FIELD_HOST),
        other => other,
    }
}

/// Tab order: the regular fields, then each visible `ssh_options` row as a
/// key/value pair.
fn tab_stops_for(form: &HostForm) -> Vec<Focus> {
    let mut stops: Vec<Focus> = (0..SshHostsView::visible_field_count(form))
        .map(Focus::Field)
        .collect();
    if form.advanced_open {
        for index in 0..form.options.len() {
            stops.push(Focus::OptionKey(index));
            stops.push(Focus::OptionValue(index));
        }
    }
    stops
}

/// Collapse the edited rows into the map the ssh layer consumes. Rows whose
/// key is blank are still being typed and are dropped rather than written as a
/// nameless option.
fn ssh_options_from_rows(
    rows: &[(TextInputState, TextInputState)],
) -> std::collections::HashMap<String, String> {
    rows.iter()
        .filter_map(|(key, value)| {
            let key = key.text().trim();
            (!key.is_empty()).then(|| (key.to_string(), value.text().trim().to_string()))
        })
        .collect()
}

fn text_input_with(text: String) -> TextInputState {
    let mut input = TextInputState::new();
    input.set_text_end(text);
    input
}

fn ssh_error(id: &'static str, error: impl Into<String>) -> String {
    let mut args = FluentArgs::new();
    args.set("error", error.into());
    crate::i18n::tr_args(id, &args)
}

/// Saves `spec` as the saved host `id` (see `ssh_hosts::try_update_host`),
/// and what goes with it.
fn update_saved_host(id: &str, spec: SshHostSpec) -> anyhow::Result<Option<String>> {
    let before = ssh_hosts::host_spec(id);
    let Some(saved_id) = ssh_hosts::try_update_host(id, spec)? else {
        return Ok(None);
    };
    if let (Some(before), Some(after)) = (before, ssh_hosts::host_spec(&saved_id)) {
        // Renamed or moved to the other network, the host is reached under
        // another name from now on, and its Spaces go with it: left under
        // the old name, they would reconnect the way it was reached before,
        // or not at all.
        if let Some((from, to)) = ssh_hosts::connect_name_change(&before, &after) {
            ssh_hosts::move_spaces_after_rename(from, to);
        }
        crate::refresh_connect_domain_for_ssh_host(&after);
    }
    Ok(Some(saved_id))
}

const PAD: f32 = 24.0;
const TOOLBAR_H: f32 = 54.0;
const TOOLBAR_GAP: f32 = 20.0;
/// The inspector never grows with the window. A form wider than this is
/// harder to read, not easier, so every extra pixel goes to the grid --
/// which is the half that actually has more to show. Dragging its edge
/// sets another width, within these bounds.
const INSPECTOR_W: f32 = 640.0;
const INSPECTOR_MIN_W: f32 = 480.0;
const INSPECTOR_MAX_W: f32 = 1200.0;
const INSPECTOR_GAP: f32 = 20.0;
const CARD_MIN_W: f32 = 340.0;
/// How wide a card may grow before the grid adds another column. Generous
/// on purpose: cap it too low and a full row ends up narrower than the page,
/// which is how a grid ends up with a margin it never asked for.
const CARD_MAX_W: f32 = 560.0;
const CARD_GAP: f32 = 18.0;
const CARD_RADIUS: f32 = 36.0;
const CARD_PAD: f32 = 22.0;
/// The Connect card's margin at its sides.
const TIP_PAD_X: f32 = 40.0;
/// The Connect card's title to its body, and its body to its button.
const TIP_TITLE_GAP: f32 = 14.0;
const TIP_BUTTON_GAP: f32 = 28.0;
/// Its picture at full size, the narrowest it is drawn whole, and the
/// narrowest the words beside it may get.
const TIP_PICTURE_W: f32 = 800.0;
const TIP_PICTURE_MIN_W: f32 = 740.0;
/// The host alone, the half of the picture kept when the rest does not fit.
const TIP_HOST_W: f32 = 460.0;
const TIP_TEXT_MIN_W: f32 = 600.0;
/// Beside the host alone the words may narrow further, to keep it there.
const TIP_TEXT_NARROW_W: f32 = 440.0;
/// Past five across the cards are too narrow to hold a machine's name and
/// its endpoint without truncating one of them, which is the only reason to
/// have a card at all.
const MAX_CARD_COLUMNS: usize = 5;
const GROUP_HEADER_H: f32 = 38.0;
/// Breathing room between a group's caption and its first row of cards.
const GROUP_TITLE_GAP: f32 = 10.0;
const GROUP_GAP: f32 = 22.0;
const INPUT_H: f32 = 54.0;
const BTN_H: f32 = 54.0;
const LIST_FADE_HEIGHT: f32 = 32.0;
/// The search field stays this wide however wide the window gets: a text box
/// spanning a 5K display is a worse target, not a better one.
const SEARCH_W: f32 = 360.0;
/// Gap between the stacked cards of the inspector.
const INSPECTOR_CARD_GAP: f32 = 16.0;
/// Gap between a field label and its input, and between two fields.
const LABEL_GAP: f32 = 7.0;
const FIELD_GAP: f32 = 18.0;
/// A second click in the same spot within this long connects instead of
/// re-selecting. Views never receive the window's click counting, so the
/// page has to notice a double click itself.
const DOUBLE_CLICK: std::time::Duration = std::time::Duration::from_millis(450);
/// How far the pointer may drift between the two clicks of a double click.
const DOUBLE_CLICK_SLOP: f32 = 6.0;

/// The Tailscale panel's fields, in tab order.
const TAILNET_AUTH_KEY: usize = 0;
const TAILNET_CONTROL_URL: usize = 1;
const TAILNET_HOSTNAME: usize = 2;
const TAILNET_FIELD_COUNT: usize = 3;
/// A machine of the tailnet that is online, and the tailnet while it is up:
/// the system's green.
const ONLINE_GREEN: (u8, u8, u8) = (0x30, 0xD1, 0x58);
/// The tailnet on its way up, or waiting for someone to log in.
const WAITING_ORANGE: (u8, u8, u8) = (0xFF, 0x9F, 0x0A);
/// How long the login link's button says it was copied.
const COPIED_FOR: Duration = Duration::from_millis(1400);
/// The gear at the end of the Tailscale group's caption.
const TAILNET_GEAR: f32 = 36.0;

/// Clickable targets inside the view. `usize` payloads index into the current
/// filtered host list, so the action type stays `Copy` (needed by `UiContext`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SshViewAction {
    Search,
    /// A host card: first click selects it, a second one connects.
    Card(usize),
    New,
    /// "Add a Server" on the card introducing Connect: `New`, lit on its own.
    TipNew,
    /// Connect to the host the inspector is showing.
    Connect,
    FocusField(usize),
    /// The icon button: a menu of the kinds of machine to show the host as.
    PickIcon,
    SetNetwork(SshNetwork),
    ToggleDetect,
    /// One of the three ways to reach the host.
    SetMode(HostMode),
    ToggleShellIntegration,
    /// The Files panel connects to the host by itself.
    ToggleFilesAutoConnect,
    RevealPassword,
    Save,
    SaveAndConnect,
    Cancel,
    ToggleSystemHosts,
    ToggleAdvanced,
    FocusOptionKey(usize),
    FocusOptionValue(usize),
    RemoveOption(usize),
    AddOption,
    /// "Switch to Connect" on a plain host's card: its form, on Connect.
    SwitchToConnect(usize),
    /// "Add with Connect" on a host from ~/.ssh/config: a new host made
    /// from it, on Connect.
    AddWithConnect(usize),
    /// The Tailscale group's caption: folds the group away, or back.
    ToggleTailnetGroup,
    /// The gear by that caption: the Tailscale panel, opened or closed.
    TailnetSettings,
    /// A machine of the tailnet, by its place in the group as painted.
    TailnetMachine(usize),
    /// The button on the card the group shows instead of machines; what it
    /// does is what it said when painted (see `TailnetGroup::notice`).
    TailnetNotice,
    /// The switch in the Tailscale panel's header.
    ToggleTailnet,
    OpenTailnetLogin,
    CopyTailnetLogin,
    FocusTailnetField(usize),
    TailnetLogout,
}

/// How a host is reached, as the form offers it: one choice of three where
/// the spec keeps two flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum HostMode {
    /// ThinkTerm Connect: the host's own mux server keeps the sessions.
    Connect,
    Ssh,
    Mosh,
}

impl HostMode {
    fn of(use_mosh: bool, multiplexing: bool) -> Self {
        if use_mosh {
            Self::Mosh
        } else if multiplexing {
            Self::Connect
        } else {
            Self::Ssh
        }
    }
}

/// A choice in a saved host's form, saved as it is clicked (see
/// `SshHostsView::save_choice`). What is typed waits for Save.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostChoice {
    Icon(Option<HostIcon>),
    Network(SshNetwork),
    Mode(HostMode),
    DetectOs(bool),
    ShellIntegration(bool),
    FilesAutoConnect(bool),
}

impl HostChoice {
    /// Makes the choice on `spec`, as Save makes it from the form.
    fn apply_to(self, spec: &mut SshHostSpec) {
        match self {
            Self::Icon(icon) => spec.icon = icon,
            Self::Network(network) => spec.network = network,
            Self::Mode(mode) => {
                // Mosh and ThinkTerm Connect are alternative transports;
                // only one can drive the connection.
                spec.use_mosh = mode == HostMode::Mosh;
                spec.multiplexing = mode == HostMode::Connect;
                if spec.mosh_server_command.trim().is_empty() {
                    spec.mosh_server_command = ssh_hosts::DEFAULT_MOSH_SERVER_COMMAND.to_string();
                }
            }
            Self::DetectOs(on) => spec.detect_os = on,
            Self::ShellIntegration(on) => spec.shell_integration = on,
            Self::FilesAutoConnect(on) => spec.files_auto_connect = on,
        }
    }

    /// Puts back in `form` what `saved` has for the choice: what one that
    /// could not be saved shows instead.
    fn show_saved(self, saved: &SshHostSpec, form: &mut HostForm) {
        match self {
            Self::Icon(_) => form.icon = saved.icon,
            Self::Network(_) => form.network = saved.network,
            Self::Mode(_) => {
                form.use_mosh = saved.use_mosh;
                form.multiplexing = saved.multiplexing;
            }
            Self::DetectOs(_) => form.detect_os = saved.detect_os,
            Self::ShellIntegration(_) => form.shell_integration = saved.shell_integration,
            Self::FilesAutoConnect(_) => form.files_auto_connect = saved.files_auto_connect,
        }
    }
}

/// The mux server of the README's architecture diagram, in the page's
/// theme, as (fill, edge, ink): how a host that keeps its sessions -- a
/// ThinkTerm Connect host -- is drawn.
fn mux_colors(palette: UiPalette) -> (LinearRgba, LinearRgba, LinearRgba) {
    if palette.is_dark() {
        (
            LinearRgba::with_srgba(23, 52, 98, 255),
            LinearRgba::with_srgba(43, 85, 146, 255),
            LinearRgba::with_srgba(77, 163, 255, 255),
        )
    } else {
        (
            palette.accent.mul_alpha(0.08),
            palette.accent.mul_alpha(0.45),
            palette.accent,
        )
    }
}

/// The green of a check and of a running session.
fn good_green(palette: UiPalette) -> LinearRgba {
    if palette.is_dark() {
        LinearRgba::with_srgba(50, 215, 75, 255)
    } else {
        LinearRgba::with_srgba(31, 157, 58, 255)
    }
}

/// The card introducing ThinkTerm Connect laid out at one width: see
/// `SshHostsView::connect_tip_layout`.
struct ConnectTipLayout {
    picture: TipPicture,
    text_w: f32,
    title: Vec<String>,
    body: Vec<String>,
    words_h: f32,
    picture_h: f32,
    height: f32,
}

/// Where the Connect card's picture goes, how wide it is drawn, and whether
/// it has room for this computer beside the host.
#[derive(Clone, Copy, PartialEq)]
enum TipPicture {
    Beside { width: f32, computer: bool },
    /// Under the words, on a page too narrow for both side by side.
    Below { width: f32, computer: bool },
    /// Not even room under them.
    None,
}

/// One way of reaching a host as its card in the form wraps it: each fact's
/// lines with whether it counts for the host, then the note's lines.
struct ModeWords {
    facts: Vec<(Vec<String>, bool)>,
    note: Vec<String>,
}

/// What the button on the Tailscale group's card does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoticeButton {
    TurnOn,
    LogIn,
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Focus {
    Search,
    Field(usize),
    /// `ssh_options` row `n`, key column.
    OptionKey(usize),
    /// `ssh_options` row `n`, value column.
    OptionValue(usize),
    /// One of the Tailscale panel's fields.
    Tailnet(usize),
}

/// The Tailscale group: its panel's fields and what it painted last, for
/// the clicks that come after.
#[derive(Default)]
struct TailnetGroup {
    /// The panel shows in the inspector, in place of a host's form.
    panel_open: bool,
    collapsed: bool,
    /// The machines as painted, those the search finds: cards name one by
    /// its place here.
    machines: Vec<TailnetMachine>,
    /// The saved host whose machine was clicked last. Its card is ringed
    /// while the inspector shows that host, the way a host's card is.
    selected: Option<String>,
    /// The approval link as painted, for the buttons that open and copy it.
    login_url: Option<String>,
    /// What the card's button said when painted.
    notice: Option<NoticeButton>,
    /// The auth key, the control server and this machine's name in the
    /// tailnet. The key's never shows a saved key, only one being typed,
    /// and empties once that one is saved.
    fields: [TextInputState; TAILNET_FIELD_COUNT],
    /// Typed into since the field last went out.
    dirty: [bool; TAILNET_FIELD_COUNT],
    /// The change each field's text went out as: the text stays while it
    /// is saved, or could not be (see crate::tailnet::saved).
    changes: [Option<u64>; TAILNET_FIELD_COUNT],
    /// The login link's button says "Copied" until then.
    copied_until: Option<Instant>,
}

/// What the Tailscale group holds under its caption.
enum TailnetBody {
    /// Nothing yet: the page has not read how the tailnet is.
    Pending,
    /// Its machines, as `TailnetGroup::machines` holds them.
    Machines,
    /// One card instead: the tailnet off, on its way, or waiting.
    Notice(TailnetNotice),
}

struct TailnetNotice {
    icon: SvgIcon,
    title: String,
    subtitle: String,
    button: Option<(String, NoticeButton)>,
    /// The invitation to turn it on, drawn in the accent.
    invite: bool,
}

#[derive(Default, Clone)]
struct HostForm {
    editing: Option<String>,
    original: Option<SshHostSpec>,
    fields: [TextInputState; FIELD_COUNT],
    /// Free-form `ssh_config` overrides, edited as key/value rows. Kept as a
    /// Vec (not a map) so a half-typed key does not collide or reorder while
    /// the user is still typing.
    options: Vec<(TextInputState, TextInputState)>,
    advanced_open: bool,
    icon: Option<HostIcon>,
    network: SshNetwork,
    detect_os: bool,
    use_mosh: bool,
    multiplexing: bool,
    shell_integration: bool,
    files_auto_connect: bool,
    error: Option<String>,
}

pub(crate) struct SshHostsView {
    search: TextInputState,
    selected: usize,
    focus: Focus,
    form: Option<HostForm>,
    filtered: Vec<SshHostEntry>,
    system_host_count: usize,
    system_hosts_collapsed: bool,
    /// ui scale captured at paint time so mouse handlers (no DrawContext)
    /// can convert pointer pixels back into design pixels.
    last_ui_scale: f32,
    /// The inspector's width in design pixels: the saved one, or the one a
    /// drag of its edge is making.
    inspector_width: f32,
    /// The widest the inspector could be on the page as last painted, in
    /// design pixels.
    inspector_max_width: f32,
    /// Where the inspector's edge can be grabbed, as last painted; empty
    /// while it is closed.
    inspector_edge: RectF,
    inspector_edge_hovered: bool,
    /// A drag of that edge: the pointer's x and the width it started from.
    inspector_drag: Option<(f32, f32)>,
    /// Scrollable region holding the host grid.
    list_area: RectF,
    /// Rect of each painted card, in the same order as `filtered`, so a
    /// right-click can name the host under the pointer.
    card_rects: Vec<(usize, RectF)>,
    /// Reveal the password field's characters. Per-session and per-form: it
    /// resets whenever another host is opened.
    password_visible: bool,
    /// The host, pointer position and time of the last card click, for the
    /// page's own double-click detection.
    last_click: Option<(String, f32, f32, std::time::Instant)>,
    /// Whether `selected` reflects a real choice. Without it the page would
    /// open with the first card already highlighted.
    selection_active: bool,
    /// Absolute x of a click that should become a caret position. Mouse
    /// handlers have no `DrawContext`, so glyph measurement is deferred to the
    /// next paint, which knows the font and the field's text origin.
    pending_caret_click: Option<(Focus, f32)>,
    /// Independent scroll for the right-hand form column; the left host list
    /// has its own `scroll`.
    form_scroll: ScrollState,
    /// Height the form needed on the previous frame. Immediate-mode layout
    /// only learns it after drawing, so extents lag by one frame and settle.
    form_content_h: f32,
    /// Bounds of the form column, remembered for wheel hit-testing.
    form_area: RectF,
    /// The icon button as last painted: where its menu opens.
    icon_button: RectF,
    widgets: UiContext<SshViewAction>,
    interaction: InteractionState<SshViewAction>,
    scroll: ScrollState,
    /// Either scroll state was still gliding after the last paint.
    scroll_animating: bool,
    /// The built-in tailnet is on, as last painted: a host can be put on it.
    tailnet_on: bool,
    /// `~/.ssh/config` as last read, with the stamp it had then. The list is
    /// rebuilt on every paint so hosts saved elsewhere show up, and the page
    /// paints for each scroll step and caret blink; the file is read again
    /// only when its stamp moves.
    system_hosts: Option<(ssh_hosts::SshConfigStamp, Vec<SshHostEntry>)>,
    /// Every host saved here, whatever the search: what tells a machine of
    /// the tailnet already saved as a host.
    saved_hosts: Vec<SshHostEntry>,
    /// The window the page is in, repainted when a read of the tailnet
    /// lands. None in tests.
    window: Option<Window>,
    tailnet: TailnetGroup,
    _tailnet_page: crate::tailnet::Page,
}

impl SshHostsView {
    pub(crate) fn new() -> Self {
        let mut view = Self {
            search: TextInputState::new(),
            selected: 0,
            focus: Focus::Search,
            form: None,
            filtered: Vec::new(),
            system_host_count: 0,
            system_hosts_collapsed: true,
            last_ui_scale: 1.0,
            inspector_width: crate::native_settings::remote_hosts_inspector_width()
                .map_or(INSPECTOR_W, |width| width as f32)
                .clamp(INSPECTOR_MIN_W, INSPECTOR_MAX_W),
            inspector_max_width: INSPECTOR_MAX_W,
            inspector_edge: rect(0.0, 0.0, 0.0, 0.0),
            inspector_edge_hovered: false,
            inspector_drag: None,
            list_area: rect(0.0, 0.0, 0.0, 0.0),
            card_rects: Vec::new(),
            password_visible: false,
            last_click: None,
            selection_active: false,
            pending_caret_click: None,
            form_scroll: ScrollState::new(),
            form_content_h: 0.0,
            form_area: rect(0.0, 0.0, 0.0, 0.0),
            icon_button: rect(0.0, 0.0, 0.0, 0.0),
            widgets: UiContext::default(),
            interaction: InteractionState::default(),
            scroll: ScrollState::new(),
            scroll_animating: false,
            tailnet_on: false,
            system_hosts: None,
            saved_hosts: Vec::new(),
            window: None,
            tailnet: TailnetGroup::default(),
            _tailnet_page: crate::tailnet::Page::open(),
        };
        view.refresh();
        view
    }

    /// The page in `window`: what the tailnet's reads repaint.
    pub(crate) fn in_window(mut self, window: Option<Window>) -> Self {
        self.window = window;
        self
    }

    /// A page that opens straight on a blank host form, for the "Add Remote
    /// Host" entry point.
    pub(crate) fn new_host() -> Self {
        let mut view = Self::new();
        view.open_new_form();
        view
    }

    fn refresh(&mut self) {
        let needle = self.search.text().trim().to_ascii_lowercase();
        let show_system_hosts = !self.system_hosts_collapsed || !needle.is_empty();
        self.system_host_count = 0;
        let stamp = ssh_hosts::system_ssh_config_stamp();
        let cached = match &self.system_hosts {
            Some((read_at, hosts)) if stamp == Some(*read_at) => Some(hosts.clone()),
            _ => None,
        };
        let system_hosts = cached.unwrap_or_else(|| {
            let hosts = ssh_hosts::list_system_hosts();
            self.system_hosts = stamp.map(|stamp| (stamp, hosts.clone()));
            hosts
        });
        let all = ssh_hosts::list_all_hosts_with(system_hosts);
        self.saved_hosts = all
            .iter()
            .filter(|entry| entry.source == SshHostSource::ThinkTerm)
            .cloned()
            .collect();
        self.filtered = all
            .into_iter()
            .filter(|entry| {
                if needle.is_empty() {
                    return true;
                }
                format!(
                    "{} {} {}",
                    entry.spec.label,
                    entry.spec.host,
                    entry.spec.username.as_deref().unwrap_or("")
                )
                .to_ascii_lowercase()
                .contains(&needle)
            })
            .filter(|entry| {
                if entry.source == SshHostSource::System {
                    self.system_host_count += 1;
                    show_system_hosts
                } else {
                    true
                }
            })
            .collect();
        if self.filtered.is_empty() {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(self.filtered.len() - 1);
        }
    }

    // ---- form helpers -----------------------------------------------------

    fn visible_field_count(form: &HostForm) -> usize {
        if form.use_mosh {
            FIELD_COUNT
        } else {
            BASE_FIELD_COUNT
        }
    }

    fn open_new_form(&mut self) {
        self.close_tailnet_panel();
        self.tailnet.selected = None;
        self.password_visible = false;
        let mut form = HostForm::default();
        form.fields[FIELD_PORT].set_text_end("22".to_string());
        form.fields[FIELD_MOSH_SERVER]
            .set_text_end(ssh_hosts::DEFAULT_MOSH_SERVER_COMMAND.to_string());
        form.detect_os = true;
        // Files opened with the host, and its programs shown on the tabs:
        // on unless turned off.
        form.files_auto_connect = true;
        form.shell_integration = true;
        // ThinkTerm Connect unless the person picks otherwise: it is the way
        // the host keeps its sessions, and the one people miss when the
        // choice starts on plain ssh.
        form.multiplexing = true;
        self.form = Some(form);
        self.focus = Focus::Field(FIELD_HOST);
        self.reset_form_scroll();
    }

    /// A new host on Connect, filled in from `spec`: a host ~/.ssh/config
    /// knows, to be saved here.
    fn open_connect_form_from(&mut self, spec: &SshHostSpec) {
        self.open_new_form();
        if let Some(form) = self.form.as_mut() {
            form.fields[FIELD_NAME].set_text_end(spec.label.clone());
            form.fields[FIELD_HOST].set_text_end(spec.host.clone());
            // No port of its own leaves the field empty, so that the one
            // ~/.ssh/config gives the host (`Host *`, `Match`) still counts.
            form.fields[FIELD_PORT] =
                text_input_with(spec.port.map(|port| port.to_string()).unwrap_or_default());
            if let Some(user) = &spec.username {
                form.fields[FIELD_USER].set_text_end(user.clone());
            }
            if let Some(identity) = &spec.identity_file {
                form.fields[FIELD_IDENTITY].set_text_end(identity.clone());
            }
        }
    }

    /// Whether the card introducing ThinkTerm Connect heads the page: always,
    /// but while a search narrows the list.
    fn connect_tip_shown(&self) -> bool {
        self.search.text().trim().is_empty()
    }

    fn open_edit_form(&mut self, project_id: &str) {
        if ssh_hosts::is_system_host_id(project_id) {
            return;
        }
        let Some(spec) = ssh_hosts::host_spec(project_id) else {
            return;
        };
        self.open_form_for(project_id, spec);
    }

    /// The inspector's form for the saved host `project_id`, as `spec` has
    /// it.
    fn open_form_for(&mut self, project_id: &str, spec: SshHostSpec) {
        self.close_tailnet_panel();
        // Revealing a password is a decision about one host. Carrying it into
        // the next one would put a stored password on screen unasked.
        self.password_visible = false;
        let mut fields: [String; FIELD_COUNT] = Default::default();
        fields[FIELD_NAME] = spec.label.clone();
        fields[FIELD_HOST] = spec.host.clone();
        fields[FIELD_PORT] = spec.port.map(|p| p.to_string()).unwrap_or_default();
        fields[FIELD_OTHER_ADDRESSES] = spec.other_addresses.join(", ");
        fields[FIELD_USER] = spec.username.clone().unwrap_or_default();
        fields[FIELD_PASSWORD] = spec
            .password
            .as_deref()
            .map(crate::secret::reveal)
            .unwrap_or_default();
        fields[FIELD_IDENTITY] = spec.identity_file.clone().unwrap_or_default();
        fields[FIELD_WORKSPACE] = spec.default_workspace.clone().unwrap_or_default();
        fields[FIELD_MOSH_SERVER] = spec.mosh_server_command.clone();
        let fields = fields.map(|text| {
            let mut input = TextInputState::new();
            input.set_text_end(text);
            input
        });
        let icon = spec.icon;
        let network = spec.network;
        let detect_os = spec.detect_os;
        let use_mosh = spec.use_mosh;
        let multiplexing = spec.multiplexing;
        let shell_integration = spec.shell_integration;
        let files_auto_connect = spec.files_auto_connect;
        // Sorted so the rows keep a stable order between opens.
        let mut option_pairs: Vec<(String, String)> = spec
            .ssh_options
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        option_pairs.sort();
        let options = option_pairs
            .into_iter()
            .map(|(key, value)| (text_input_with(key), text_input_with(value)))
            .collect::<Vec<_>>();
        let advanced_open = !options.is_empty();
        self.form = Some(HostForm {
            editing: Some(project_id.to_string()),
            original: Some(spec),
            fields,
            options,
            advanced_open,
            icon,
            network,
            detect_os,
            use_mosh,
            multiplexing,
            shell_integration,
            files_auto_connect,
            error: None,
        });
        self.focus = Focus::Field(FIELD_HOST);
        self.reset_form_scroll();
    }

    fn close_form(&mut self) {
        self.form = None;
        self.focus = Focus::Search;
        // Revealing a password is a decision about one host, not a mode.
        self.password_visible = false;
        self.reset_form_scroll();
    }

    /// A newly installed form must start at the top. `form_content_h` is the
    /// previous frame's measurement, so it has to go too — otherwise the first
    /// frame sizes the scroll extents from the old form's height.
    fn reset_form_scroll(&mut self) {
        self.form_scroll.reset();
        self.form_content_h = 0.0;
    }

    /// Validate + create/update. On success returns the project id and closes
    /// the form; on error records the message and keeps the form open.
    fn persist_form(&mut self) -> Option<String> {
        let form = self.form.as_mut()?;
        if form.network == SshNetwork::Tailnet && form.use_mosh {
            form.error = Some(crate::i18n::tr("ssh-error-tailnet-mosh"));
            return None;
        }
        let host = form.fields[FIELD_HOST].text().trim().to_string();
        if host.is_empty() {
            form.error = Some(crate::i18n::tr("ssh-error-host-required"));
            return None;
        }
        let port = match form.fields[FIELD_PORT].text().trim() {
            "" => None,
            value => match value.parse::<u16>() {
                Ok(p) => Some(p),
                Err(_) => {
                    form.error = Some(crate::i18n::tr("ssh-error-port-number"));
                    return None;
                }
            },
        };
        let opt = |s: &str| {
            let t = s.trim();
            (!t.is_empty()).then(|| t.to_string())
        };
        let label = opt(&form.fields[FIELD_NAME].text()).unwrap_or_else(|| host.clone());

        let mut spec = form.original.clone().unwrap_or_else(|| SshHostSpec {
            label: label.clone(),
            host: host.clone(),
            port,
            other_addresses: Vec::new(),
            network: Default::default(),
            username: None,
            identity_file: None,
            password: None,
            ssh_options: Default::default(),
            multiplexing: false,
            default_workspace: None,
            detect_os: true,
            detected_distro: None,
            use_mosh: false,
            mosh_server_command: ssh_hosts::DEFAULT_MOSH_SERVER_COMMAND.to_string(),
            shell_integration: false,
            shell_integration_applied: false,
            shell_integration_unsupported: None,
            shell_integration_failure: None,
            files_auto_connect: false,
            icon: None,
        });
        spec.label = label;
        spec.host = host;
        spec.port = port;
        spec.icon = form.icon;
        spec.network = form.network;
        spec.other_addresses =
            ssh_hosts::parse_other_addresses(&form.fields[FIELD_OTHER_ADDRESSES].text());
        spec.username = opt(&form.fields[FIELD_USER].text());
        // Encrypt the password before it is persisted to ssh_hosts.json. Never
        // fall back to plaintext: if encryption fails, keep the form open.
        spec.password = match opt(&form.fields[FIELD_PASSWORD].text()) {
            Some(password) => match crate::secret::encrypt(&password) {
                Ok(encrypted) => Some(encrypted),
                Err(err) => {
                    log::error!("failed to encrypt SSH password: {err:#}");
                    form.error = Some(ssh_error("ssh-error-encrypt", format!("{err:#}")));
                    return None;
                }
            },
            None => None,
        };
        spec.identity_file = opt(&form.fields[FIELD_IDENTITY].text());
        spec.default_workspace = opt(&form.fields[FIELD_WORKSPACE].text());
        // Rows with an empty key are still being typed; drop them rather than
        // writing a nameless option the ssh layer would ignore.
        spec.ssh_options = ssh_options_from_rows(&form.options);
        spec.detect_os = form.detect_os;
        spec.use_mosh = form.use_mosh;
        spec.multiplexing = form.multiplexing;
        spec.shell_integration = form.shell_integration;
        spec.files_auto_connect = form.files_auto_connect;
        let mosh_server_command = form.fields[FIELD_MOSH_SERVER].text().trim();
        if spec.use_mosh && mosh_server_command.is_empty() {
            form.error = Some(crate::i18n::tr("ssh-error-mosh-command"));
            return None;
        }
        spec.mosh_server_command = if mosh_server_command.is_empty() {
            ssh_hosts::DEFAULT_MOSH_SERVER_COMMAND.to_string()
        } else {
            mosh_server_command.to_string()
        };

        // One record per endpoint, checked before either path. A record is
        // keyed by its endpoint, so `editing` already equals this id when an
        // edit leaves the endpoint alone -- which is the one case that is not
        // a collision. Saving is what reports it; `ssh_hosts` refuses the
        // same thing again underneath, without the localized wording.
        let endpoint_id = ssh_hosts::host_id_for_host(&spec);
        if form.editing.as_deref() != Some(endpoint_id.as_str())
            && ssh_hosts::host_exists(&endpoint_id)
        {
            let mut args = FluentArgs::new();
            args.set("endpoint", ssh_hosts::endpoint(&spec));
            form.error = Some(crate::i18n::tr_args("ssh-error-duplicate", &args));
            return None;
        }

        let project_id = match &form.editing {
            // Not `id`: moving the host to another endpoint re-keys the
            // record, and the caller selects what comes back.
            Some(id) => match update_saved_host(id, spec) {
                Ok(Some(saved_id)) => saved_id,
                Ok(None) => {
                    form.error = Some(crate::i18n::tr("ssh-error-host-missing"));
                    return None;
                }
                Err(err) => {
                    form.error = Some(ssh_error("ssh-error-save", format!("{err:#}")));
                    return None;
                }
            },
            None => match ssh_hosts::try_create_host(spec) {
                Ok(id) => id,
                Err(err) => {
                    form.error = Some(ssh_error("ssh-error-save", format!("{err:#}")));
                    return None;
                }
            },
        };
        self.close_form();
        self.refresh();
        Some(project_id)
    }

    /// Saves `choice`, just made in the form, on the saved host the form
    /// edits: that one choice, onto the host as saved, so that what was
    /// typed into the form still waits for Save. A new host has nothing to
    /// make it on yet; Save saves it with the rest.
    fn save_choice(&mut self, choice: HostChoice) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let Some(id) = form.editing.clone() else {
            return;
        };
        let Some(saved) = ssh_hosts::host_spec(&id) else {
            form.error = Some(crate::i18n::tr("ssh-error-host-missing"));
            return;
        };
        let mut spec = saved.clone();
        choice.apply_to(&mut spec);
        // Made already: there is nothing to write.
        if spec == saved {
            return;
        }
        // One record per endpoint, as Save checks: on the other network,
        // the host may be saved already.
        let endpoint_id = ssh_hosts::host_id_for_host(&spec);
        if endpoint_id != id && ssh_hosts::host_exists(&endpoint_id) {
            let mut args = FluentArgs::new();
            args.set("endpoint", ssh_hosts::endpoint(&spec));
            form.error = Some(crate::i18n::tr_args("ssh-error-duplicate", &args));
            choice.show_saved(&saved, form);
            return;
        }
        match update_saved_host(&id, spec) {
            Ok(Some(saved_id)) => {
                let now = ssh_hosts::host_spec(&saved_id);
                let on_tailnet = now
                    .as_ref()
                    .is_some_and(|spec| spec.network == SshNetwork::Tailnet);
                if let Some(spec) = now {
                    form.original = Some(spec);
                }
                form.editing = Some(saved_id.clone());
                self.keep_ring_on(&id, &saved_id, on_tailnet);
            }
            Ok(None) => {
                form.error = Some(crate::i18n::tr("ssh-error-host-missing"));
                choice.show_saved(&saved, form);
            }
            Err(err) => {
                form.error = Some(ssh_error("ssh-error-save", format!("{err:#}")));
                choice.show_saved(&saved, form);
            }
        }
    }

    /// The tailnet's machine of the host `id` keeps the ring once the host
    /// is saved as `saved_id`: moved to the other network, the record is
    /// keyed anew. Moved off the tailnet, no machine there stands for it any
    /// more, so its card takes the ring -- where the list has it now, as the
    /// record is keyed anew in place.
    fn keep_ring_on(&mut self, id: &str, saved_id: &str, on_tailnet: bool) {
        if self.tailnet.selected.as_deref() != Some(id) {
            return;
        }
        if on_tailnet {
            self.tailnet.selected = Some(saved_id.to_string());
            return;
        }
        self.tailnet.selected = None;
        if let Some(index) = self.filtered.iter().position(|entry| entry.id == id) {
            self.selected = index;
            self.selection_active = true;
        }
    }

    // ---- input ------------------------------------------------------------

    /// The inspector's edge, grabbed and dragged to make it wider or
    /// narrower; None leaves the event to the page.
    fn drag_inspector_edge(
        &mut self,
        x: f32,
        y: f32,
        event: &MouseEvent,
    ) -> Option<ContentViewResponse> {
        let widest = INSPECTOR_MAX_W.min(self.inspector_max_width).max(INSPECTOR_MIN_W);
        if let Some((start_x, start_width)) = self.inspector_drag {
            let held = event.mouse_buttons.contains(MouseButtons::LEFT);
            if matches!(event.kind, WMEK::Move) && held {
                self.inspector_width = (start_width + (start_x - x) / self.last_ui_scale)
                    .clamp(INSPECTOR_MIN_W, widest);
                return Some(ContentViewResponse::Redraw);
            }
            // Let go -- here, or outside the page, which the next event
            // tells by the button no longer being held. A click that moved
            // nothing keeps the saved width, even one the window is too
            // narrow to show.
            self.inspector_drag = None;
            if (self.inspector_width - start_width).abs() >= 1.0 {
                let width = self.inspector_width.round() as usize;
                if let Err(err) = crate::native_settings::save_remote_hosts_inspector_width(width)
                {
                    log::warn!("failed to remember the Remote Hosts form's width: {err:#}");
                }
            }
            if !matches!(event.kind, WMEK::Press(_)) {
                self.inspector_edge_hovered = contains(self.inspector_edge, x, y);
                return Some(ContentViewResponse::Redraw);
            }
        }
        let on_edge = contains(self.inspector_edge, x, y);
        match event.kind {
            WMEK::Press(MousePress::Left) if on_edge => {
                self.interaction.pressed = None;
                self.inspector_width = self.inspector_width.min(widest);
                self.inspector_drag = Some((x, self.inspector_width));
                Some(ContentViewResponse::Redraw)
            }
            WMEK::Move if on_edge != self.inspector_edge_hovered => {
                self.inspector_edge_hovered = on_edge;
                let _ = self.on_mouse(x, y, event.kind.clone());
                Some(ContentViewResponse::Redraw)
            }
            _ => None,
        }
    }

    fn on_mouse_impl(&mut self, x: f32, y: f32, kind: WMEK) -> ContentViewResponse {
        let hit = self.widgets.hit_test(x, y).map(|t| t.action);
        match kind {
            WMEK::Move => {
                if self.interaction.hovered != hit {
                    self.interaction.hovered = hit;
                    ContentViewResponse::Redraw
                } else {
                    ContentViewResponse::Ignored
                }
            }
            WMEK::Press(MousePress::Left) => {
                self.interaction.pressed = hit;
                ContentViewResponse::Redraw
            }
            WMEK::Release(MousePress::Left) => {
                let pressed = self.interaction.pressed.take();
                // A second click in the same spot connects to the host the
                // first one selected. Keyed on the pointer rather than on the
                // card under it: selecting a host opens the inspector, which
                // takes its width off the grid and reflows it, so by the
                // second click this position can belong to another card
                // entirely -- or to no card at all.
                if let Some((host_id, click_x, click_y, at)) = self.last_click.take() {
                    if at.elapsed() < DOUBLE_CLICK
                        && (x - click_x).abs() <= self.last_ui_scale * DOUBLE_CLICK_SLOP
                        && (y - click_y).abs() <= self.last_ui_scale * DOUBLE_CLICK_SLOP
                    {
                        return open_thread_response(host_id);
                    }
                }
                if let (Some(a), Some(b)) = (hit, pressed) {
                    if a == b {
                        // A saved machine of the tailnet is that host's card
                        // again, double click and all.
                        let host = match a {
                            SshViewAction::Card(index) => self.filtered.get(index),
                            SshViewAction::TailnetMachine(index) => self
                                .tailnet
                                .machines
                                .get(index)
                                .and_then(|machine| self.saved_host_for(machine)),
                            _ => None,
                        };
                        if let Some(entry) = host {
                            self.last_click =
                                Some((entry.id.clone(), x, y, std::time::Instant::now()));
                        }
                        self.pending_caret_click = match a {
                            SshViewAction::Search => Some((Focus::Search, x)),
                            SshViewAction::FocusField(n) => Some((Focus::Field(n), x)),
                            SshViewAction::FocusOptionKey(n) => Some((Focus::OptionKey(n), x)),
                            SshViewAction::FocusOptionValue(n) => Some((Focus::OptionValue(n), x)),
                            SshViewAction::FocusTailnetField(n) => Some((Focus::Tailnet(n), x)),
                            _ => None,
                        };
                        return self.apply(a);
                    }
                }
                ContentViewResponse::Redraw
            }
            WMEK::Press(MousePress::Right) => {
                // A right press opens the menu. Clear any pending left-button
                // bookkeeping first: the menu runs its own event loop and the
                // matching release never comes back to us.
                self.interaction.pressed = None;
                self.host_context_menu(x, y)
            }
            _ => ContentViewResponse::Ignored,
        }
    }

    fn point_in_host_list(&self, x: f32, y: f32) -> bool {
        x >= self.list_area.origin.x
            && x <= self.list_area.origin.x + self.list_area.size.width
            && y >= self.list_area.origin.y
            && y <= self.list_area.origin.y + self.list_area.size.height
    }

    fn apply(&mut self, action: SshViewAction) -> ContentViewResponse {
        self.clear_focused_selection();
        match action {
            SshViewAction::Search => {
                self.focus = Focus::Search;
                ContentViewResponse::Redraw
            }
            SshViewAction::ToggleAdvanced => {
                if let Some(form) = self.form.as_mut() {
                    form.advanced_open = !form.advanced_open;
                    if !form.advanced_open {
                        // Leaving focus on a row that is no longer drawn would
                        // let the next keystroke edit an invisible field.
                        self.focus = focus_after_collapsing_advanced(self.focus);
                    }
                }
                ContentViewResponse::Redraw
            }
            SshViewAction::FocusOptionKey(n) => {
                self.focus = Focus::OptionKey(n);
                ContentViewResponse::Redraw
            }
            SshViewAction::FocusOptionValue(n) => {
                self.focus = Focus::OptionValue(n);
                ContentViewResponse::Redraw
            }
            SshViewAction::AddOption => {
                if let Some(form) = self.form.as_mut() {
                    form.advanced_open = true;
                    form.options
                        .push((TextInputState::new(), TextInputState::new()));
                    self.focus = Focus::OptionKey(form.options.len() - 1);
                }
                ContentViewResponse::Redraw
            }
            SshViewAction::RemoveOption(n) => {
                if let Some(form) = self.form.as_mut() {
                    if n < form.options.len() {
                        form.options.remove(n);
                    }
                }
                // The removed row's index would now point at its neighbour.
                if matches!(self.focus, Focus::OptionKey(_) | Focus::OptionValue(_)) {
                    self.focus = Focus::Field(FIELD_HOST);
                }
                ContentViewResponse::Redraw
            }
            SshViewAction::New | SshViewAction::TipNew => {
                self.open_new_form();
                ContentViewResponse::Redraw
            }
            SshViewAction::Card(index) => {
                // The double click that connects is resolved in the mouse
                // handler, which is the only place that knows where the
                // pointer was.
                self.select_host(index);
                ContentViewResponse::Redraw
            }
            SshViewAction::Connect => {
                let id = self.form.as_ref().and_then(|form| form.editing.clone());
                match id {
                    Some(id) => open_thread_response(id),
                    None => ContentViewResponse::Redraw,
                }
            }
            SshViewAction::RevealPassword => {
                self.password_visible = !self.password_visible;
                ContentViewResponse::Redraw
            }
            SshViewAction::FocusField(n) => {
                let count = self
                    .form
                    .as_ref()
                    .map(Self::visible_field_count)
                    .unwrap_or(BASE_FIELD_COUNT);
                self.focus = Focus::Field(n.min(count - 1));
                ContentViewResponse::Redraw
            }
            SshViewAction::PickIcon => self.icon_menu(),
            SshViewAction::SetNetwork(network) => {
                let tailnet_on = self.tailnet_on;
                if let Some(form) = self.form.as_mut() {
                    // Greyed out until Tailscale is turned on on this page.
                    if network == SshNetwork::Tailnet
                        && !tailnet_on
                        && form.network != SshNetwork::Tailnet
                    {
                        return ContentViewResponse::Redraw;
                    }
                    if network == SshNetwork::Tailnet && form.use_mosh {
                        form.error = Some(crate::i18n::tr("ssh-error-tailnet-mosh"));
                        return ContentViewResponse::Redraw;
                    }
                    form.network = network;
                    form.error = None;
                }
                self.save_choice(HostChoice::Network(network));
                ContentViewResponse::Redraw
            }
            SshViewAction::ToggleDetect => {
                if let Some(form) = self.form.as_mut() {
                    form.detect_os = !form.detect_os;
                    let choice = HostChoice::DetectOs(form.detect_os);
                    self.save_choice(choice);
                }
                ContentViewResponse::Redraw
            }
            SshViewAction::SetMode(mode) => {
                if let Some(form) = self.form.as_mut() {
                    if mode == HostMode::Mosh && form.network == SshNetwork::Tailnet {
                        form.error = Some(crate::i18n::tr("ssh-error-mosh-tailnet"));
                        return ContentViewResponse::Redraw;
                    }
                    form.error = None;
                    // Mosh and ThinkTerm Connect are alternative transports;
                    // only one can drive the connection.
                    form.use_mosh = mode == HostMode::Mosh;
                    form.multiplexing = mode == HostMode::Connect;
                    if form.use_mosh && form.fields[FIELD_MOSH_SERVER].text().trim().is_empty() {
                        form.fields[FIELD_MOSH_SERVER]
                            .set_text_end(ssh_hosts::DEFAULT_MOSH_SERVER_COMMAND.to_string());
                    }
                    if !form.use_mosh && self.focus == Focus::Field(FIELD_MOSH_SERVER) {
                        self.focus = Focus::Field(FIELD_WORKSPACE);
                    }
                }
                self.save_choice(HostChoice::Mode(mode));
                ContentViewResponse::Redraw
            }
            SshViewAction::SwitchToConnect(index) => {
                // Saved on the spot: a choice left waiting in the form would
                // be lost to the next Connect, which dials what is on disk.
                // The host's form then opens as a click on the card would; one
                // already open on it keeps what was typed into it.
                let Some(host_id) = self.filtered.get(index).map(|entry| entry.id.clone()) else {
                    return ContentViewResponse::Redraw;
                };
                let Some(mut spec) = ssh_hosts::host_spec(&host_id) else {
                    return ContentViewResponse::Redraw;
                };
                spec.multiplexing = true;
                spec.use_mosh = false;
                if let Err(err) = ssh_hosts::try_update_host(&host_id, spec) {
                    log::warn!("failed to switch {host_id} to ThinkTerm Connect: {err:#}");
                    return ContentViewResponse::Redraw;
                }
                self.refresh();
                if let Some(index) = self.filtered.iter().position(|entry| entry.id == host_id) {
                    self.select_host(index);
                }
                if let Some(form) = self.form.as_mut() {
                    if form.editing.as_deref() == Some(host_id.as_str()) {
                        form.multiplexing = true;
                        form.use_mosh = false;
                        form.error = None;
                    }
                }
                ContentViewResponse::Redraw
            }
            SshViewAction::AddWithConnect(index) => {
                let Some(entry) = self.filtered.get(index).cloned() else {
                    return ContentViewResponse::Redraw;
                };
                self.open_connect_form_from(&entry.spec);
                // The form is a new host's, not the selected card's.
                self.selection_active = false;
                ContentViewResponse::Redraw
            }
            SshViewAction::ToggleShellIntegration => {
                if let Some(form) = self.form.as_mut() {
                    form.shell_integration = !form.shell_integration;
                    let choice = HostChoice::ShellIntegration(form.shell_integration);
                    self.save_choice(choice);
                }
                ContentViewResponse::Redraw
            }
            SshViewAction::ToggleFilesAutoConnect => {
                if let Some(form) = self.form.as_mut() {
                    form.files_auto_connect = !form.files_auto_connect;
                    let choice = HostChoice::FilesAutoConnect(form.files_auto_connect);
                    self.save_choice(choice);
                }
                ContentViewResponse::Redraw
            }
            SshViewAction::Save => {
                self.persist_form();
                ContentViewResponse::Redraw
            }
            SshViewAction::SaveAndConnect => match self.persist_form() {
                Some(id) => open_thread_response(id),
                None => ContentViewResponse::Redraw,
            },
            SshViewAction::Cancel => {
                self.close_form();
                ContentViewResponse::Redraw
            }
            SshViewAction::ToggleSystemHosts => {
                self.system_hosts_collapsed = !self.system_hosts_collapsed;
                // The list is about to be re-filtered under the highlight, and
                // `selected` is an index into it. Rather than let the ring --
                // and Enter -- land on whichever host inherits that slot, drop
                // the selection.
                self.selection_active = false;
                self.refresh();
                ContentViewResponse::Redraw
            }
            SshViewAction::ToggleTailnetGroup => {
                self.tailnet.collapsed = !self.tailnet.collapsed;
                ContentViewResponse::Redraw
            }
            SshViewAction::TailnetSettings => {
                if self.tailnet.panel_open {
                    self.close_tailnet_panel();
                } else {
                    self.open_tailnet_panel();
                }
                ContentViewResponse::Redraw
            }
            SshViewAction::TailnetMachine(index) => self.open_tailnet_machine(index),
            SshViewAction::TailnetNotice => match self.tailnet.notice {
                Some(NoticeButton::TurnOn) => {
                    // What was typed into the panel goes first: a control
                    // server is set before the node starts with it.
                    self.commit_tailnet_fields(None);
                    crate::tailnet::turn_on();
                    self.open_tailnet_panel();
                    ContentViewResponse::Redraw
                }
                Some(NoticeButton::LogIn) => self.apply(SshViewAction::OpenTailnetLogin),
                Some(NoticeButton::Settings) => {
                    self.open_tailnet_panel();
                    ContentViewResponse::Redraw
                }
                None => ContentViewResponse::Redraw,
            },
            SshViewAction::ToggleTailnet => {
                self.commit_tailnet_fields(None);
                crate::tailnet::toggle();
                ContentViewResponse::Redraw
            }
            SshViewAction::OpenTailnetLogin => {
                if let Some(url) = self.tailnet.login_url.as_deref() {
                    wezterm_open_url::open_url(url);
                }
                ContentViewResponse::Redraw
            }
            SshViewAction::CopyTailnetLogin => {
                let Some(url) = self.tailnet.login_url.clone() else {
                    return ContentViewResponse::Redraw;
                };
                self.tailnet.copied_until = Some(Instant::now() + COPIED_FOR);
                ContentViewResponse::Run(Box::new(move |term_window| {
                    term_window.copy_to_clipboard(
                        config::keyassignment::ClipboardCopyDestination::Clipboard,
                        url,
                    );
                    term_window.invalidate_window();
                }))
            }
            SshViewAction::FocusTailnetField(field) => {
                self.focus = Focus::Tailnet(field.min(TAILNET_FIELD_COUNT - 1));
                ContentViewResponse::Redraw
            }
            SshViewAction::TailnetLogout => {
                self.commit_tailnet_fields(None);
                crate::tailnet::logout();
                ContentViewResponse::Redraw
            }
        }
    }

    /// Show a host in the inspector. A host that came from `~/.ssh/config`
    /// has no record of ours to edit, so selecting one only highlights it.
    fn select_host(&mut self, index: usize) {
        self.selected = index;
        self.selection_active = true;
        self.tailnet.selected = None;
        let Some(entry) = self.filtered.get(index) else {
            return;
        };
        let id = entry.id.clone();
        // Re-selecting the host already open would rebuild its form from disk
        // and throw away everything typed into it -- including on the first
        // click of a double click, which is a gesture, not an edit.
        if self.form.as_ref().and_then(|form| form.editing.as_deref()) == Some(id.as_str()) {
            return;
        }
        match entry.source {
            SshHostSource::ThinkTerm => self.open_edit_form(&id),
            SshHostSource::System => self.close_form(),
        }
    }

    /// Delete a host and everything that only existed because of it.
    fn delete_host(&mut self, host_id: &str) -> ContentViewResponse {
        let Some(entry) = self
            .filtered
            .iter()
            .find(|entry| entry.id == host_id)
            .filter(|entry| entry.source == SshHostSource::ThinkTerm)
        else {
            return ContentViewResponse::Redraw;
        };
        // Read what this host brought in while the record naming it is still
        // here. A ThinkTerm Connect host mirrors whole Spaces from its mux
        // server, and those are keyed by domain name, not by host id -- so
        // `remove_project` below, which matches the host id, never sees them.
        // Left behind they are unreachable: nothing resolves the domain to
        // connect, and every rename or delete is refused because it has to go
        // through a server this device can no longer name.
        let host_domains = ssh_hosts::domain_names_for_host(&entry.spec);
        let orphaned_spaces = workspace_threads::space_ids_for_domains(&host_domains);

        if let Err(err) = ssh_hosts::try_remove_host(host_id) {
            log::error!("failed to delete SSH host {host_id}: {err:#}");
            self.refresh();
            return ContentViewResponse::Redraw;
        }
        let _ = workspace_threads::remove_project(host_id);
        // References into this host's mux Spaces would otherwise linger as
        // permanent grey rows: with the host record gone, nothing can ever
        // resolve them again.
        workspace_threads::purge_thread_refs_for_machines(&host_domains);
        if self.form.as_ref().and_then(|form| form.editing.as_deref()) == Some(host_id) {
            self.close_form();
        }
        self.selection_active = false;
        self.refresh();

        if orphaned_spaces.is_empty() {
            return ContentViewResponse::Redraw;
        }
        // Through the window, because one of them may be the Space this very
        // window is showing; that path moves it off first. Local removal: the
        // server keeps its Spaces, so re-adding the host brings them back.
        ContentViewResponse::Run(Box::new(move |term_window| {
            for space_id in orphaned_spaces {
                term_window.start_delete_space(
                    &space_id,
                    workspace_threads::SpaceRemoval::Local,
                    false,
                );
            }
        }))
    }

    /// Duplicate a host under a new name, ready to be edited.
    fn duplicate_host(&mut self, host_id: &str) -> ContentViewResponse {
        let Some(spec) = self
            .filtered
            .iter()
            .find(|entry| entry.id == host_id)
            .map(|entry| entry.spec.clone())
        else {
            return ContentViewResponse::Redraw;
        };
        self.open_new_form();
        if let Some(form) = self.form.as_mut() {
            form.fields[FIELD_NAME].set_text_end(format!("{} copy", spec.label));
            // Everything but the hostname. A record is keyed by its endpoint,
            // so a duplicate carrying the original's host could never be
            // saved -- it would collide with the host it was copied from.
            // `open_new_form` has already put the caret in this field.
            if let Some(port) = spec.port {
                form.fields[FIELD_PORT] = text_input_with(port.to_string());
            }
            if let Some(user) = spec.username.clone() {
                form.fields[FIELD_USER].set_text_end(user);
            }
            if let Some(identity) = spec.identity_file.clone() {
                form.fields[FIELD_IDENTITY].set_text_end(identity);
            }
            if let Some(workspace) = spec.default_workspace.clone() {
                form.fields[FIELD_WORKSPACE].set_text_end(workspace);
            }
            if let Some(password) = spec.password.as_deref().map(crate::secret::reveal) {
                form.fields[FIELD_PASSWORD] = text_input_with(password);
            }
            form.fields[FIELD_MOSH_SERVER] = text_input_with(spec.mosh_server_command.clone());
            form.detect_os = spec.detect_os;
            form.use_mosh = spec.use_mosh;
            form.multiplexing = spec.multiplexing;
            form.shell_integration = spec.shell_integration;
            form.files_auto_connect = spec.files_auto_connect;
            form.icon = spec.icon;
            form.options = spec
                .ssh_options
                .iter()
                .map(|(key, value)| (text_input_with(key.clone()), text_input_with(value.clone())))
                .collect();
            form.advanced_open = !form.options.is_empty();
        }
        ContentViewResponse::Redraw
    }

    /// Run one of the card menu's entries. Reached from the window, which is
    /// where the native menu reports back to.
    pub(crate) fn run_host_command(
        &mut self,
        host_id: &str,
        command: RemoteHostCommand,
    ) -> ContentViewResponse {
        match command {
            RemoteHostCommand::Connect => open_thread_response(host_id.to_string()),
            RemoteHostCommand::Edit => {
                if let Some(index) = self.filtered.iter().position(|e| e.id == host_id) {
                    self.select_host(index);
                }
                ContentViewResponse::Redraw
            }
            RemoteHostCommand::Duplicate => self.duplicate_host(host_id),
            RemoteHostCommand::Delete => self.delete_host(host_id),
            RemoteHostCommand::SetIcon(icon) => self.set_form_icon(host_id, icon),
        }
    }

    /// The icon button's menu, under the button. The page cannot open a
    /// menu itself, as for a card's.
    fn icon_menu(&mut self) -> ContentViewResponse {
        let Some(form) = self.form.as_ref() else {
            return ContentViewResponse::Redraw;
        };
        let host_id = form.editing.clone().unwrap_or_default();
        let current = form.icon;
        let (x, y) = (self.icon_button.origin.x, self.icon_button.max_y());
        ContentViewResponse::Run(Box::new(move |term_window| {
            term_window.show_remote_host_icon_menu(host_id, current, x, y);
        }))
    }

    /// `icon`, picked in the icon menu, on the form for `host_id` (empty
    /// for a new host) if that form is still the one open. The plain server
    /// is what no icon shows, so picking it saves none.
    fn set_form_icon(&mut self, host_id: &str, icon: HostIcon) -> ContentViewResponse {
        let Some(form) = self.form.as_mut() else {
            return ContentViewResponse::Redraw;
        };
        if form.editing.as_deref().unwrap_or("") != host_id {
            return ContentViewResponse::Redraw;
        }
        let icon = (icon != HostIcon::Server).then_some(icon);
        form.icon = icon;
        self.save_choice(HostChoice::Icon(icon));
        ContentViewResponse::Redraw
    }

    /// The card under the pointer, if any.
    fn card_at(&self, x: f32, y: f32) -> Option<usize> {
        self.card_rects
            .iter()
            .rev()
            .find(|(_, rect)| contains(*rect, x, y))
            .map(|(index, _)| *index)
    }

    /// Native context menu for one host card. The page cannot open a menu
    /// itself -- menus belong to the window -- so it hands the window a
    /// callback and the window pops it at the same coordinates.
    fn host_context_menu(&mut self, x: f32, y: f32) -> ContentViewResponse {
        let Some(index) = self.card_at(x, y) else {
            return ContentViewResponse::Ignored;
        };
        let Some(entry) = self.filtered.get(index) else {
            return ContentViewResponse::Ignored;
        };
        let host_id = entry.id.clone();
        let label = entry.spec.label.clone();
        // A host read out of ~/.ssh/config is ours to connect to, not to
        // rewrite: this page never writes that file back.
        let editable = entry.source == SshHostSource::ThinkTerm;
        self.selected = index;
        self.selection_active = true;
        // Its ring takes over from a tailnet machine's.
        self.tailnet.selected = None;
        ContentViewResponse::Run(Box::new(move |term_window| {
            term_window.show_remote_host_menu(host_id, label, editable, x, y);
        }))
    }

    fn on_key_impl(&mut self, key: KeyCode, mods: KeyModifiers) -> ContentViewResponse {
        // Up/Down drive the host list, so they must reach the navigation match
        // below even while a field has focus.
        if !matches!(key, KeyCode::UpArrow | KeyCode::DownArrow) {
            if let Some(response) = self.handle_text_editing_key(key, mods) {
                return response;
            }
        }

        if self.tailnet.panel_open {
            if matches!(key, KeyCode::Escape) {
                // Leaving without sending what was typed: a half-typed key
                // or address would restart the node on it.
                self.tailnet.dirty = [false; TAILNET_FIELD_COUNT];
                self.close_tailnet_panel();
                return ContentViewResponse::Redraw;
            }
            if let Focus::Tailnet(field) = self.focus {
                match (key, mods) {
                    (KeyCode::Enter, _) => {
                        self.commit_tailnet_field(field);
                        return ContentViewResponse::Redraw;
                    }
                    (KeyCode::Tab, KeyModifiers::NONE) => {
                        self.focus = Focus::Tailnet((field + 1) % TAILNET_FIELD_COUNT);
                        return ContentViewResponse::Redraw;
                    }
                    (KeyCode::Tab, KeyModifiers::SHIFT) => {
                        self.focus =
                            Focus::Tailnet((field + TAILNET_FIELD_COUNT - 1) % TAILNET_FIELD_COUNT);
                        return ContentViewResponse::Redraw;
                    }
                    _ => {}
                }
            }
        }

        let in_form = self.form.is_some();
        match (key, mods) {
            (KeyCode::Escape, _) => {
                if in_form {
                    self.close_form();
                    ContentViewResponse::Redraw
                } else {
                    ContentViewResponse::Close
                }
            }
            (KeyCode::Enter, _) => {
                if in_form {
                    match self.persist_form() {
                        Some(id) => open_thread_response(id),
                        None => ContentViewResponse::Redraw,
                    }
                } else if let Some(entry) = self.filtered.get(self.selected) {
                    open_thread_response(entry.id.clone())
                } else {
                    ContentViewResponse::Redraw
                }
            }
            (KeyCode::Tab, KeyModifiers::NONE) | (KeyCode::DownArrow, KeyModifiers::NONE) => {
                if in_form {
                    self.step_field(1);
                } else if !self.filtered.is_empty() {
                    self.selected = (self.selected + 1).min(self.filtered.len() - 1);
                    self.selection_active = true;
                }
                ContentViewResponse::Redraw
            }
            (KeyCode::Tab, KeyModifiers::SHIFT) | (KeyCode::UpArrow, KeyModifiers::NONE) => {
                if in_form {
                    self.step_field(-1);
                } else {
                    self.selected = self.selected.saturating_sub(1);
                    self.selection_active = true;
                }
                ContentViewResponse::Redraw
            }
            _ => ContentViewResponse::Ignored,
        }
    }

    /// Text-editing keys shared by the search box and every form field.
    /// Returns `None` when the key is not an editing key, so the caller falls
    /// through to view-level navigation (Tab / Enter / Escape / list arrows).
    ///
    /// Chords are resolved through [`EditModifiers`] rather than testing raw
    /// modifiers, so ⌘ on macOS and Ctrl on Windows/Linux behave identically.
    fn handle_text_editing_key(
        &mut self,
        key: KeyCode,
        mods: KeyModifiers,
    ) -> Option<ContentViewResponse> {
        let edit = EditModifiers::from(mods);
        let shift = edit.shift;
        let macos = cfg!(target_os = "macos");

        if edit.command {
            match key {
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    self.edit_focused(|input| input.caret_select_all());
                    return Some(ContentViewResponse::Redraw);
                }
                // Clipboard lives on TermWindow (it owns the window handle), so
                // hand the work back rather than duplicating it here.
                KeyCode::Char('c') | KeyCode::Char('C') => {
                    return Some(ContentViewResponse::Run(Box::new(|term_window| {
                        term_window.content_view_copy()
                    })));
                }
                KeyCode::Char('x') | KeyCode::Char('X') => {
                    return Some(ContentViewResponse::Run(Box::new(|term_window| {
                        term_window.content_view_cut()
                    })));
                }
                KeyCode::Char('v') | KeyCode::Char('V') => {
                    return Some(ContentViewResponse::Run(Box::new(|term_window| {
                        term_window.content_view_paste()
                    })));
                }
                // ⌘←/→/⌫ are line-start/end/delete-to-start on macOS. Elsewhere
                // the same physical chord is the word modifier, handled below.
                KeyCode::LeftArrow if macos => {
                    self.edit_focused(|input| input.caret_move_home(shift));
                    return Some(ContentViewResponse::Redraw);
                }
                KeyCode::RightArrow if macos => {
                    self.edit_focused(|input| input.caret_move_end(shift));
                    return Some(ContentViewResponse::Redraw);
                }
                KeyCode::Backspace if macos => {
                    self.edit_focused(|input| input.caret_delete_to_start());
                    return Some(ContentViewResponse::Redraw);
                }
                _ => {}
            }
        }

        if edit.word {
            match key {
                KeyCode::LeftArrow => {
                    self.edit_focused(|input| input.caret_word_left(shift));
                    return Some(ContentViewResponse::Redraw);
                }
                KeyCode::RightArrow => {
                    self.edit_focused(|input| input.caret_word_right(shift));
                    return Some(ContentViewResponse::Redraw);
                }
                KeyCode::Backspace => {
                    self.edit_focused(|input| input.caret_delete_word_back());
                    return Some(ContentViewResponse::Redraw);
                }
                _ => {}
            }
        }

        if !edit.plain() {
            return None;
        }

        match key {
            KeyCode::LeftArrow => {
                self.edit_focused(|input| input.caret_move_left(shift));
                Some(ContentViewResponse::Redraw)
            }
            KeyCode::RightArrow => {
                self.edit_focused(|input| input.caret_move_right(shift));
                Some(ContentViewResponse::Redraw)
            }
            KeyCode::Home => {
                self.edit_focused(|input| input.caret_move_home(shift));
                Some(ContentViewResponse::Redraw)
            }
            KeyCode::End => {
                self.edit_focused(|input| input.caret_move_end(shift));
                Some(ContentViewResponse::Redraw)
            }
            KeyCode::Backspace => {
                self.edit_focused(|input| input.caret_backspace());
                Some(ContentViewResponse::Redraw)
            }
            KeyCode::Delete => {
                self.edit_focused(|input| input.caret_delete_forward());
                Some(ContentViewResponse::Redraw)
            }
            KeyCode::Char(c) if !c.is_control() => {
                self.edit_focused(|input| input.caret_insert(&c.to_string(), false));
                Some(ContentViewResponse::Redraw)
            }
            _ => None,
        }
    }

    fn on_paste_impl(&mut self, text: &str) -> ContentViewResponse {
        let cleaned: String = text.chars().filter(|c| !c.is_control()).collect();
        self.edit_focused(|input| input.caret_insert(&cleaned, false));
        ContentViewResponse::Redraw
    }

    /// Tab order: the regular fields, then each visible `ssh_options` row as a
    /// key/value pair. Wraps in both directions.
    fn form_tab_stops(&self) -> Vec<Focus> {
        self.form.as_ref().map(tab_stops_for).unwrap_or_default()
    }

    fn step_field(&mut self, delta: isize) {
        self.clear_focused_selection();
        let stops = self.form_tab_stops();
        if stops.is_empty() {
            self.focus = Focus::Field(FIELD_HOST);
            return;
        }
        let current = stops.iter().position(|stop| *stop == self.focus);
        self.focus = match current {
            Some(index) => {
                let next = (index as isize + delta).rem_euclid(stops.len() as isize) as usize;
                stops[next]
            }
            // Coming from the search box (or a row that just disappeared).
            None => Focus::Field(FIELD_HOST),
        };
    }

    /// Turn a recorded click into a caret position now that a `DrawContext`
    /// and the field's laid-out text origin are available. `text` is what is
    /// actually drawn (the password field passes its mask, which is one glyph
    /// per character, so indices still line up).
    fn resolve_pending_caret_click(
        &mut self,
        ctx: &DrawContext,
        font: &Rc<LoadedFont>,
        focus: Focus,
        text: &str,
        text_left: f32,
    ) {
        let Some((target, x)) = self.pending_caret_click else {
            return;
        };
        if target != focus {
            return;
        }
        self.pending_caret_click = None;
        let index = char_index_for_x(ctx, font, text, x - text_left);
        if let Some(input) = self.focused_input_mut() {
            input.caret_set(index, false);
        }
    }

    fn clear_focused_selection(&mut self) {
        if let Some(input) = self.focused_input_mut() {
            input.clear_selection();
        }
    }

    /// Caret + selection to render for `focus`, or `None` when that field is
    /// not the focused one (an unfocused field never shows a caret).
    fn caret_for(&self, focus: Focus) -> Option<InputCaret> {
        if self.focus != focus {
            return None;
        }
        let input = self.focused_input()?;
        Some(InputCaret {
            cursor: input.cursor,
            selection: input.caret_selection_range(),
        })
    }

    fn focused_input(&self) -> Option<&TextInputState> {
        match self.focus {
            Focus::Search => Some(&self.search),
            Focus::Field(n) => self.form.as_ref()?.fields.get(n),
            Focus::OptionKey(n) => {
                let form = self.form.as_ref()?;
                form.advanced_open
                    .then(|| form.options.get(n).map(|(key, _)| key))
                    .flatten()
            }
            Focus::OptionValue(n) => {
                let form = self.form.as_ref()?;
                form.advanced_open
                    .then(|| form.options.get(n).map(|(_, value)| value))
                    .flatten()
            }
            Focus::Tailnet(n) => self
                .tailnet
                .panel_open
                .then(|| self.tailnet.fields.get(n))
                .flatten(),
        }
    }

    /// Mutable access to whatever field currently has focus. Clears the form's
    /// error banner, since any edit invalidates the last validation result.
    fn focused_input_mut(&mut self) -> Option<&mut TextInputState> {
        match self.focus {
            Focus::Search => Some(&mut self.search),
            Focus::Field(n) => {
                let form = self.form.as_mut()?;
                form.error = None;
                form.fields.get_mut(n)
            }
            Focus::OptionKey(n) => {
                let form = self.form.as_mut()?;
                if !form.advanced_open {
                    return None;
                }
                form.error = None;
                form.options.get_mut(n).map(|(key, _)| key)
            }
            Focus::OptionValue(n) => {
                let form = self.form.as_mut()?;
                if !form.advanced_open {
                    return None;
                }
                form.error = None;
                form.options.get_mut(n).map(|(_, value)| value)
            }
            Focus::Tailnet(n) => {
                if !self.tailnet.panel_open {
                    return None;
                }
                self.tailnet.fields.get_mut(n)
            }
        }
    }

    /// Run `f` against the focused field, then re-filter the host list if the
    /// search box was the one edited.
    fn edit_focused(&mut self, f: impl FnOnce(&mut TextInputState)) {
        let focus = self.focus;
        let Some(input) = self.focused_input_mut() else {
            return;
        };
        // A Tailscale field goes out once it is left, and only if what it
        // says changed: moving the caret through it is no edit.
        let before = matches!(focus, Focus::Tailnet(_)).then(|| input.text().to_string());
        f(input);
        let changed = before.is_some_and(|before| before != input.text());
        if let (true, Focus::Tailnet(field)) = (changed, focus) {
            self.tailnet.dirty[field] = true;
        }
        if matches!(self.focus, Focus::Search) {
            // Typing in the search box reshuffles the list under the
            // highlight, so the old choice no longer means anything.
            self.selected = 0;
            self.selection_active = false;
            self.refresh();
        }
    }

    fn take_focused_selection(&mut self) -> Option<String> {
        let mut taken = None;
        self.edit_focused(|input| taken = input.caret_take_selected_text());
        taken
    }

    // ---- paint ------------------------------------------------------------

    /// Height of a host card: two lines of text plus breathing room.
    fn card_height(ctx: &DrawContext) -> f32 {
        (ctx.metrics.cell_size.height as f32 * 2.05 + ctx.px(50.0)).max(ctx.px(104.0))
    }

    /// The card introducing ThinkTerm Connect, `width` wide: its words --
    /// title, body and button, one under the other -- down the left, and a
    /// picture of what it means beside them, or under them when the page is
    /// too narrow for both.
    fn connect_tip_layout(
        ctx: &DrawContext,
        section_ctx: &DrawContext,
        caption_ctx: &DrawContext,
        section_font: &Rc<LoadedFont>,
        caption_font: &Rc<LoadedFont>,
        width: f32,
    ) -> ConnectTipLayout {
        let section_h = section_ctx.metrics.cell_size.height as f32;
        let body_h = caption_ctx.metrics.cell_size.height as f32;
        let line_h = ctx.metrics.cell_size.height as f32;
        // A wider margin at the sides than the host cards': a banner reads
        // better with room around its words.
        let pad = ctx.px(TIP_PAD_X);
        let gap = ctx.px(48.0);
        let words_end = width - pad;
        // The whole picture beside the words; then the host alone, which is
        // the half that says what Connect keeps; then the host under them.
        let beside = words_end - gap - pad - ctx.px(TIP_TEXT_MIN_W);
        let host_w = ctx.px(TIP_HOST_W);
        let picture = if beside >= ctx.px(TIP_PICTURE_MIN_W) {
            TipPicture::Beside {
                width: beside.min(ctx.px(TIP_PICTURE_W)),
                computer: true,
            }
        } else if words_end - gap - pad - ctx.px(TIP_TEXT_NARROW_W) >= host_w {
            TipPicture::Beside {
                width: host_w,
                computer: false,
            }
        } else if width - pad * 2.0 >= host_w {
            // Under the words it spans the card, this computer too when
            // there is room.
            let width = width - pad * 2.0;
            TipPicture::Below {
                width,
                computer: width >= ctx.px(TIP_PICTURE_MIN_W),
            }
        } else {
            TipPicture::None
        };
        let text_w = match picture {
            TipPicture::Beside { width, .. } => words_end - width - gap - pad,
            TipPicture::Below { .. } | TipPicture::None => words_end - pad,
        }
        .max(0.0);
        // The words beside the picture can be narrower than the title.
        let title = wrap_snippet_text_for_width(
            &crate::i18n::tr("ssh-connect-tip-title"),
            2,
            false,
            |segment| section_ctx.measure_text_width(section_font, segment) / text_w.max(1.0),
        );
        let body = wrap_snippet_text_for_width(
            &crate::i18n::tr("ssh-connect-tip-body"),
            5,
            false,
            |segment| caption_ctx.measure_text_width(caption_font, segment) / text_w.max(1.0),
        );
        let words_h = section_h * title.len() as f32
            + ctx.px(TIP_TITLE_GAP)
            + body_h * body.len() as f32
            + ctx.px(TIP_BUTTON_GAP)
            + ctx.px(BTN_H);
        let picture_h = line_h * 4.0 + ctx.px(92.0);
        let inside = match picture {
            TipPicture::Beside { .. } => words_h.max(picture_h),
            TipPicture::Below { .. } => words_h + ctx.px(TIP_BUTTON_GAP) + picture_h,
            TipPicture::None => words_h,
        };
        ConnectTipLayout {
            picture,
            text_w,
            title,
            body,
            words_h,
            picture_h,
            height: ctx.px(CARD_PAD) * 2.0 + inside,
        }
    }

    /// One column count shared by both groups, so the two grids line up with
    /// each other instead of each picking its own width.
    fn card_columns(ctx: &DrawContext, counts: &[usize], content_width: f32) -> usize {
        shared_grid_columns(
            counts,
            content_width,
            MAX_CARD_COLUMNS,
            ctx.px(CARD_MIN_W),
            ctx.px(CARD_MIN_W + 60.0),
            ctx.px(CARD_GAP),
        )
    }

    /// Indices into `filtered`, split into the two groups the grid draws:
    /// this app's own hosts first, then whatever `~/.ssh/config` contributed.
    fn grouped_indices(&self) -> (Vec<usize>, Vec<usize>) {
        let mut own = Vec::new();
        let mut system = Vec::new();
        for (index, entry) in self.filtered.iter().enumerate() {
            match entry.source {
                SshHostSource::ThinkTerm => own.push(index),
                SshHostSource::System => system.push(index),
            }
        }
        (own, system)
    }

    fn system_group_expanded(&self) -> bool {
        !self.system_hosts_collapsed || !self.search.text().trim().is_empty()
    }

    /// What a card says under the host name: where it connects, and -- unless
    /// it is a saved Connect host, whose whole card says so -- how.
    ///
    /// Spelled out after the endpoint rather than shown as a corner tag: a
    /// two-word tag next to the title has to be read as a label anyway, and
    /// "connect" as a tag collides with the verb.
    fn card_subtitle(entry: &SshHostEntry) -> String {
        let endpoint = ssh_hosts::endpoint(&entry.spec);
        // A Connect host says so with its whole card; the endpoint gets the
        // room.
        let mode = match HostMode::of(entry.spec.use_mosh, entry.spec.multiplexing) {
            HostMode::Connect if entry.source != SshHostSource::System => return endpoint,
            HostMode::Connect | HostMode::Ssh => "ssh-mode-ssh-title",
            HostMode::Mosh => "ssh-mode-mosh-title",
        };
        format!("{endpoint}  ·  {}", crate::i18n::tr(mode))
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_impl(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
        title_font: &Rc<LoadedFont>,
        section_font: &Rc<LoadedFont>,
        caption_font: &Rc<LoadedFont>,
        cursor_on: bool,
    ) -> anyhow::Result<()> {
        let tokens = UiTokens::for_dpi(ctx.dimensions.dpi);
        self.refresh();
        let tailnet = self.read_tailnet();
        self.tailnet_on = tailnet.enabled;
        self.widgets.clear();
        self.card_rects.clear();

        // Headings need their own context: text sits on the baseline of the
        // metrics the *context* carries, not of the font handed to draw_text,
        // so a larger font drawn through the body context lands on the body
        // baseline and looks a few pixels high.
        let title_metrics = RenderMetrics::with_font_metrics(&title_font.metrics());
        let section_metrics = RenderMetrics::with_font_metrics(&section_font.metrics());
        let caption_metrics = RenderMetrics::with_font_metrics(&caption_font.metrics());
        let title_ctx = ctx.with_metrics(&title_metrics);
        let section_ctx = ctx.with_metrics(&section_metrics);
        let caption_ctx = ctx.with_metrics(&caption_metrics);

        let ox = area.origin.x;
        let oy = area.origin.y;
        let w = area.size.width;
        let h = area.size.height;
        self.last_ui_scale = ctx.px(1.0).max(0.01);
        // The glide a notched wheel was given lives in the scroll states.
        let now = Instant::now();
        self.scroll_animating =
            self.scroll.advance_animation(now) | self.form_scroll.advance_animation(now);

        ctx.draw_rect(layers, 0, ox, oy, w, h, palette.window_bg)?;

        let pad = ctx.px(PAD);
        // The inspector takes its fixed width off the top and the grid lays
        // out in what is left. That is the whole responsive story: widen the
        // window and the leftover width becomes more columns, not a wider
        // form floating in the middle of the page.
        let inspector_room = ((w - pad * 2.0) * 0.6).max(0.0);
        self.inspector_max_width = inspector_room / self.last_ui_scale;
        let inspector_w = if self.form.is_some() || self.tailnet.panel_open {
            ctx.px(self.inspector_width).min(inspector_room)
        } else {
            0.0
        };
        let inspector_gap = if inspector_w > 0.0 {
            ctx.px(INSPECTOR_GAP)
        } else {
            0.0
        };
        // The gap before the inspector is its edge, to drag.
        self.inspector_edge = if inspector_w > 0.0 {
            rect(
                ox + w - pad - inspector_w - inspector_gap,
                oy + pad,
                inspector_gap + ctx.px(6.0),
                (h - pad).max(0.0),
            )
        } else {
            self.inspector_drag = None;
            self.inspector_edge_hovered = false;
            rect(0.0, 0.0, 0.0, 0.0)
        };
        let content_x = ox + pad;
        let content_w = (w - pad * 2.0 - inspector_w - inspector_gap).max(0.0);

        let toolbar_y = oy + pad;
        let toolbar_h = ctx.px(TOOLBAR_H);
        let list_top = toolbar_y + toolbar_h + ctx.px(TOOLBAR_GAP);
        let list_bottom = oy + h - pad;
        self.list_area = rect(
            content_x,
            list_top,
            content_w,
            (list_bottom - list_top).max(0.0),
        );

        self.paint_grid(
            ctx,
            &section_ctx,
            &caption_ctx,
            layers,
            font,
            section_font,
            caption_font,
            palette,
            &tailnet,
            content_x,
            content_w,
            list_top,
            list_bottom,
        )?;

        // Cards scroll under the toolbar and past the bottom edge. Blank both
        // strips on every layer, then repaint the toolbar above them.
        let masked_w = (content_x + content_w) - ox;
        self.paint_list_mask(
            ctx,
            layers,
            ox,
            oy,
            masked_w,
            (list_top - oy).max(0.0),
            palette,
        )?;
        self.paint_list_mask(
            ctx,
            layers,
            ox,
            list_bottom,
            masked_w,
            (oy + h - list_bottom).max(0.0),
            palette,
        )?;
        self.paint_list_fades(ctx, layers, self.list_area, palette, self.scroll)?;
        self.paint_toolbar(
            ctx, layers, font, palette, tokens, cursor_on, content_x, toolbar_y, content_w,
        )?;
        if self.scroll.has_overflow() {
            draw_scrollbar(ctx, layers, palette, tokens, self.list_area, self.scroll)?;
        }

        if inspector_w > 0.0 && (self.inspector_edge_hovered || self.inspector_drag.is_some()) {
            let line = ctx.px(2.0);
            ctx.draw_rect(
                layers,
                1,
                ox + w - pad - inspector_w - (inspector_gap + line) / 2.0,
                oy + pad,
                line,
                (h - pad).max(0.0),
                palette.accent.mul_alpha(0.7),
            )?;
        }
        if inspector_w > 0.0 {
            // Down to the window's edge: what does not fit scrolls off the
            // bottom of the window, not off a line drawn above it. The body
            // already ends in a pad of its own.
            let inspector = rect(
                ox + w - pad - inspector_w,
                oy + pad,
                inspector_w,
                (h - pad).max(0.0),
            );
            if self.tailnet.panel_open {
                self.paint_tailnet_panel(
                    ctx,
                    &title_ctx,
                    &section_ctx,
                    layers,
                    font,
                    title_font,
                    section_font,
                    palette,
                    tokens,
                    cursor_on,
                    inspector,
                    &tailnet,
                )?;
            } else {
                self.paint_inspector(
                    ctx,
                    &title_ctx,
                    &section_ctx,
                    &caption_ctx,
                    layers,
                    font,
                    title_font,
                    section_font,
                    caption_font,
                    palette,
                    tokens,
                    cursor_on,
                    inspector,
                )?;
            }
        } else {
            self.form_area = rect(0.0, 0.0, 0.0, 0.0);
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_toolbar(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        palette: UiPalette,
        tokens: UiTokens,
        cursor_on: bool,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<()> {
        // Everything here is painted on layer 2, above the mask that hides the
        // cards scrolling underneath.
        let height = ctx.px(TOOLBAR_H);
        let new_label = crate::i18n::tr("ssh-new-host");
        let new_w = (ctx.measure_text_width(font, &new_label) + ctx.px(60.0)).min(width);
        let new_state = self.button_state(SshViewAction::New);
        draw_button_on_layer(
            ctx,
            layers,
            font,
            &mut self.widgets,
            palette,
            ButtonSpec {
                label: &new_label,
                action: SshViewAction::New,
                rect: rect(x, y, new_w, height),
                state: new_state,
                kind: WidgetKind::Button,
                variant: ButtonVariant::Primary,
            },
            2,
        )?;

        let gap = ctx.px(12.0);
        let search_x = x + new_w + gap;
        let search_w = (width - new_w - gap).min(ctx.px(SEARCH_W)).max(0.0);
        if search_w <= 0.0 {
            return Ok(());
        }
        let search_rect = rect(search_x, y, search_w, height);
        // A magnifier, and the same pill the button next to it is: two
        // controls sharing a toolbar should share a shape.
        let glyph = (ctx.metrics.cell_size.height as f32).clamp(ctx.px(18.0), ctx.px(24.0));
        let text_pad = ctx.px(16.0) + glyph + ctx.px(10.0);
        let search_tokens = UiTokens {
            control_radius: height / 2.0,
            ..tokens
        };
        let search_text = self.search.text().to_string();
        self.resolve_pending_caret_click(
            ctx,
            font,
            Focus::Search,
            &search_text,
            search_rect.origin.x + text_pad,
        );
        let caret = self.caret_for(Focus::Search);
        draw_text_input_on_layer(
            ctx,
            layers,
            font,
            &mut self.widgets,
            &self.interaction,
            palette,
            search_tokens,
            text_pad,
            cursor_on,
            TextInputSpec {
                placeholder: &crate::i18n::tr("ssh-search"),
                text: &search_text,
                rect: search_rect,
                focused: matches!(self.focus, Focus::Search),
                selected_all: false,
                action: SshViewAction::Search,
            },
            caret,
            2,
        )?;
        ctx.draw_svg_icon(
            layers,
            SvgIcon::Search,
            search_rect.origin.x + ctx.px(16.0),
            search_rect.origin.y + (height - glyph) / 2.0,
            glyph,
            palette.muted_text,
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_grid(
        &mut self,
        ctx: &DrawContext,
        section_ctx: &DrawContext,
        caption_ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        section_font: &Rc<LoadedFont>,
        caption_font: &Rc<LoadedFont>,
        palette: UiPalette,
        tailnet: &TailnetView,
        content_x: f32,
        content_w: f32,
        list_top: f32,
        list_bottom: f32,
    ) -> anyhow::Result<()> {
        let viewport = rect(
            content_x,
            list_top,
            content_w,
            (list_bottom - list_top).max(0.0),
        );
        let tailnet_body = self.tailnet_body(tailnet);
        self.tailnet.notice = None;
        let tip = self.connect_tip_shown();
        if self.filtered.is_empty()
            && self.system_host_count == 0
            && tailnet_body.is_none()
            && !tip
        {
            self.scroll.set_extents(viewport.size.height, 0.0);
            return self.paint_empty_state(ctx, layers, font, palette, viewport);
        }

        let (own, system) = self.grouped_indices();
        // A lone "Hosts" caption over the only group on the page is noise;
        // the captions earn their place once there is a second group to tell
        // apart from the first -- and the Tailscale group always has one, for
        // its caption carries the tailnet's state and its gear.
        let grouped = self.system_host_count > 0 || tailnet_body.is_some();
        // Its cards, while it shows them.
        let machines = match &tailnet_body {
            Some(TailnetBody::Machines) if !self.tailnet.collapsed => self.tailnet.machines.len(),
            _ => 0,
        };
        let counts = [own.len(), machines, system.len()];
        let columns = Self::card_columns(ctx, &counts, content_w);
        let gap = ctx.px(CARD_GAP);
        let card_h = Self::card_height(ctx);
        let header_h = ctx.px(GROUP_HEADER_H);
        let max_w = ctx.px(CARD_MAX_W);

        let own_grid = card_grid(own.len(), content_w, columns, max_w, gap, card_h);
        let tailnet_grid = card_grid(machines, content_w, columns, max_w, gap, card_h);
        let system_grid = card_grid(system.len(), content_w, columns, max_w, gap, card_h);

        let title_gap = ctx.px(GROUP_TITLE_GAP);
        let caption_h = if grouped { header_h + title_gap } else { 0.0 };
        // An empty Hosts group has nothing to say under its caption.
        let show_own = !own.is_empty();
        let tailnet_h = match &tailnet_body {
            _ if self.tailnet.collapsed => 0.0,
            Some(TailnetBody::Machines) => tailnet_grid.height(gap),
            Some(TailnetBody::Notice(notice)) => Self::tailnet_notice_height(ctx, notice),
            Some(TailnetBody::Pending) | None => 0.0,
        };
        let tip = tip.then(|| {
            Self::connect_tip_layout(
                ctx,
                section_ctx,
                caption_ctx,
                section_font,
                caption_font,
                content_w,
            )
        });
        let tip_h = tip.as_ref().map_or(0.0, |tip| tip.height);
        let mut blocks = Vec::new();
        if tip.is_some() {
            blocks.push(tip_h);
        }
        if show_own {
            blocks.push(caption_h + own_grid.height(gap));
        }
        if tailnet_body.is_some() {
            blocks.push(caption_h + tailnet_h);
        }
        if self.system_host_count > 0 {
            blocks.push(caption_h + system_grid.height(gap));
        }
        let content_h =
            blocks.iter().sum::<f32>() + ctx.px(GROUP_GAP) * blocks.len().saturating_sub(1) as f32;
        self.scroll.set_extents(viewport.size.height, content_h);

        let mut y = list_top - self.scroll.offset;
        let mut first = true;
        if let Some(tip) = &tip {
            first = false;
            self.paint_connect_tip(
                ctx,
                section_ctx,
                caption_ctx,
                layers,
                font,
                section_font,
                caption_font,
                palette,
                tip,
                rect(content_x, y, content_w, tip_h),
                viewport,
            )?;
            y += tip_h;
        }
        if show_own {
            if !std::mem::take(&mut first) {
                y += ctx.px(GROUP_GAP);
            }
            if grouped {
                self.paint_group_header(
                    section_ctx,
                    layers,
                    section_font,
                    palette,
                    &crate::i18n::tr("ssh-group-hosts"),
                    None,
                    content_x,
                    y,
                    content_w,
                    0.0,
                    viewport,
                )?;
                y += header_h + title_gap;
            }
            self.paint_cards(
                ctx,
                (caption_ctx, caption_font),
                layers,
                font,
                palette,
                &own,
                own_grid,
                content_x,
                content_w,
                y,
                gap,
                viewport,
            )?;
            y += own_grid.height(gap);
        }

        if let Some(body) = &tailnet_body {
            if !std::mem::take(&mut first) {
                y += ctx.px(GROUP_GAP);
            }
            let label = match body {
                TailnetBody::Machines => {
                    let mut args = FluentArgs::new();
                    args.set("count", self.tailnet.machines.len());
                    crate::i18n::tr_args("ssh-group-tailscale-count", &args)
                }
                TailnetBody::Notice(_) | TailnetBody::Pending => {
                    crate::i18n::tr("ssh-group-tailscale")
                }
            };
            let status_w = self.tailnet_status_width(ctx, font, tailnet);
            self.paint_group_header(
                section_ctx,
                layers,
                section_font,
                palette,
                &label,
                Some((SshViewAction::ToggleTailnetGroup, !self.tailnet.collapsed)),
                content_x,
                y,
                content_w,
                status_w,
                viewport,
            )?;
            self.paint_tailnet_status(
                ctx, layers, font, palette, tailnet, content_x, y, content_w, viewport,
            )?;
            y += header_h + title_gap;
            if !self.tailnet.collapsed {
                match body {
                    TailnetBody::Machines => {
                        for slot in 0..machines {
                            let card = card_rect(
                                slot,
                                machines,
                                tailnet_grid,
                                content_x,
                                content_w,
                                y,
                                gap,
                                RowAlign::Justify,
                            );
                            if card_is_warm(card, viewport, 0.0) {
                                self.paint_tailnet_card(
                                    ctx, layers, font, palette, slot, card, viewport,
                                )?;
                            }
                        }
                    }
                    TailnetBody::Notice(notice) => {
                        let card = rect(content_x, y, content_w, tailnet_h);
                        self.paint_tailnet_notice(
                            ctx, layers, font, palette, notice, card, viewport,
                        )?;
                    }
                    TailnetBody::Pending => {}
                }
                y += tailnet_h;
            }
        }

        if self.system_host_count > 0 {
            if !first {
                y += ctx.px(GROUP_GAP);
            }
            let mut args = FluentArgs::new();
            args.set("count", self.system_host_count);
            let label = crate::i18n::tr_args("ssh-system-hosts", &args);
            self.paint_group_header(
                section_ctx,
                layers,
                section_font,
                palette,
                &label,
                Some((
                    SshViewAction::ToggleSystemHosts,
                    self.system_group_expanded(),
                )),
                content_x,
                y,
                content_w,
                0.0,
                viewport,
            )?;
            y += header_h + title_gap;
            self.paint_cards(
                ctx,
                (caption_ctx, caption_font),
                layers,
                font,
                palette,
                &system,
                system_grid,
                content_x,
                content_w,
                y,
                gap,
                viewport,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_cards(
        &mut self,
        ctx: &DrawContext,
        caption: (&DrawContext, &Rc<LoadedFont>),
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        palette: UiPalette,
        indices: &[usize],
        grid: CardGrid,
        content_x: f32,
        content_w: f32,
        cards_y: f32,
        gap: f32,
        viewport: RectF,
    ) -> anyhow::Result<()> {
        for (slot, &index) in indices.iter().enumerate() {
            let card = card_rect(
                slot,
                indices.len(),
                grid,
                content_x,
                content_w,
                cards_y,
                gap,
                // A list, not a gallery: rows start at the left margin, and
                // a full row spends whatever the card width cap left over on
                // its gaps so it still reaches the right one.
                RowAlign::Justify,
            );
            if !card_is_warm(card, viewport, 0.0) {
                continue;
            }
            self.paint_host_card(ctx, caption, layers, font, palette, index, card, viewport)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_host_card(
        &mut self,
        ctx: &DrawContext,
        (caption_ctx, caption_font): (&DrawContext, &Rc<LoadedFont>),
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        palette: UiPalette,
        index: usize,
        card: RectF,
        viewport: RectF,
    ) -> anyhow::Result<()> {
        let Some(entry) = self.filtered.get(index).cloned() else {
            return Ok(());
        };
        let action = SshViewAction::Card(index);
        // Only the part inside the viewport is clickable: a card sliding
        // under the toolbar must not keep taking clicks up there.
        let Some(visible) = card.intersection(&viewport) else {
            return Ok(());
        };
        self.widgets.push(visible, WidgetKind::SidebarRow, action);
        self.card_rects.push((index, visible));

        let selected = self.selection_active && self.selected == index;
        let hovered = self.interaction.hovered == Some(action);
        let system = entry.source == SshHostSource::System;
        let connect = !system && HostMode::of(entry.spec.use_mosh, entry.spec.multiplexing)
            == HostMode::Connect;
        let (mux_fill, mux_edge, mux_ink) = mux_colors(palette);
        let radius = ctx.px(CARD_RADIUS);
        // A Connect host is drawn as the diagram's mux server, a host
        // reached over plain ssh or Mosh as a plain card. Selection is a
        // ring, not a wash: the card already carries a logo and two
        // lines of text, and lightening it as well says the same thing
        // twice.
        if connect {
            let fill = if hovered {
                crate::ui::color::mix(mux_fill, mux_ink, 0.08)
            } else {
                mux_fill
            };
            ctx.draw_rounded_frame(
                layers,
                0,
                card.origin.x,
                card.origin.y,
                card.size.width,
                card.size.height,
                fill,
                if selected { palette.accent } else { mux_edge },
                radius,
            )?;
        } else {
            let bg = if hovered {
                palette.sidebar_row_hover_bg
            } else {
                palette.card_bg
            };
            ctx.draw_rounded_frame(
                layers,
                0,
                card.origin.x,
                card.origin.y,
                card.size.width,
                card.size.height,
                bg,
                if selected {
                    palette.accent
                } else {
                    palette.separator
                },
                radius,
            )?;
        }
        if selected {
            // A second ring just inside the first: one device pixel of accent
            // disappears on a retina display.
            ctx.draw_rounded_frame(
                layers,
                0,
                card.origin.x + 1.0,
                card.origin.y + 1.0,
                card.size.width - 2.0,
                card.size.height - 2.0,
                LinearRgba::TRANSPARENT,
                palette.accent,
                radius - 1.0,
            )?;
        }

        let pad = ctx.px(CARD_PAD);
        let line_h = ctx.metrics.cell_size.height as f32;
        // The logo is how you pick a machine out of the grid without
        // reading, so it gets real estate: roughly half the card's height.
        let icon = (line_h * 2.2).clamp(ctx.px(50.0), ctx.px(64.0));
        let icon_x = card.origin.x + pad;
        let icon_y = card.origin.y + (card.size.height - icon) / 2.0;
        match entry
            .spec
            .detected_distro
            .as_deref()
            .and_then(distro_to_icon)
        {
            Some(brand) => ctx.draw_brand_icon(layers, brand, icon_x, icon_y, icon)?,
            None if connect => {
                ctx.draw_svg_icon(layers, SvgIcon::MuxServer, icon_x, icon_y, icon, mux_ink)?
            }
            None => ctx.draw_svg_icon(
                layers,
                SvgIcon::Server,
                icon_x,
                icon_y,
                icon,
                palette.muted_text,
            )?,
        }

        let text_x = icon_x + icon + ctx.px(14.0);
        let text_w = (card.max_x() - pad - text_x).max(0.0);
        let line_gap = ctx.px(4.0);
        let stack = line_h * 2.0 + line_gap;
        let title_y = card.origin.y + ((card.size.height - stack) / 2.0).max(ctx.px(8.0));

        // A host that keeps nothing offers the way to the kind that does,
        // as a pill level with its name: the name rarely fills its line.
        let mut title_w = text_w;
        if !connect {
            let (label, link_action) = if system {
                ("ssh-card-system-add", SshViewAction::AddWithConnect(index))
            } else {
                ("ssh-card-switch", SshViewAction::SwitchToConnect(index))
            };
            // A tag beside the name, a step quieter than it.
            let label = crate::i18n::tr(label);
            let pill_pad = ctx.px(12.0);
            let pill_w = caption_ctx.measure_text_width(caption_font, &label) + pill_pad * 2.0;
            let caption_h = caption_ctx.metrics.cell_size.height as f32;
            let pill_h = caption_h + ctx.px(6.0);
            let pill_x = card.max_x() - pad - pill_w;
            let pill_y = title_y + (line_h - pill_h) / 2.0;
            if pill_x - text_x >= ctx.px(120.0) {
                let pill_hovered = self.interaction.hovered == Some(link_action);
                ctx.draw_rounded_frame(
                    layers,
                    1,
                    pill_x,
                    pill_y,
                    pill_w,
                    pill_h,
                    mux_ink.mul_alpha(if pill_hovered { 0.22 } else { 0.12 }),
                    mux_edge,
                    pill_h / 2.0,
                )?;
                caption_ctx.draw_text(
                    layers,
                    caption_font,
                    pill_x + pill_pad,
                    pill_y + ctx.px(3.0),
                    &label,
                    mux_ink,
                    pill_w,
                )?;
                // Pushed after the card, so it is the target on top.
                if let Some(target) = rect(pill_x, pill_y, pill_w, pill_h).intersection(&viewport) {
                    self.widgets.push(target, WidgetKind::Button, link_action);
                }
                title_w = (pill_x - ctx.px(12.0) - text_x).max(0.0);
            }
        }
        ctx.draw_text(
            layers,
            font,
            text_x,
            title_y,
            &entry.spec.label,
            palette.text,
            title_w,
        )?;
        ctx.draw_text(
            layers,
            font,
            text_x,
            title_y + line_h + line_gap,
            &Self::card_subtitle(&entry),
            palette.muted_text,
            text_w,
        )?;
        Ok(())
    }

    /// The card that heads the page: what ThinkTerm Connect is, beside a
    /// picture of it -- the README diagram's mux server, still running while
    /// the computer that reached it is away.
    #[allow(clippy::too_many_arguments)]
    fn paint_connect_tip(
        &mut self,
        ctx: &DrawContext,
        section_ctx: &DrawContext,
        caption_ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        section_font: &Rc<LoadedFont>,
        caption_font: &Rc<LoadedFont>,
        palette: UiPalette,
        tip: &ConnectTipLayout,
        card: RectF,
        viewport: RectF,
    ) -> anyhow::Result<()> {
        if !row_visible(card.origin.y, card.size.height, viewport) {
            return Ok(());
        }
        let widgets_start = self.widgets.len();
        let (mux_fill, mux_edge, mux_ink) = mux_colors(palette);
        ctx.draw_rounded_frame(
            layers,
            0,
            card.origin.x,
            card.origin.y,
            card.size.width,
            card.size.height,
            mux_fill,
            mux_edge,
            ctx.px(CARD_RADIUS),
        )?;
        let pad = ctx.px(CARD_PAD);
        let section_h = section_ctx.metrics.cell_size.height as f32;
        let body_h = caption_ctx.metrics.cell_size.height as f32;
        Self::paint_card_mesh(ctx, layers, mux_ink, card, 0.3, 0.7)?;

        let pad_x = ctx.px(TIP_PAD_X);
        let text_x = card.origin.x + pad_x;
        let words_top = match tip.picture {
            TipPicture::Beside { .. } => card.origin.y + (card.size.height - tip.words_h) / 2.0,
            TipPicture::Below { .. } | TipPicture::None => card.origin.y + pad,
        };
        match tip.picture {
            TipPicture::Beside { width, computer } => self.paint_connect_picture(
                ctx,
                layers,
                font,
                palette,
                card.max_x() - pad_x - width,
                card.origin.y,
                width,
                card.size.height,
                computer,
            )?,
            TipPicture::Below { width, computer } => self.paint_connect_picture(
                ctx,
                layers,
                font,
                palette,
                text_x,
                words_top + tip.words_h + ctx.px(TIP_BUTTON_GAP),
                width,
                tip.picture_h,
                computer,
            )?,
            TipPicture::None => {}
        }

        // The words, then what to do about them.
        let mut ty = words_top;
        for line in &tip.title {
            section_ctx.draw_text(
                layers,
                section_font,
                text_x,
                ty,
                line,
                palette.text,
                tip.text_w,
            )?;
            ty += section_h;
        }
        ty += ctx.px(TIP_TITLE_GAP);
        for line in &tip.body {
            caption_ctx.draw_text(
                layers,
                caption_font,
                text_x,
                ty,
                line,
                palette.secondary_text,
                tip.text_w,
            )?;
            ty += body_h;
        }
        ty += ctx.px(TIP_BUTTON_GAP);
        let label = crate::i18n::tr("ssh-connect-tip-add");
        let button_w = ctx.measure_text_width(font, &label) + ctx.px(56.0);
        let state = self.button_state(SshViewAction::TipNew);
        draw_button(
            ctx,
            layers,
            font,
            &mut self.widgets,
            palette,
            ButtonSpec {
                label: &label,
                action: SshViewAction::TipNew,
                rect: rect(text_x, ty, button_w, ctx.px(BTN_H)),
                state,
                kind: WidgetKind::Button,
                variant: ButtonVariant::Primary,
            },
        )?;
        self.widgets.clip_since(widgets_start, viewport);
        Ok(())
    }

    /// The picture on the Connect card, drawn as the README diagram draws
    /// it: this computer away (its line dashed), and the host's mux server
    /// with the sessions it keeps running meanwhile. Without `computer`,
    /// the host alone fills `w`.
    #[allow(clippy::too_many_arguments)]
    fn paint_connect_picture(
        &self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        palette: UiPalette,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        computer: bool,
    ) -> anyhow::Result<()> {
        let (mux_fill, mux_edge, mux_ink) = mux_colors(palette);
        let line_h = ctx.metrics.cell_size.height as f32;
        let green = good_green(palette);

        // The host: a platter holding the mux server and two sessions.
        let platter_w = if computer {
            ctx.px(TIP_HOST_W).min(w * 0.62)
        } else {
            w
        };
        let platter_h = line_h * 4.0 + ctx.px(92.0);
        let platter_x = x + w - platter_w;
        let platter_y = y + (h - platter_h) / 2.0;
        ctx.draw_rounded_frame(
            layers,
            1,
            platter_x,
            platter_y,
            platter_w,
            platter_h,
            palette.card_bg,
            palette.separator,
            ctx.px(36.0),
        )?;
        let inset = ctx.px(16.0);
        let block_h = line_h * 2.0 + ctx.px(24.0);
        let block_x = platter_x + inset;
        let block_w = platter_w - inset * 2.0;
        let block_y = platter_y + inset;
        ctx.draw_rounded_frame(
            layers,
            1,
            block_x,
            block_y,
            block_w,
            block_h,
            mux_fill,
            mux_edge,
            ctx.px(26.0),
        )?;
        let glyph = ctx.px(40.0);
        ctx.draw_svg_icon(
            layers,
            SvgIcon::MuxServer,
            block_x + ctx.px(20.0),
            block_y + (block_h - glyph) / 2.0,
            glyph,
            mux_ink,
        )?;
        let words_x = block_x + ctx.px(20.0) + glyph + ctx.px(20.0);
        let words_w = (block_x + block_w - words_x - ctx.px(16.0)).max(0.0);
        ctx.draw_text(
            layers,
            font,
            words_x,
            block_y + ctx.px(12.0),
            &crate::i18n::tr("ssh-connect-tip-mux"),
            palette.text,
            words_w,
        )?;
        ctx.draw_text(
            layers,
            font,
            words_x,
            block_y + ctx.px(12.0) + line_h,
            &crate::i18n::tr("ssh-connect-tip-sessions"),
            palette.muted_text,
            words_w,
        )?;
        let mut ry = block_y + block_h + ctx.px(16.0);
        for (command, state) in [
            ("cargo build", "ssh-connect-tip-running"),
            ("claude", "ssh-connect-tip-working"),
        ] {
            let dot = ctx.px(16.0);
            ctx.draw_disc(
                layers,
                1,
                block_x + ctx.px(16.0) + dot / 2.0,
                ry + line_h / 2.0,
                dot,
                green,
            )?;
            let state = crate::i18n::tr(state);
            let state_w = ctx.measure_text_width(font, &state);
            let state_x = block_x + block_w - ctx.px(12.0) - state_w;
            let cmd_x = block_x + ctx.px(16.0) + dot + ctx.px(20.0);
            ctx.draw_text(
                layers,
                font,
                cmd_x,
                ry,
                command,
                palette.text,
                (state_x - cmd_x - ctx.px(16.0)).max(0.0),
            )?;
            ctx.draw_text(
                layers,
                font,
                state_x,
                ry,
                &state,
                palette.muted_text,
                state_w + ctx.px(4.0),
            )?;
            ry += line_h + ctx.px(12.0);
        }

        if !computer {
            return Ok(());
        }

        // This computer, away: its tile, its name and "offline".
        let tile = ctx.px(96.0);
        let column_w = ctx.px(220.0);
        let tile_x = x + (column_w - tile) / 2.0;
        let column_h = tile + ctx.px(16.0) + line_h * 2.0 + ctx.px(12.0);
        let tile_y = y + (h - column_h) / 2.0;
        ctx.draw_rounded_frame(
            layers,
            1,
            tile_x,
            tile_y,
            tile,
            tile,
            palette.control_bg,
            palette.control_border,
            ctx.px(26.0),
        )?;
        let laptop = ctx.px(48.0);
        ctx.draw_svg_icon(
            layers,
            SvgIcon::Laptop,
            tile_x + (tile - laptop) / 2.0,
            tile_y + (tile - laptop) / 2.0,
            laptop,
            palette.text,
        )?;
        let name = crate::i18n::tr("ssh-connect-tip-this-computer");
        let name_w = ctx.measure_text_width(font, &name).min(column_w);
        let name_y = tile_y + tile + ctx.px(16.0);
        ctx.draw_text(
            layers,
            font,
            x + (column_w - name_w) / 2.0,
            name_y,
            &name,
            palette.text,
            column_w,
        )?;
        let away = crate::i18n::tr("ssh-connect-tip-offline");
        let away_w = ctx.measure_text_width(font, &away);
        let pill_pad = ctx.px(16.0);
        let pill_w = away_w + pill_pad * 2.0;
        let pill_x = x + (column_w - pill_w) / 2.0;
        let pill_y = name_y + line_h + ctx.px(12.0);
        ctx.draw_rounded_frame(
            layers,
            1,
            pill_x,
            pill_y,
            pill_w,
            line_h,
            palette.control_bg,
            palette.control_border,
            line_h / 2.0,
        )?;
        ctx.draw_text(
            layers,
            font,
            pill_x + pill_pad,
            pill_y,
            &away,
            palette.muted_text,
            away_w + ctx.px(4.0),
        )?;

        // The line between them, dashed: nothing is connected.
        let line_y = tile_y + tile / 2.0;
        let mut dx = x + column_w + ctx.px(12.0);
        let end = platter_x - ctx.px(20.0);
        let dash = ctx.px(16.0);
        while dx < end {
            ctx.draw_rect(
                layers,
                1,
                dx,
                line_y - ctx.px(2.0),
                dash.min(end - dx),
                ctx.px(4.0),
                palette.muted_text.mul_alpha(0.7),
            )?;
            dx += dash + ctx.px(12.0);
        }
        Ok(())
    }

    /// A group's caption. One that folds carries its action and whether it
    /// is open; `trailing` is kept clear at the right end for what the
    /// caller draws there.
    #[allow(clippy::too_many_arguments)]
    fn paint_group_header(
        &mut self,
        section_ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        section_font: &Rc<LoadedFont>,
        palette: UiPalette,
        label: &str,
        toggle: Option<(SshViewAction, bool)>,
        x: f32,
        y: f32,
        width: f32,
        trailing: f32,
        viewport: RectF,
    ) -> anyhow::Result<()> {
        let height = section_ctx.px(GROUP_HEADER_H);
        if !row_visible(y, height, viewport) {
            return Ok(());
        }
        let line_h = section_ctx.metrics.cell_size.height as f32;
        let mut text_x = x;
        if let Some((action, expanded)) = toggle {
            let hit_h = (y + height).min(viewport.max_y()) - y.max(viewport.min_y());
            self.widgets.push(
                rect(x, y.max(viewport.min_y()), width, hit_h.max(0.0)),
                WidgetKind::Button,
                action,
            );
            let chevron = section_ctx.px(18.0);
            let icon = if expanded {
                SvgIcon::ChevronDown
            } else {
                SvgIcon::ChevronRight
            };
            section_ctx.draw_svg_icon(
                layers,
                icon,
                x,
                y + (height - chevron) / 2.0,
                chevron,
                palette.muted_text,
            )?;
            text_x += chevron + section_ctx.px(6.0);
        }
        section_ctx.draw_text(
            layers,
            section_font,
            text_x,
            y + ((height - line_h) / 2.0).max(0.0),
            label,
            palette.secondary_text,
            (width - trailing - (text_x - x)).max(0.0),
        )?;
        Ok(())
    }

    fn paint_empty_state(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        palette: UiPalette,
        viewport: RectF,
    ) -> anyhow::Result<()> {
        if viewport.size.width <= 0.0 || viewport.size.height <= 0.0 {
            return Ok(());
        }
        let icon = ctx.px(56.0);
        let center_x = viewport.origin.x + viewport.size.width / 2.0;
        let center_y = viewport.origin.y + viewport.size.height / 2.0;
        ctx.draw_svg_icon(
            layers,
            SvgIcon::Server,
            center_x - icon / 2.0,
            center_y - icon - ctx.px(10.0),
            icon,
            palette.muted_text,
        )?;
        let message = crate::i18n::tr("ssh-no-hosts");
        let width = ctx.measure_text_width(font, &message);
        ctx.draw_text(
            layers,
            font,
            center_x - width / 2.0,
            center_y + ctx.px(8.0),
            &message,
            palette.muted_text,
            viewport.size.width,
        )?;
        Ok(())
    }

    fn paint_list_fades(
        &self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        palette: UiPalette,
        scroll: ScrollState,
    ) -> anyhow::Result<()> {
        if !scroll.has_overflow() || area.size.width <= 0.0 || area.size.height <= 0.0 {
            return Ok(());
        }

        let fade_height = ctx.px(LIST_FADE_HEIGHT).min(area.size.height).ceil() as usize;
        if fade_height == 0 {
            return Ok(());
        }

        if scroll.offset > 0.5 {
            for step in 0..fade_height {
                let progress = step as f32 / fade_height as f32;
                let alpha = 1.0 - Easing::Smooth.apply(progress);
                ctx.draw_rect(
                    layers,
                    2,
                    area.origin.x,
                    area.origin.y + step as f32,
                    area.size.width,
                    1.0,
                    palette.window_bg.mul_alpha(alpha),
                )?;
            }
        }

        if scroll.offset < scroll.max_offset() - 0.5 {
            let start_y = area.origin.y + area.size.height - fade_height as f32;
            for step in 0..fade_height {
                let progress = (step + 1) as f32 / fade_height as f32;
                let alpha = Easing::Smooth.apply(progress);
                ctx.draw_rect(
                    layers,
                    2,
                    area.origin.x,
                    start_y + step as f32,
                    area.size.width,
                    1.0,
                    palette.window_bg.mul_alpha(alpha),
                )?;
            }
        }

        Ok(())
    }

    fn paint_list_mask(
        &self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        palette: UiPalette,
    ) -> anyhow::Result<()> {
        if width <= 0.0 || height <= 0.0 {
            return Ok(());
        }
        for layer in 0..=2 {
            ctx.draw_rect(layers, layer, x, y, width, height, palette.window_bg)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]

    /// Height of one label-over-input field row.
    fn field_row_height(ctx: &DrawContext) -> f32 {
        ctx.metrics.cell_size.height as f32 + ctx.px(LABEL_GAP) + ctx.px(INPUT_H)
    }

    fn toggle_row_height(ctx: &DrawContext) -> f32 {
        ctx.px(40.0)
            .max(ctx.metrics.cell_size.height as f32 + ctx.px(10.0))
    }

    /// A card tall enough for `rows`, including its caption and padding.
    fn inspector_card_height(ctx: &DrawContext, section_h: f32, rows: &[f32]) -> f32 {
        let gap = ctx.px(FIELD_GAP);
        ctx.px(20.0) * 2.0
            + section_h
            + ctx.px(12.0)
            + rows.iter().sum::<f32>()
            + gap * rows.len().saturating_sub(1) as f32
    }

    /// Draw a card and its caption; returns the y of its first row.
    #[allow(clippy::too_many_arguments)]
    fn paint_inspector_card(
        ctx: &DrawContext,
        section_ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        section_font: &Rc<LoadedFont>,
        palette: UiPalette,
        tokens: UiTokens,
        caption: &str,
        area: RectF,
    ) -> anyhow::Result<f32> {
        ctx.draw_card(layers, 0, area, palette, tokens)?;
        let pad = ctx.px(20.0);
        let section_h = section_ctx.metrics.cell_size.height as f32;
        section_ctx.draw_text(
            layers,
            section_font,
            area.origin.x + pad,
            area.origin.y + pad,
            caption,
            palette.secondary_text,
            (area.size.width - pad * 2.0).max(0.0),
        )?;
        Ok(area.origin.y + pad + section_h + ctx.px(12.0))
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_field(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        palette: UiPalette,
        tokens: UiTokens,
        cursor_on: bool,
        label: &str,
        index: usize,
        shown: &str,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<f32> {
        self.paint_field_hinted(
            ctx, layers, font, palette, tokens, cursor_on, label, index, shown, "", x, y, width,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_field_hinted(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        palette: UiPalette,
        tokens: UiTokens,
        cursor_on: bool,
        label: &str,
        index: usize,
        shown: &str,
        placeholder: &str,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<f32> {
        self.paint_input_row(
            ctx,
            layers,
            font,
            palette,
            tokens,
            cursor_on,
            label,
            Focus::Field(index),
            SshViewAction::FocusField(index),
            shown,
            placeholder,
            x,
            y,
            width,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_toggle_row(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        palette: UiPalette,
        label: &str,
        on: bool,
        action: SshViewAction,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<f32> {
        let height = Self::toggle_row_height(ctx);
        let line_h = ctx.metrics.cell_size.height as f32;
        let toggle_w = ctx.px(64.0);
        let toggle_h = ctx.px(36.0);
        ctx.draw_text(
            layers,
            font,
            x,
            y + ((height - line_h) / 2.0).max(0.0),
            label,
            palette.text,
            (width - toggle_w - ctx.px(12.0)).max(0.0),
        )?;
        draw_toggle(
            ctx,
            layers,
            &mut self.widgets,
            &self.interaction,
            palette,
            rect(
                x + width - toggle_w,
                y + (height - toggle_h) / 2.0,
                toggle_w,
                toggle_h,
            ),
            on,
            action,
        )?;
        Ok(y + height)
    }

    /// The "Connect with" card: each way to reach the host, with what it
    /// keeps and what it costs, the chosen one ringed. The words wrap rather
    /// than run out: the form's column is narrow. Returns its height.
    #[allow(clippy::too_many_arguments)]
    fn paint_mode_card(
        &mut self,
        ctx: &DrawContext,
        section_ctx: &DrawContext,
        caption_ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        section_font: &Rc<LoadedFont>,
        caption_font: &Rc<LoadedFont>,
        palette: UiPalette,
        tokens: UiTokens,
        chosen: HostMode,
        area: RectF,
    ) -> anyhow::Result<f32> {
        let line_h = ctx.metrics.cell_size.height as f32;
        // What each way does is read after its name, a step down from it.
        let fact_h = caption_ctx.metrics.cell_size.height as f32;
        let section_h = section_ctx.metrics.cell_size.height as f32;
        let pad = ctx.px(20.0);
        let inner = (area.size.width - pad * 2.0).max(0.0);
        let row_pad = ctx.px(22.0);
        let line_gap = ctx.px(6.0);
        let tile = ctx.px(60.0);
        let glyph = ctx.px(34.0);
        let fact_icon = ctx.px(22.0);
        let fact_gap = ctx.px(10.0);
        let text_w = (inner - row_pad * 2.0 - tile - ctx.px(22.0)).max(ctx.px(40.0));
        let fact_w = (text_w - fact_icon - fact_gap).max(ctx.px(40.0));
        let wrap = |text: &str, width: f32| -> Vec<String> {
            wrap_snippet_text_for_width(text, 3, false, |segment| {
                caption_ctx.measure_text_width(caption_font, segment) / width
            })
        };
        let modes = [
            (
                HostMode::Connect,
                "ssh-mode-connect-title",
                [("ssh-mode-connect-keeps", true), ("ssh-mode-connect-server", true)],
                Some("ssh-mode-connect-install"),
            ),
            (
                HostMode::Ssh,
                "ssh-mode-ssh-title",
                [("ssh-mode-ssh-ends", false), ("ssh-mode-ssh-nothing", true)],
                None,
            ),
            (
                HostMode::Mosh,
                "ssh-mode-mosh-title",
                [("ssh-mode-mosh-roams", true), ("ssh-mode-mosh-needs", false)],
                None,
            ),
        ];
        let laid: Vec<ModeWords> = modes
            .iter()
            .map(|(_, _, facts, note)| ModeWords {
                facts: facts
                    .iter()
                    .map(|(fact, good)| (wrap(&crate::i18n::tr(fact), fact_w), *good))
                    .collect(),
                note: note
                    .map(|note| wrap(&crate::i18n::tr(note), text_w))
                    .unwrap_or_default(),
            })
            .collect();
        let rows: Vec<f32> = laid
            .iter()
            .map(|ModeWords { facts, note }| {
                let lines = facts.iter().map(|(lines, _)| lines.len()).sum::<usize>()
                    + note.len();
                let words = line_h + (fact_h + line_gap) * lines as f32;
                row_pad * 2.0 + words.max(tile)
            })
            .collect();
        let height = Self::inspector_card_height(ctx, section_h, &rows);
        let mut y = Self::paint_inspector_card(
            ctx,
            section_ctx,
            layers,
            section_font,
            palette,
            tokens,
            &crate::i18n::tr("ssh-group-connect-with"),
            rect(area.origin.x, area.origin.y, area.size.width, height),
        )?;

        let green = good_green(palette);
        let (mux_fill, mux_edge, mux_ink) = mux_colors(palette);
        let x = area.origin.x + pad;
        for (((mode, title, _, _), ModeWords { facts, note }), row_h) in
            modes.iter().zip(laid.iter()).zip(rows.iter().copied())
        {
            let on = *mode == chosen;
            let radius = ctx.px(24.0);
            // The chosen one is ringed and tinted; the rest sit quiet.
            if on {
                let ring = ctx.px(4.0);
                ctx.draw_rounded_rect(layers, 0, x, y, inner, row_h, palette.accent, radius)?;
                for fill in [palette.control_bg, palette.accent.mul_alpha(0.08)] {
                    ctx.draw_rounded_rect(
                        layers,
                        0,
                        x + ring,
                        y + ring,
                        inner - ring * 2.0,
                        row_h - ring * 2.0,
                        fill,
                        radius - ring,
                    )?;
                }
            } else {
                ctx.draw_rounded_frame(
                    layers,
                    0,
                    x,
                    y,
                    inner,
                    row_h,
                    palette.control_bg,
                    palette.control_border,
                    radius,
                )?;
            }
            self.widgets.push(
                rect(x, y, inner, row_h),
                WidgetKind::Button,
                SshViewAction::SetMode(*mode),
            );

            // The host as the diagram draws it: a mux server for Connect, a
            // plain machine for the rest.
            let tile_x = x + row_pad;
            let tile_y = y + row_pad;
            let (fill, edge, ink, icon) = if *mode == HostMode::Connect {
                (mux_fill, mux_edge, mux_ink, SvgIcon::MuxServer)
            } else {
                (
                    palette.control_bg,
                    palette.control_border,
                    palette.secondary_text,
                    SvgIcon::Server,
                )
            };
            ctx.draw_rounded_frame(layers, 1, tile_x, tile_y, tile, tile, fill, edge, ctx.px(16.0))?;
            ctx.draw_svg_icon(
                layers,
                icon,
                tile_x + (tile - glyph) / 2.0,
                tile_y + (tile - glyph) / 2.0,
                glyph,
                ink,
            )?;

            let text_x = tile_x + tile + ctx.px(22.0);
            let mut ty = y + row_pad;
            let title = crate::i18n::tr(title);
            ctx.draw_text(layers, font, text_x, ty, &title, palette.text, text_w)?;
            if *mode == HostMode::Connect {
                // A tag, a step quieter than the name it follows.
                let label = crate::i18n::tr("ssh-mode-recommended");
                let title_w = ctx.measure_text_width(font, &title);
                let pill_x = text_x + title_w + ctx.px(12.0);
                let pill_pad = ctx.px(10.0);
                let pill_w = caption_ctx.measure_text_width(caption_font, &label) + pill_pad * 2.0;
                let pill_h = fact_h + ctx.px(2.0);
                let pill_y = ty + (line_h - pill_h) / 2.0;
                if pill_x + pill_w <= text_x + text_w {
                    ctx.draw_rounded_frame(
                        layers,
                        1,
                        pill_x,
                        pill_y,
                        pill_w,
                        pill_h,
                        mux_ink.mul_alpha(0.12),
                        mux_edge,
                        pill_h / 2.0,
                    )?;
                    caption_ctx.draw_text(
                        layers,
                        caption_font,
                        pill_x + pill_pad,
                        pill_y + ctx.px(1.0),
                        &label,
                        mux_ink,
                        pill_w,
                    )?;
                }
            }
            ty += line_h - fact_h;
            for (lines, good) in facts {
                ty += fact_h + line_gap;
                let (icon, color) = if *good {
                    (SvgIcon::Check, green)
                } else {
                    (SvgIcon::X, palette.danger)
                };
                ctx.draw_svg_icon(
                    layers,
                    icon,
                    text_x,
                    ty + (fact_h - fact_icon) / 2.0,
                    fact_icon,
                    color,
                )?;
                for (n, line) in lines.iter().enumerate() {
                    if n > 0 {
                        ty += fact_h + line_gap;
                    }
                    caption_ctx.draw_text(
                        layers,
                        caption_font,
                        text_x + fact_icon + fact_gap,
                        ty,
                        line,
                        palette.secondary_text,
                        fact_w,
                    )?;
                }
            }
            for line in note {
                ty += fact_h + line_gap;
                caption_ctx.draw_text(
                    layers,
                    caption_font,
                    text_x,
                    ty,
                    line,
                    palette.muted_text,
                    text_w,
                )?;
            }
            y += row_h + ctx.px(FIELD_GAP);
        }
        Ok(height)
    }

    /// The lines under the shell integration toggle: where it works, then
    /// what the next connection will do about it, what the last one found,
    /// or why it failed.
    fn shell_integration_note(
        ctx: &DrawContext,
        font: &Rc<LoadedFont>,
        editing: Option<&str>,
        on: bool,
        width: f32,
    ) -> Vec<String> {
        if width <= 0.0 {
            return Vec::new();
        }
        let record = editing
            .map(ssh_hosts::shell_integration_record)
            .unwrap_or_default();
        [
            crate::i18n::tr("ssh-shell-integration-note"),
            Self::shell_integration_status(on, record),
        ]
        .iter()
        .flat_map(|text| {
            wrap_snippet_text_for_width(text, 5, false, |segment| {
                ctx.measure_text_width(font, segment) / width
            })
        })
        .collect()
    }

    /// The line under the note: what the next connection will do about the
    /// setting, what the last one found, or why it failed.
    fn shell_integration_status(on: bool, record: ssh_hosts::ShellIntegrationRecord) -> String {
        // A failure counts only while the change still waits on a
        // connection: turned back, nothing is left to fail.
        if on != record.applied {
            if let Some(failure) = &record.failure {
                return Self::shell_integration_failure_text(failure, on);
            }
        }
        match (on, record.applied, record.unsupported) {
            (true, true, Some(shell)) => {
                let mut args = FluentArgs::new();
                args.set("shell", shell);
                crate::i18n::tr_args("ssh-shell-integration-unsupported", &args)
            }
            (true, true, None) => crate::i18n::tr("ssh-shell-integration-installed"),
            (false, true, None) => crate::i18n::tr("ssh-shell-integration-remove"),
            _ => crate::i18n::tr("ssh-shell-integration-install"),
        }
    }

    /// Why the last connection could not carry the setting out, and that the
    /// next one tries again.
    fn shell_integration_failure_text(
        failure: &ShellIntegrationFailure,
        installing: bool,
    ) -> String {
        let reason = match failure {
            ShellIntegrationFailure::Unwritable(path) => {
                let mut args = FluentArgs::new();
                args.set("path", path.clone());
                crate::i18n::tr_args("ssh-shell-integration-unwritable", &args)
            }
            ShellIntegrationFailure::Other(message) => message.clone(),
        };
        let mut args = FluentArgs::new();
        args.set("reason", reason);
        let id = if installing {
            "ssh-shell-integration-failed-install"
        } else {
            "ssh-shell-integration-failed-remove"
        };
        crate::i18n::tr_args(id, &args)
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_inspector(
        &mut self,
        ctx: &DrawContext,
        title_ctx: &DrawContext,
        section_ctx: &DrawContext,
        caption_ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        title_font: &Rc<LoadedFont>,
        section_font: &Rc<LoadedFont>,
        caption_font: &Rc<LoadedFont>,
        palette: UiPalette,
        tokens: UiTokens,
        cursor_on: bool,
        area: RectF,
    ) -> anyhow::Result<()> {
        let Some(form) = self.form.as_ref() else {
            return Ok(());
        };
        let editing = form.editing.clone();
        let icon = form.icon;
        let network = form.network;
        let detect_os = form.detect_os;
        let use_mosh = form.use_mosh;
        let multiplexing = form.multiplexing;
        let shell_integration = form.shell_integration;
        let files_auto_connect = form.files_auto_connect;
        let advanced_open = form.advanced_open;
        let error = form.error.clone();
        let fields = form.fields.clone();
        let options = form.options.clone();
        let password_visible = self.password_visible;

        self.form_area = area;

        let x = area.origin.x;
        let w = area.size.width;
        let pad = ctx.px(20.0);
        let inner = (w - pad * 2.0).max(0.0);
        let line_h = ctx.metrics.cell_size.height as f32;
        let section_h = section_ctx.metrics.cell_size.height as f32;
        let title_h = title_ctx.metrics.cell_size.height as f32;
        let field_h = Self::field_row_height(ctx);
        let toggle_h = Self::toggle_row_height(ctx);
        let gap = ctx.px(FIELD_GAP);
        let card_gap = ctx.px(INSPECTOR_CARD_GAP);

        // The header does not scroll. It names the machine you are editing
        // and carries the one action you came for, and both stop meaning
        // anything the moment they slide off the top of the panel -- which is
        // exactly what they used to do, taking half the Connect button with
        // them.
        let name = fields[FIELD_NAME].text().to_string();
        let heading = if !name.trim().is_empty() {
            name
        } else if editing.is_some() {
            crate::i18n::tr("ssh-edit-host")
        } else {
            crate::i18n::tr("ssh-new-host")
        };
        let host = fields[FIELD_HOST].text().to_string();
        let endpoint = (!host.trim().is_empty()).then(|| {
            let user = fields[FIELD_USER].text().to_string();
            let port = fields[FIELD_PORT].text().to_string();
            let mut endpoint = host.trim().to_string();
            if !port.trim().is_empty() && port.trim() != "22" {
                endpoint = format!("{endpoint}:{}", port.trim());
            }
            if !user.trim().is_empty() {
                endpoint = format!("{}@{endpoint}", user.trim());
            }
            endpoint
        });
        // Connect is only meaningful once the host exists on disk; before
        // that, "Save & Open" at the bottom is the way through.
        let connect = editing.is_some();
        let mut header_h = title_h + ctx.px(4.0);
        if endpoint.is_some() {
            header_h += line_h;
        }
        header_h += ctx.px(14.0);
        if connect {
            header_h += ctx.px(BTN_H) + card_gap;
        }

        // ---- footer. Creating a host offers "Save & Open" as the primary
        // action, since there is no Connect button above yet. The buttons
        // float in a capsule at the bottom of the panel while the body
        // scrolls under it, so that Save is always in sight and an edit is
        // not left behind unsaved. The primary one comes last, at the right
        // end of a capsule centred on the panel.
        let button_gap = ctx.px(10.0);
        let mut buttons: Vec<(String, SshViewAction, ButtonVariant)> = vec![(
            crate::i18n::tr("ssh-cancel"),
            SshViewAction::Cancel,
            ButtonVariant::Secondary,
        )];
        if editing.is_none() {
            buttons.push((
                crate::i18n::tr("ssh-save"),
                SshViewAction::Save,
                ButtonVariant::Secondary,
            ));
            buttons.push((
                crate::i18n::tr("ssh-save-open"),
                SshViewAction::SaveAndConnect,
                ButtonVariant::Primary,
            ));
        } else {
            buttons.push((
                crate::i18n::tr("ssh-save"),
                SshViewAction::Save,
                ButtonVariant::Primary,
            ));
        }
        // Natural widths while they fit in one row; stacked full width when
        // they do not, rather than squeezed until the labels ellipsize.
        let natural: Vec<f32> = buttons
            .iter()
            .map(|(label, _, _)| ctx.measure_text_width(font, label) + ctx.px(56.0))
            .collect();
        let capsule_pad = ctx.px(8.0);
        let row_w = natural.iter().sum::<f32>()
            + button_gap * buttons.len().saturating_sub(1) as f32
            + capsule_pad * 2.0;
        let row_fits = row_w <= w;
        let stack_gap = ctx.px(8.0);
        let capsule_w = if row_fits { row_w } else { w };
        let capsule_h = capsule_pad * 2.0
            + if row_fits {
                ctx.px(BTN_H)
            } else {
                (ctx.px(BTN_H) + stack_gap) * buttons.len() as f32 - stack_gap
            };
        let capsule = rect(
            x + (w - capsule_w) / 2.0,
            area.max_y() - ctx.px(PAD) - capsule_h,
            capsule_w,
            capsule_h,
        );
        // The body's last row can scroll clear of it.
        let footer_room = capsule_h + ctx.px(PAD) + ctx.px(16.0);
        let footer_top = area.max_y();

        let body_top = area.origin.y + header_h;
        let body_height = (footer_top - body_top).max(0.0);
        self.form_scroll
            .set_extents(body_height, self.form_content_h);
        let top = body_top - self.form_scroll.offset;
        let mut y = top;
        // The body is recorded and replayed clipped to its own bounds. A mask
        // cannot fix this one: a card scrolled far enough up is drawn *above*
        // this view's area entirely -- over the tab bar -- and painting there
        // to hide it would erase the tab bar with it.
        let mut heap = HeapQuadAllocator::default();
        // Where the body's hit targets start, so they can be bounded to the
        // body once its height is known. Clipping the replay below bounds the
        // pixels only; a field scrolled above `body_top` would otherwise keep
        // taking clicks in the header band, where it is invisible.
        let body_widgets_start = self.widgets.len();
        {
            let mut body_layers = TripleLayerQuadAllocator::Heap(&mut heap);

            // ---- Connection
            // The built-in tailnet can be chosen once Tailscale is on; a host
            // already on it keeps showing so.
            let tailnet_locked = !self.tailnet_on && network != SshNetwork::Tailnet;
            let caption_h = caption_ctx.metrics.cell_size.height as f32;
            let lock_note = if !tailnet_locked {
                Vec::new()
            } else {
                // A build without the built-in tailnet has no Tailscale
                // group on the page to turn it on in.
                let note = if crate::tailnet::available() {
                    crate::i18n::tr("ssh-network-tailnet-off")
                } else {
                    crate::i18n::tr("ssh-network-tailnet-unavailable")
                };
                wrap_snippet_text_for_width(&note, 3, false, |segment| {
                    caption_ctx.measure_text_width(caption_font, segment) / inner.max(1.0)
                })
            };
            let lock_note_h = if lock_note.is_empty() {
                0.0
            } else {
                ctx.px(6.0) + caption_h * lock_note.len() as f32
            };
            let conn_h = Self::inspector_card_height(
                ctx,
                section_h,
                &[field_h, field_h, field_h + lock_note_h, field_h, field_h, field_h],
            );
            let mut fy = Self::paint_inspector_card(
                ctx,
                section_ctx,
                &mut body_layers,
                section_font,
                palette,
                tokens,
                &crate::i18n::tr("ssh-group-connection"),
                rect(x, y, w, conn_h),
            )?;
            fy = self.paint_field(
                ctx,
                &mut body_layers,
                font,
                palette,
                tokens,
                cursor_on,
                &crate::i18n::tr("ssh-field-name"),
                FIELD_NAME,
                &fields[FIELD_NAME].text(),
                x + pad,
                fy,
                inner,
            )? + gap;
            fy = self.paint_icon_row(
                ctx,
                &mut body_layers,
                font,
                palette,
                icon,
                x + pad,
                fy,
                inner,
            )? + gap;
            ctx.draw_text(
                &mut body_layers,
                font,
                x + pad,
                fy,
                &crate::i18n::tr("ssh-field-network"),
                palette.text,
                inner,
            )?;
            let each = (inner - ctx.px(8.0)) / 2.0;
            for (index, (choice, label)) in [
                (SshNetwork::System, "ssh-network-system"),
                (SshNetwork::Tailnet, "ssh-network-tailnet"),
            ]
            .iter()
            .copied()
            .enumerate()
            {
                let action = SshViewAction::SetNetwork(choice);
                let state = if choice == SshNetwork::Tailnet && tailnet_locked {
                    ControlState::Disabled
                } else {
                    self.button_state(action)
                };
                draw_button(
                    ctx,
                    &mut body_layers,
                    font,
                    &mut self.widgets,
                    palette,
                    ButtonSpec {
                        label: &crate::i18n::tr(label),
                        action,
                        rect: rect(
                            x + pad + index as f32 * (each + ctx.px(8.0)),
                            fy + line_h + ctx.px(LABEL_GAP),
                            each,
                            ctx.px(INPUT_H),
                        ),
                        state,
                        kind: WidgetKind::Button,
                        variant: if network == choice {
                            ButtonVariant::Primary
                        } else {
                            ButtonVariant::Secondary
                        },
                    },
                )?;
            }
            let mut note_y = fy + line_h + ctx.px(LABEL_GAP) + ctx.px(INPUT_H) + ctx.px(6.0);
            for line in &lock_note {
                caption_ctx.draw_text(
                    &mut body_layers,
                    caption_font,
                    x + pad,
                    note_y,
                    line,
                    palette.muted_text,
                    inner,
                )?;
                note_y += caption_h;
            }
            fy += field_h + lock_note_h + gap;
            // Host and Port share a row: a port needs four characters, not a
            // column of its own.
            let port_w = ctx.px(96.0).min(inner * 0.32);
            let host_w = (inner - port_w - ctx.px(10.0)).max(0.0);
            self.paint_field(
                ctx,
                &mut body_layers,
                font,
                palette,
                tokens,
                cursor_on,
                &crate::i18n::tr("ssh-field-host"),
                FIELD_HOST,
                &fields[FIELD_HOST].text(),
                x + pad,
                fy,
                host_w,
            )?;
            fy = self.paint_field(
                ctx,
                &mut body_layers,
                font,
                palette,
                tokens,
                cursor_on,
                &crate::i18n::tr("ssh-field-port"),
                FIELD_PORT,
                &fields[FIELD_PORT].text(),
                x + pad + host_w + ctx.px(10.0),
                fy,
                port_w,
            )? + gap;
            // Tried after Host, in order, when it does not answer.
            fy = self.paint_field_hinted(
                ctx,
                &mut body_layers,
                font,
                palette,
                tokens,
                cursor_on,
                &crate::i18n::tr("ssh-field-other-addresses"),
                FIELD_OTHER_ADDRESSES,
                &fields[FIELD_OTHER_ADDRESSES].text(),
                "myhost.local, 127.0.0.1",
                x + pad,
                fy,
                inner,
            )? + gap;
            self.paint_field(
                ctx,
                &mut body_layers,
                font,
                palette,
                tokens,
                cursor_on,
                &crate::i18n::tr("ssh-field-user"),
                FIELD_USER,
                &fields[FIELD_USER].text(),
                x + pad,
                fy,
                inner,
            )?;
            y += conn_h + card_gap;

            // ---- Connect with: the choice that decides what the host keeps.
            let mode_h = self.paint_mode_card(
                ctx,
                section_ctx,
                caption_ctx,
                &mut body_layers,
                font,
                section_font,
                caption_font,
                palette,
                tokens,
                HostMode::of(use_mosh, multiplexing),
                rect(x, y, w, 0.0),
            )?;
            y += mode_h + card_gap;

            // ---- Authentication
            let auth_h = Self::inspector_card_height(ctx, section_h, &[field_h, field_h]);
            let mut fy = Self::paint_inspector_card(
                ctx,
                section_ctx,
                &mut body_layers,
                section_font,
                palette,
                tokens,
                &crate::i18n::tr("ssh-group-authentication"),
                rect(x, y, w, auth_h),
            )?;
            let raw_password = fields[FIELD_PASSWORD].text().to_string();
            let masked = "•".repeat(raw_password.chars().count());
            let eye = ctx.px(INPUT_H);
            let password_w = (inner - eye - ctx.px(8.0)).max(0.0);
            self.paint_field(
                ctx,
                &mut body_layers,
                font,
                palette,
                tokens,
                cursor_on,
                &crate::i18n::tr("ssh-field-password"),
                FIELD_PASSWORD,
                if password_visible {
                    &raw_password
                } else {
                    &masked
                },
                x + pad,
                fy,
                password_w,
            )?;
            draw_icon_button(
                ctx,
                &mut body_layers,
                &mut self.widgets,
                &self.interaction,
                palette,
                x + pad + password_w + ctx.px(8.0),
                fy + line_h + ctx.px(LABEL_GAP),
                eye,
                if password_visible {
                    SvgIcon::EyeOff
                } else {
                    SvgIcon::Eye
                },
                SshViewAction::RevealPassword,
            )?;
            fy += field_h + gap;
            self.paint_field(
                ctx,
                &mut body_layers,
                font,
                palette,
                tokens,
                cursor_on,
                &crate::i18n::tr("ssh-field-identity"),
                FIELD_IDENTITY,
                &fields[FIELD_IDENTITY].text(),
                x + pad,
                fy,
                inner,
            )?;
            y += auth_h + card_gap;

            // ---- Session
            // Only plain ssh needs the shell to report what runs: Connect
            // sees the host's processes itself, and Mosh does not pass the
            // reports on.
            let integration_note = (!use_mosh && !multiplexing).then(|| {
                Self::shell_integration_note(
                    ctx,
                    font,
                    editing.as_deref(),
                    shell_integration,
                    inner,
                )
            });
            let mut session_rows = vec![field_h, toggle_h, toggle_h];
            if use_mosh {
                session_rows.push(field_h);
            }
            if let Some(lines) = &integration_note {
                // The toggle and the note under it are one row.
                session_rows.push(toggle_h + lines.len() as f32 * line_h);
            }
            let session_h = Self::inspector_card_height(ctx, section_h, &session_rows);
            let mut fy = Self::paint_inspector_card(
                ctx,
                section_ctx,
                &mut body_layers,
                section_font,
                palette,
                tokens,
                &crate::i18n::tr("ssh-group-session"),
                rect(x, y, w, session_h),
            )?;
            fy = self.paint_field(
                ctx,
                &mut body_layers,
                font,
                palette,
                tokens,
                cursor_on,
                &crate::i18n::tr("ssh-field-workspace"),
                FIELD_WORKSPACE,
                &fields[FIELD_WORKSPACE].text(),
                x + pad,
                fy,
                inner,
            )? + gap;
            fy = self.paint_toggle_row(
                ctx,
                &mut body_layers,
                font,
                palette,
                &crate::i18n::tr("ssh-detect-os"),
                detect_os,
                SshViewAction::ToggleDetect,
                x + pad,
                fy,
                inner,
            )? + gap;
            fy = self.paint_toggle_row(
                ctx,
                &mut body_layers,
                font,
                palette,
                &crate::i18n::tr("ssh-files-auto-connect"),
                files_auto_connect,
                SshViewAction::ToggleFilesAutoConnect,
                x + pad,
                fy,
                inner,
            )? + gap;
            if let Some(lines) = &integration_note {
                fy = self.paint_toggle_row(
                    ctx,
                    &mut body_layers,
                    font,
                    palette,
                    &crate::i18n::tr("ssh-shell-integration"),
                    shell_integration,
                    SshViewAction::ToggleShellIntegration,
                    x + pad,
                    fy,
                    inner,
                )?;
                for line in lines {
                    ctx.draw_text(
                        &mut body_layers,
                        font,
                        x + pad,
                        fy,
                        line,
                        palette.muted_text,
                        inner,
                    )?;
                    fy += line_h;
                }
                fy += gap;
            }
            if use_mosh {
                let mosh = fields[FIELD_MOSH_SERVER].text().to_string();
                let text_pad = ctx.px(12.0);
                self.resolve_pending_caret_click(
                    ctx,
                    font,
                    Focus::Field(FIELD_MOSH_SERVER),
                    &mosh,
                    x + pad + text_pad,
                );
                let caret = self.caret_for(Focus::Field(FIELD_MOSH_SERVER));
                let focused = self.focus == Focus::Field(FIELD_MOSH_SERVER);
                ctx.draw_text(
                    &mut body_layers,
                    font,
                    x + pad,
                    fy,
                    // The row had no name: it printed the default command as
                    // its own label and again as the placeholder.
                    &crate::i18n::tr("ssh-field-mosh-server"),
                    palette.muted_text,
                    inner,
                )?;
                draw_text_input(
                    ctx,
                    &mut body_layers,
                    font,
                    &mut self.widgets,
                    &self.interaction,
                    palette,
                    tokens,
                    text_pad,
                    cursor_on,
                    TextInputSpec {
                        placeholder: ssh_hosts::DEFAULT_MOSH_SERVER_COMMAND,
                        text: &mosh,
                        rect: rect(
                            x + pad,
                            fy + line_h + ctx.px(LABEL_GAP),
                            inner,
                            ctx.px(INPUT_H),
                        ),
                        focused,
                        selected_all: false,
                        action: SshViewAction::FocusField(FIELD_MOSH_SERVER),
                    },
                    caret,
                )?;
            }
            y += session_h + card_gap;

            // ---- Advanced: raw ssh_config overrides.
            let disclosure = if advanced_open { "▾" } else { "▸" };
            let advanced_h = line_h + ctx.px(14.0);
            self.widgets.push(
                rect(x, y, w, advanced_h),
                WidgetKind::Button,
                SshViewAction::ToggleAdvanced,
            );
            ctx.draw_text(
                &mut body_layers,
                font,
                x,
                y + ((advanced_h - line_h) / 2.0).max(0.0),
                &format!("{disclosure}  {}", crate::i18n::tr("ssh-advanced")),
                palette.secondary_text,
                w,
            )?;
            y += advanced_h + ctx.px(8.0);

            if advanced_open {
                let remove_w = ctx.px(INPUT_H);
                let row_gap = ctx.px(8.0);
                let pair_w = (w - remove_w - row_gap * 2.0).max(0.0);
                let key_w = pair_w * 0.42;
                let value_w = pair_w - key_w;
                for (index, (key, value)) in options.iter().enumerate() {
                    let key_x = x;
                    let value_x = key_x + key_w + row_gap;
                    let text_pad = ctx.px(12.0);
                    self.resolve_pending_caret_click(
                        ctx,
                        font,
                        Focus::OptionKey(index),
                        &key.text(),
                        key_x + text_pad,
                    );
                    let key_caret = self.caret_for(Focus::OptionKey(index));
                    let key_focused = self.focus == Focus::OptionKey(index);
                    draw_text_input(
                        ctx,
                        &mut body_layers,
                        font,
                        &mut self.widgets,
                        &self.interaction,
                        palette,
                        tokens,
                        text_pad,
                        cursor_on,
                        TextInputSpec {
                            placeholder: "ProxyJump",
                            text: &key.text(),
                            rect: rect(key_x, y, key_w, ctx.px(INPUT_H)),
                            focused: key_focused,
                            selected_all: false,
                            action: SshViewAction::FocusOptionKey(index),
                        },
                        key_caret,
                    )?;
                    self.resolve_pending_caret_click(
                        ctx,
                        font,
                        Focus::OptionValue(index),
                        &value.text(),
                        value_x + text_pad,
                    );
                    let value_caret = self.caret_for(Focus::OptionValue(index));
                    let value_focused = self.focus == Focus::OptionValue(index);
                    draw_text_input(
                        ctx,
                        &mut body_layers,
                        font,
                        &mut self.widgets,
                        &self.interaction,
                        palette,
                        tokens,
                        text_pad,
                        cursor_on,
                        TextInputSpec {
                            placeholder: "user@bastion",
                            text: &value.text(),
                            rect: rect(value_x, y, value_w, ctx.px(INPUT_H)),
                            focused: value_focused,
                            selected_all: false,
                            action: SshViewAction::FocusOptionValue(index),
                        },
                        value_caret,
                    )?;
                    draw_icon_button(
                        ctx,
                        &mut body_layers,
                        &mut self.widgets,
                        &self.interaction,
                        palette,
                        value_x + value_w + row_gap,
                        y,
                        remove_w,
                        SvgIcon::Trash2,
                        SshViewAction::RemoveOption(index),
                    )?;
                    y += ctx.px(INPUT_H) + row_gap;
                }

                let add_label = crate::i18n::tr("ssh-add-option");
                let add_w = ctx.measure_text_width(font, &add_label) + ctx.px(56.0);
                let add_state = self.button_state(SshViewAction::AddOption);
                draw_button(
                    ctx,
                    &mut body_layers,
                    font,
                    &mut self.widgets,
                    palette,
                    ButtonSpec {
                        label: &add_label,
                        action: SshViewAction::AddOption,
                        rect: rect(x, y, add_w, ctx.px(BTN_H)),
                        state: add_state,
                        kind: WidgetKind::Button,
                        variant: ButtonVariant::Secondary,
                    },
                )?;
                y += ctx.px(BTN_H) + ctx.px(18.0);
            }

            if let Some(err) = &error {
                ctx.draw_text(
                    &mut body_layers,
                    font,
                    x,
                    y,
                    &format!("⚠ {err}"),
                    palette.danger,
                    w,
                )?;
                y += line_h + ctx.px(12.0);
            }

            y += footer_room;
        }
        self.form_content_h = y - top;
        let clip = QuadClipRect::from_top_left_pixels(
            area.origin.x,
            body_top,
            area.max_x(),
            footer_top,
            &ctx.dimensions,
        );
        // The body shows under the capsule, but takes no clicks there: one
        // between its buttons must not reach a switch scrolled out of sight.
        self.widgets.clip_since(
            body_widgets_start,
            rect(
                area.origin.x,
                body_top,
                area.size.width,
                (capsule.origin.y - body_top).max(0.0),
            ),
        );
        heap.apply_to_clipped(layers, clip, 1.0)?;

        // The capsule, above the body scrolling under it.
        let radius = if row_fits {
            capsule_h / 2.0
        } else {
            ctx.px(24.0)
        };
        ctx.draw_shadow(
            layers,
            2,
            capsule,
            radius,
            ctx.px(16.0),
            ctx.px(4.0),
            LinearRgba::with_components(0.0, 0.0, 0.0, 0.35),
        )?;
        // The card colour is translucent; the body must not show through.
        ctx.draw_rounded_rect(
            layers,
            2,
            capsule.origin.x,
            capsule.origin.y,
            capsule.size.width,
            capsule.size.height,
            palette.window_bg,
            radius,
        )?;
        ctx.draw_rounded_frame(
            layers,
            2,
            capsule.origin.x,
            capsule.origin.y,
            capsule.size.width,
            capsule.size.height,
            palette.card_bg,
            palette.separator,
            radius,
        )?;
        let mut bx = capsule.origin.x + capsule_pad;
        let mut by = capsule.origin.y + capsule_pad;
        for ((label, action, variant), natural_w) in buttons.iter().zip(natural.iter()) {
            let each = if row_fits {
                *natural_w
            } else {
                capsule_w - capsule_pad * 2.0
            };
            let state = self.button_state(*action);
            draw_button_on_layer(
                ctx,
                layers,
                font,
                &mut self.widgets,
                palette,
                ButtonSpec {
                    label,
                    action: *action,
                    rect: rect(bx, by, each, ctx.px(BTN_H)),
                    state,
                    kind: WidgetKind::Button,
                    variant: *variant,
                },
                2,
            )?;
            if row_fits {
                bx += each + button_gap;
            } else {
                by += ctx.px(BTN_H) + stack_gap;
            }
        }

        // The header sits above the clip, so the band it occupies is simply
        // never drawn into by the body -- no mask needed.
        let mut hy = area.origin.y;
        title_ctx.draw_text_on_layer(layers, 2, title_font, x, hy, &heading, palette.text, w)?;
        hy += title_h + ctx.px(4.0);
        if let Some(endpoint) = endpoint {
            ctx.draw_text_on_layer(layers, 2, font, x, hy, &endpoint, palette.muted_text, w)?;
            hy += line_h;
        }
        hy += ctx.px(14.0);
        if connect {
            let label = crate::i18n::tr("ssh-connect");
            let state = self.button_state(SshViewAction::Connect);
            draw_button_on_layer(
                ctx,
                layers,
                font,
                &mut self.widgets,
                palette,
                ButtonSpec {
                    label: &label,
                    action: SshViewAction::Connect,
                    rect: rect(x, hy, w, ctx.px(BTN_H)),
                    state,
                    kind: WidgetKind::Button,
                    variant: ButtonVariant::Primary,
                },
                2,
            )?;
        }

        // No scrollbar and no edge fades here: a fixed-width column of three
        // cards is short enough that a thumb is mostly clutter, and a fade
        // over the clipped edge dimmed the Save/Cancel row into looking
        // broken. Content past the fold is simply clipped.
        Ok(())
    }

    /// The icon row: its label, and a button showing the icon picked with
    /// its name, which opens the menu of them. Returns the y under it.
    #[allow(clippy::too_many_arguments)]
    fn paint_icon_row(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        palette: UiPalette,
        icon: Option<HostIcon>,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<f32> {
        let line_h = ctx.metrics.cell_size.height as f32;
        ctx.draw_text(
            layers,
            font,
            x,
            y,
            &crate::i18n::tr("ssh-field-icon"),
            palette.text,
            width,
        )?;
        let icon = icon.unwrap_or(HostIcon::Server);
        let glyph = match icon {
            HostIcon::Server => SvgIcon::Server,
            HostIcon::Cloud => SvgIcon::Cloud,
            HostIcon::Globe => SvgIcon::Globe,
            HostIcon::Laptop => SvgIcon::Laptop,
            HostIcon::MacMini => SvgIcon::HardDrive,
            HostIcon::Desktop => SvgIcon::Monitor,
            HostIcon::Chip => SvgIcon::Cpu,
            HostIcon::Container => SvgIcon::Package,
        };
        let label = ssh_hosts::host_icon_name(icon);
        let action = SshViewAction::PickIcon;
        let state = self.button_state(action);
        let variant = ButtonVariant::Secondary;
        let button = rect(
            x,
            y + line_h + ctx.px(LABEL_GAP),
            width,
            ctx.px(INPUT_H),
        );
        self.icon_button = button;
        draw_button(
            ctx,
            layers,
            font,
            &mut self.widgets,
            palette,
            ButtonSpec {
                label: "",
                action,
                rect: button,
                state,
                kind: WidgetKind::Button,
                variant,
            },
        )?;
        // Laid out as the system's pop-up button: the choice at the start,
        // the arrows at the end.
        let (_, _, ink) = variant.colors(state, palette);
        let size = ctx.px(16.0);
        let inset = ctx.px(14.0);
        let icon_y = button.origin.y + (button.size.height - size) / 2.0;
        ctx.draw_svg_icon(layers, glyph, button.origin.x + inset, icon_y, size, ink)?;
        let arrows_x = button.max_x() - inset - size;
        ctx.draw_svg_icon(
            layers,
            SvgIcon::ChevronsUpDown,
            arrows_x,
            icon_y,
            size,
            palette.muted_text,
        )?;
        let text_x = button.origin.x + inset + size + ctx.px(8.0);
        ctx.draw_text(
            layers,
            font,
            text_x,
            button.origin.y + ((button.size.height - line_h) / 2.0).max(0.0),
            &label,
            ink,
            (arrows_x - ctx.px(8.0) - text_x).max(0.0),
        )?;
        Ok(y + Self::field_row_height(ctx))
    }

    /// Which of the three visual states a button is in. "Primary" is no
    /// longer one of them: that is the button's `variant`, and it is
    /// orthogonal to whether the pointer happens to be over it.
    fn button_state(&self, action: SshViewAction) -> ControlState {
        if self.interaction.pressed == Some(action) {
            ControlState::Pressed
        } else if self.interaction.hovered == Some(action) {
            ControlState::Hovered
        } else {
            ControlState::Normal
        }
    }
}

/// How the Tailscale group's caption reads the tailnet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tone {
    Off,
    Waiting,
    Up,
    Trouble,
}

impl Tone {
    fn color(self, palette: UiPalette) -> LinearRgba {
        let (r, g, b) = match self {
            Self::Off => return palette.muted_text,
            Self::Trouble => return palette.danger,
            Self::Waiting => WAITING_ORANGE,
            Self::Up => ONLINE_GREEN,
        };
        LinearRgba::with_srgba(r, g, b, 0xFF)
    }
}

/// The tailnet in a word or two, for the group's caption: nothing until
/// the page has read how it is.
fn tailnet_status(view: &TailnetView) -> Option<(&'static str, Tone)> {
    if view.error.is_some() {
        return Some(("ssh-tailscale-not-saved", Tone::Trouble));
    }
    if view.read_error.is_some() {
        return Some(("ssh-tailscale-unreadable", Tone::Trouble));
    }
    if !view.known {
        return None;
    }
    if !view.enabled {
        return Some(("ssh-tailscale-off", Tone::Off));
    }
    if !view.server_running {
        return Some(("ssh-tailscale-starting", Tone::Waiting));
    }
    Some(match &view.state {
        TailnetState::Connected { .. } => ("ssh-tailscale-connected", Tone::Up),
        TailnetState::NeedsLogin(_) => ("ssh-tailscale-login", Tone::Waiting),
        TailnetState::Failed(_) => ("ssh-tailscale-failed", Tone::Trouble),
        TailnetState::HeldElsewhere => ("ssh-tailscale-elsewhere", Tone::Waiting),
        TailnetState::Off | TailnetState::Starting => ("ssh-tailscale-starting", Tone::Waiting),
    })
}

/// The panel's account of the tailnet: where the node is, or why not.
fn tailnet_state_text(view: &TailnetView) -> String {
    if !view.known {
        return "…".to_string();
    }
    if !view.enabled {
        return crate::i18n::tr("ssh-tailscale-state-off");
    }
    if !view.server_running {
        return crate::i18n::tr("ssh-tailscale-server-not-running");
    }
    match &view.state {
        TailnetState::Off | TailnetState::Starting => crate::i18n::tr("ssh-tailscale-starting"),
        TailnetState::NeedsLogin(_) => crate::i18n::tr("ssh-tailscale-login"),
        TailnetState::Connected {
            name,
            address: Some(address),
        } => tr_named(
            "ssh-tailscale-state-connected",
            &[("name", name.clone()), ("address", address.clone())],
        ),
        TailnetState::Connected {
            name,
            address: None,
        } => tr_named(
            "ssh-tailscale-state-connected-no-address",
            &[("name", name.clone())],
        ),
        TailnetState::Failed(error) => {
            tr_named("ssh-tailscale-state-failed", &[("error", error.clone())])
        }
        TailnetState::HeldElsewhere => crate::i18n::tr("ssh-tailscale-state-elsewhere"),
    }
}

fn tr_named(id: &str, named: &[(&str, String)]) -> String {
    let mut args = FluentArgs::new();
    for (name, value) in named {
        args.set(*name, value.clone());
    }
    crate::i18n::tr_args(id, &args)
}

/// What a machine of the tailnet is drawn as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MachineIcon {
    /// A maker's mark in its own colours.
    Brand(BrandIcon),
    /// A mark tinted like the page's text.
    Plain(SvgIcon),
}

/// The maker's mark where there is one -- the distribution's, for Linux --
/// and a plain picture where not.
fn machine_icon(machine: &TailnetMachine) -> MachineIcon {
    match machine.device {
        Device::Mac | Device::IPhone | Device::IPad | Device::AppleTv => {
            MachineIcon::Plain(SvgIcon::Apple)
        }
        Device::Android => MachineIcon::Brand(BrandIcon::Android),
        Device::Linux => {
            MachineIcon::Brand(distro_to_icon(&machine.distro).unwrap_or(BrandIcon::Linux))
        }
        // The app carries no Windows mark.
        Device::Windows => MachineIcon::Plain(SvgIcon::Monitor),
        Device::Unknown => MachineIcon::Plain(SvgIcon::Server),
    }
}

/// Under a machine's name: what it is, and whether it is online.
fn machine_subtitle(machine: &TailnetMachine) -> String {
    let presence = match machine.online {
        Some(true) => Some(crate::i18n::tr("ssh-tailscale-online")),
        Some(false) => Some(crate::i18n::tr("ssh-tailscale-offline")),
        None => None,
    };
    match (machine.device.name(), presence) {
        (Some(device), Some(presence)) => format!("{device}  ·  {presence}"),
        (Some(device), None) => device.to_string(),
        (None, Some(presence)) => presence,
        (None, None) => machine.target.clone(),
    }
}

/// Whether the search box's text, lowercased, finds this machine.
fn machine_matches(machine: &TailnetMachine, needle: &str) -> bool {
    let addresses = machine
        .addresses
        .iter()
        .map(|address| address.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "{} {} {} {addresses}",
        machine.name,
        machine.target,
        machine.device.name().unwrap_or_default()
    )
    .to_ascii_lowercase()
    .contains(needle)
}

/// Whether a Tailscale field keeps the text it went out with (`change`)
/// rather than show the file: while that change is saved, and while the
/// panel says why it could not be. Forgets the change once it is in.
fn keeps_typed_text(change: &mut Option<u64>) -> bool {
    match change.map(crate::tailnet::saved) {
        Some(None) | Some(Some(false)) => true,
        Some(Some(true)) => {
            *change = None;
            false
        }
        None => false,
    }
}

impl SshHostsView {
    /// The tailnet as last read -- read again when that is due -- with the
    /// group's fields and machines brought up to it.
    fn read_tailnet(&mut self) -> TailnetView {
        if !crate::tailnet::available() {
            return TailnetView::default();
        }
        if let Some(window) = self.window.clone() {
            if crate::tailnet::poll_is_due() {
                crate::tailnet::refresh(window);
            }
        }
        let view = crate::tailnet::view();
        self.sync_tailnet_fields(&view);
        self.tailnet.login_url = view.login_url().map(str::to_string);
        let needle = self.search.text().trim().to_ascii_lowercase();
        self.tailnet.machines = view
            .machines
            .iter()
            .filter(|machine| needle.is_empty() || machine_matches(machine, &needle))
            .cloned()
            .collect();
        view
    }

    /// Fields nobody is editing show what the settings file says, which
    /// `thinkterm tailnet` may have changed meanwhile -- once what was typed
    /// into them is saved. Text that could not be saved stays, for the
    /// panel says why, until another change is saved.
    fn sync_tailnet_fields(&mut self, view: &TailnetView) {
        for field in 0..TAILNET_FIELD_COUNT {
            if self.focus == Focus::Tailnet(field)
                || self.tailnet.dirty[field]
                || keeps_typed_text(&mut self.tailnet.changes[field])
            {
                continue;
            }
            let file = match field {
                // A saved key never shows: the field empties once it is in.
                TAILNET_AUTH_KEY => "",
                TAILNET_CONTROL_URL => view.control_url.as_str(),
                _ => view.hostname.as_str(),
            };
            if self.tailnet.fields[field].text() != file {
                self.tailnet.fields[field].set_text_end(file.to_string());
            }
        }
    }

    /// Sends a Tailscale field that was typed into. The node restarts with
    /// it once it is saved (see thinkterm_tailnet::update).
    fn commit_tailnet_field(&mut self, field: usize) {
        if !std::mem::take(&mut self.tailnet.dirty[field]) {
            return;
        }
        let text = self.tailnet.fields[field].text().to_string();
        self.tailnet.changes[field] = match field {
            TAILNET_AUTH_KEY => crate::tailnet::set_auth_key(&text),
            TAILNET_CONTROL_URL => Some(crate::tailnet::set_control_url(&text)),
            _ => Some(crate::tailnet::set_hostname(&text)),
        };
    }

    /// Sends every Tailscale field typed into but `keep`, the one still
    /// being typed in.
    fn commit_tailnet_fields(&mut self, keep: Option<usize>) {
        for field in 0..TAILNET_FIELD_COUNT {
            if Some(field) != keep {
                self.commit_tailnet_field(field);
            }
        }
    }

    /// After a click or a key: a Tailscale field goes out once it is left,
    /// as Settings' fields did.
    fn commit_left_tailnet_fields(&mut self) {
        let keep = match self.focus {
            Focus::Tailnet(field) if self.tailnet.panel_open => Some(field),
            _ => None,
        };
        self.commit_tailnet_fields(keep);
    }

    fn open_tailnet_panel(&mut self) {
        if !crate::tailnet::available() {
            return;
        }
        // One inspector: a host's form gives way, as it does to another
        // host's.
        self.close_form();
        self.tailnet.panel_open = true;
        self.reset_form_scroll();
    }

    fn close_tailnet_panel(&mut self) {
        if !self.tailnet.panel_open {
            return;
        }
        self.commit_tailnet_fields(None);
        self.tailnet.panel_open = false;
        if matches!(self.focus, Focus::Tailnet(_)) {
            self.focus = Focus::Search;
        }
        self.reset_form_scroll();
    }

    /// The saved host that is this machine of the tailnet, if one is: its
    /// address, or one of its other addresses, names the machine.
    fn saved_host_for(&self, machine: &TailnetMachine) -> Option<&SshHostEntry> {
        self.saved_hosts.iter().find(|entry| {
            entry.spec.network == SshNetwork::Tailnet
                && (machine.answers_to(&entry.spec.host)
                    || entry
                        .spec
                        .other_addresses
                        .iter()
                        .any(|address| machine.answers_to(address)))
        })
    }

    /// A machine of the tailnet, clicked the way a host's card is: one saved
    /// as a host is selected and its form opens in the inspector, a second
    /// click connecting (see the mouse handler); another opens a new host's
    /// form with its name and address filled in, for whom to sign in as --
    /// saved, it is a host like any other, in Hosts.
    fn open_tailnet_machine(&mut self, index: usize) -> ContentViewResponse {
        let Some(machine) = self.tailnet.machines.get(index).cloned() else {
            return ContentViewResponse::Redraw;
        };
        if let Some(entry) = self.saved_host_for(&machine).cloned() {
            // Its ring takes over from a host card's.
            self.selection_active = false;
            // As for a host card: the form already open for it keeps what
            // was typed into it.
            // As the store has it now: the list's copy can be older than what
            // another window saved since, and saving that would undo it.
            if self.form.as_ref().and_then(|form| form.editing.as_deref()) != Some(entry.id.as_str()) {
                let spec = ssh_hosts::saved_host_spec_if_loaded(&entry.id).unwrap_or(entry.spec);
                self.open_form_for(&entry.id, spec);
            }
            self.tailnet.selected = Some(entry.id);
            return ContentViewResponse::Redraw;
        }
        self.open_new_form();
        if let Some(form) = self.form.as_mut() {
            form.network = SshNetwork::Tailnet;
            form.fields[FIELD_NAME].set_text_end(machine.name.clone());
            form.fields[FIELD_HOST].set_text_end(machine.target.clone());
        }
        // Where it is, the tailnet said; whom to sign in as is left.
        self.focus = Focus::Field(FIELD_USER);
        self.selection_active = false;
        ContentViewResponse::Redraw
    }

    /// What the Tailscale group holds, or None when it does not show: in a
    /// build that cannot join a tailnet, and in a search that finds none of
    /// its machines.
    fn tailnet_body(&self, view: &TailnetView) -> Option<TailnetBody> {
        if !crate::tailnet::available() {
            return None;
        }
        if !self.search.text().trim().is_empty() {
            return (!self.tailnet.machines.is_empty()).then_some(TailnetBody::Machines);
        }
        if !view.known {
            return Some(TailnetBody::Pending);
        }
        let tr = crate::i18n::tr;
        let notice = |icon, title: &str, subtitle: String, button: Option<(&str, NoticeButton)>| {
            TailnetBody::Notice(TailnetNotice {
                icon,
                title: tr(title),
                subtitle,
                button: button.map(|(label, which)| (tr(label), which)),
                invite: false,
            })
        };
        if !view.enabled {
            return Some(TailnetBody::Notice(TailnetNotice {
                icon: SvgIcon::Network,
                title: tr("ssh-tailscale-off-title"),
                subtitle: tr("ssh-tailscale-off-subtitle"),
                button: Some((tr("ssh-tailscale-turn-on"), NoticeButton::TurnOn)),
                invite: true,
            }));
        }
        if !view.server_running {
            return Some(notice(
                SvgIcon::Network,
                "ssh-tailscale-starting",
                tr("ssh-tailscale-server-not-running"),
                None,
            ));
        }
        Some(match &view.state {
            TailnetState::Connected { .. } if !self.tailnet.machines.is_empty() => {
                TailnetBody::Machines
            }
            TailnetState::Connected { .. } => notice(
                SvgIcon::Network,
                "ssh-tailscale-empty-title",
                tr("ssh-tailscale-empty-subtitle"),
                None,
            ),
            TailnetState::NeedsLogin(_) => notice(
                SvgIcon::KeyRound,
                "ssh-tailscale-login",
                tr("ssh-tailscale-login-description"),
                Some(("ssh-tailscale-login-button", NoticeButton::LogIn)),
            ),
            TailnetState::Failed(error) => notice(
                SvgIcon::CircleAlert,
                "ssh-tailscale-failed",
                error.clone(),
                Some(("ssh-tailscale-settings-button", NoticeButton::Settings)),
            ),
            TailnetState::HeldElsewhere => notice(
                SvgIcon::Network,
                "ssh-tailscale-elsewhere",
                tr("ssh-tailscale-state-elsewhere"),
                None,
            ),
            TailnetState::Off | TailnetState::Starting => notice(
                SvgIcon::Network,
                "ssh-tailscale-starting",
                tr("ssh-tailscale-starting-subtitle"),
                None,
            ),
        })
    }

    /// The room the caption's status and gear take at its right end.
    fn tailnet_status_width(
        &self,
        ctx: &DrawContext,
        font: &Rc<LoadedFont>,
        view: &TailnetView,
    ) -> f32 {
        let gear = ctx.px(TAILNET_GEAR + 12.0);
        match tailnet_status(view) {
            Some((status, _)) => {
                gear + ctx.px(10.0 + 7.0 + 8.0)
                    + ctx.measure_text_width(font, &crate::i18n::tr(status))
            }
            None => gear,
        }
    }

    /// The right end of the Tailscale caption: the tailnet's state by its
    /// dot, and the gear that opens the panel. Either opens it.
    #[allow(clippy::too_many_arguments)]
    fn paint_tailnet_status(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        palette: UiPalette,
        view: &TailnetView,
        x: f32,
        y: f32,
        width: f32,
        viewport: RectF,
    ) -> anyhow::Result<()> {
        let height = ctx.px(GROUP_HEADER_H);
        if !row_visible(y, height, viewport) {
            return Ok(());
        }
        let widgets_start = self.widgets.len();
        let action = SshViewAction::TailnetSettings;
        let hovered = self.button_state(action) != ControlState::Normal;
        let gear = ctx.px(TAILNET_GEAR).min(height);
        let gear_x = x + width - gear;
        let gear_y = y + (height - gear) / 2.0;
        let status = tailnet_status(view).map(|(id, tone)| (crate::i18n::tr(id), tone));
        let line_h = ctx.metrics.cell_size.height as f32;
        let status_w = status
            .as_ref()
            .map(|(text, _)| ctx.measure_text_width(font, text))
            .unwrap_or_default();
        let status_x = gear_x - ctx.px(10.0) - status_w;
        let dot = ctx.px(8.0);
        let dot_x = status_x - ctx.px(7.0) - dot;
        let hit_x = if status.is_some() { dot_x } else { gear_x };
        self.widgets.push(
            rect(hit_x, y, x + width - hit_x, height),
            WidgetKind::Button,
            action,
        );
        // A quiet gear, open panel or not: a lit square beside the
        // caption outshouted everything on the page.
        if hovered {
            ctx.draw_rounded_rect(
                layers,
                0,
                gear_x,
                gear_y,
                gear,
                gear,
                palette.control_hover_bg,
                ctx.px(8.0),
            )?;
        }
        // As large as an icon button's, next to the caption's text.
        let glyph = (line_h + ctx.px(4.0))
            .clamp(ctx.px(20.0), ctx.px(30.0))
            .min(gear - ctx.px(6.0))
            .round();
        ctx.draw_svg_icon(
            layers,
            SvgIcon::Settings,
            gear_x + (gear - glyph) / 2.0,
            gear_y + (gear - glyph) / 2.0,
            glyph,
            if hovered {
                palette.text
            } else {
                palette.muted_text
            },
        )?;
        if let Some((status, tone)) = &status {
            ctx.draw_disc(
                layers,
                1,
                dot_x + dot / 2.0,
                y + height / 2.0,
                dot,
                tone.color(palette),
            )?;
            ctx.draw_text(
                layers,
                font,
                status_x,
                y + ((height - line_h) / 2.0).max(0.0),
                status,
                if *tone == Tone::Trouble {
                    palette.danger
                } else {
                    palette.secondary_text
                },
                status_w,
            )?;
        }
        self.widgets.clip_since(widgets_start, viewport);
        Ok(())
    }

    /// One machine of the tailnet: its mark, its name, what it is and
    /// whether it is online -- and, when it is saved as a host already,
    /// that it is.
    #[allow(clippy::too_many_arguments)]
    fn paint_tailnet_card(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        palette: UiPalette,
        index: usize,
        card: RectF,
        viewport: RectF,
    ) -> anyhow::Result<()> {
        let Some(machine) = self.tailnet.machines.get(index).cloned() else {
            return Ok(());
        };
        let action = SshViewAction::TailnetMachine(index);
        let Some(visible) = card.intersection(&viewport) else {
            return Ok(());
        };
        self.widgets.push(visible, WidgetKind::SidebarRow, action);
        let hovered = self.interaction.hovered == Some(action);
        let editing = self.form.as_ref().and_then(|form| form.editing.as_deref());
        let selected = editing.is_some()
            && self.tailnet.selected.as_deref() == editing
            && self.saved_host_for(&machine).map(|entry| entry.id.as_str()) == editing;
        let bg = if hovered {
            palette.sidebar_row_hover_bg
        } else {
            palette.card_bg
        };
        ctx.draw_rounded_frame(
            layers,
            0,
            card.origin.x,
            card.origin.y,
            card.size.width,
            card.size.height,
            bg,
            if selected {
                palette.accent
            } else {
                palette.separator
            },
            ctx.px(CARD_RADIUS),
        )?;

        let online = machine.online == Some(true);
        let offline = machine.online == Some(false);
        let pad = ctx.px(CARD_PAD);
        let line_h = ctx.metrics.cell_size.height as f32;
        // The same size a host's logo gets.
        let icon = (line_h * 2.2).clamp(ctx.px(50.0), ctx.px(64.0));
        let icon_x = card.origin.x + pad;
        let icon_y = card.origin.y + (card.size.height - icon) / 2.0;
        match machine_icon(&machine) {
            MachineIcon::Brand(brand) => {
                ctx.draw_brand_icon(layers, brand, icon_x, icon_y, icon)?
            }
            MachineIcon::Plain(mark) => ctx.draw_svg_icon(
                layers,
                mark,
                icon_x,
                icon_y,
                icon,
                if offline {
                    palette.muted_text
                } else {
                    palette.secondary_text
                },
            )?,
        }
        if offline {
            // A machine that is away fades: its mark under a veil of the
            // card's own colour.
            ctx.draw_rect(layers, 2, icon_x, icon_y, icon, icon, bg.mul_alpha(0.55))?;
        }

        let text_x = icon_x + icon + ctx.px(14.0);
        let right = card.max_x() - pad;
        let mut title_w = (right - text_x).max(0.0);
        let stack = line_h * 2.0 + ctx.px(4.0);
        let title_y = card.origin.y + ((card.size.height - stack) / 2.0).max(ctx.px(8.0));
        // Saved already: clicking it connects the way it was saved. The
        // name comes first: where both do not fit, the word goes and the
        // check stays.
        if self.saved_host_for(&machine).is_some() {
            let label = crate::i18n::tr("ssh-tailscale-added");
            let check = (line_h * 0.85).round();
            let gap = ctx.px(10.0);
            let label_w = ctx.measure_text_width(font, &label);
            let worded = check + ctx.px(4.0) + label_w;
            let name_w = ctx.measure_text_width(font, &machine.name);
            let word = name_w + gap + worded <= title_w;
            let tag_x = right - if word { worded } else { check };
            ctx.draw_svg_icon(
                layers,
                SvgIcon::Check,
                tag_x,
                title_y + (line_h - check) / 2.0,
                check,
                palette.muted_text,
            )?;
            if word {
                ctx.draw_text(
                    layers,
                    font,
                    tag_x + check + ctx.px(4.0),
                    title_y,
                    &label,
                    palette.muted_text,
                    label_w,
                )?;
            }
            title_w = (tag_x - gap - text_x).max(0.0);
        }
        ctx.draw_text(
            layers,
            font,
            text_x,
            title_y,
            &machine.name,
            if offline {
                palette.secondary_text
            } else {
                palette.text
            },
            title_w,
        )?;
        let subtitle_y = title_y + line_h + ctx.px(4.0);
        let mut subtitle_x = text_x;
        if online {
            let dot = ctx.px(8.0);
            ctx.draw_disc(
                layers,
                1,
                subtitle_x + dot / 2.0,
                subtitle_y + line_h / 2.0,
                dot,
                Tone::Up.color(palette),
            )?;
            subtitle_x += dot + ctx.px(7.0);
        }
        ctx.draw_text(
            layers,
            font,
            subtitle_x,
            subtitle_y,
            &machine_subtitle(&machine),
            palette.muted_text,
            (right - subtitle_x).max(0.0),
        )?;
        Ok(())
    }

    /// The notice's height: its two lines, or its button beside them when
    /// that is taller.
    fn tailnet_notice_height(ctx: &DrawContext, notice: &TailnetNotice) -> f32 {
        let line_h = ctx.metrics.cell_size.height as f32;
        let words = line_h * 2.0 + ctx.px(4.0);
        let button = if notice.button.is_some() {
            ctx.px(BTN_H)
        } else {
            0.0
        };
        ctx.px(CARD_PAD) * 2.0 + words.max(button)
    }

    /// A faint mesh of nodes behind a card, its machines as it were: over
    /// the card from `from` (a fraction of its width) to its right end,
    /// fading in towards the right, and `strength` at its strongest.
    fn paint_card_mesh(
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        color: LinearRgba,
        card: RectF,
        from: f32,
        strength: f32,
    ) -> anyhow::Result<()> {
        let step = ctx.px(28.0);
        let inset = ctx.px(18.0);
        let start = card.origin.x + card.size.width * from;
        let end = card.max_x() - inset;
        if end <= start || card.size.height <= inset * 2.0 {
            return Ok(());
        }
        // Rows spread over the whole height, the grid centred in it.
        let rows = ((card.size.height - inset * 2.0) / step).floor() as usize;
        let top = card.origin.y + (card.size.height - rows as f32 * step) / 2.0;
        let columns = ((end - start) / step).floor() as usize;
        for row in 0..=rows {
            for column in 0..=columns {
                let x = end - column as f32 * step;
                let y = top + row as f32 * step;
                let fade = strength * (1.0 - column as f32 / columns.max(1) as f32);
                // Every few dots a node, linked to the one at its right.
                let node = (row * 7 + column * 3) % 5 == 0;
                let size = if node { ctx.px(9.0) } else { ctx.px(4.0) };
                let alpha = if node { 0.32 } else { 0.18 } * fade;
                ctx.draw_disc(layers, 0, x, y, size, color.mul_alpha(alpha))?;
                if node && column > 0 {
                    ctx.draw_rect(
                        layers,
                        0,
                        x,
                        y - ctx.px(1.0),
                        step,
                        ctx.px(2.0),
                        color.mul_alpha(0.16 * fade),
                    )?;
                }
            }
        }
        Ok(())
    }

    /// The card the Tailscale group shows instead of machines: the tailnet
    /// off -- an invitation to turn it on -- or on its way, or waiting.
    #[allow(clippy::too_many_arguments)]
    fn paint_tailnet_notice(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        palette: UiPalette,
        notice: &TailnetNotice,
        card: RectF,
        viewport: RectF,
    ) -> anyhow::Result<()> {
        if !row_visible(card.origin.y, card.size.height, viewport) {
            return Ok(());
        }
        let widgets_start = self.widgets.len();
        let (fill, border, mark) = if notice.invite {
            (
                palette.accent.mul_alpha(0.06),
                palette.accent.mul_alpha(0.55),
                palette.accent,
            )
        } else {
            (palette.card_bg, palette.separator, palette.muted_text)
        };
        ctx.draw_rounded_frame(
            layers,
            0,
            card.origin.x,
            card.origin.y,
            card.size.width,
            card.size.height,
            fill,
            border,
            ctx.px(CARD_RADIUS),
        )?;
        let pad = ctx.px(CARD_PAD);
        let line_h = ctx.metrics.cell_size.height as f32;
        let icon = (line_h * 1.8).clamp(ctx.px(40.0), ctx.px(52.0));
        let icon_x = card.origin.x + pad;
        let words = line_h * 2.0 + ctx.px(4.0);
        let title_y = card.origin.y + (card.size.height - words) / 2.0;
        if notice.invite {
            Self::paint_card_mesh(ctx, layers, palette.accent, card, 0.4, 1.0)?;
        }
        // The icon beside the two lines, and the button at the right, all
        // centred on the card's height.
        ctx.draw_svg_icon(
            layers,
            notice.icon,
            icon_x,
            title_y + (words - icon) / 2.0,
            icon,
            mark,
        )?;
        let mut right = card.max_x() - pad;
        if let Some((label, which)) = &notice.button {
            let button_w = ctx.measure_text_width(font, label) + ctx.px(56.0);
            let button_x = right - button_w;
            self.tailnet.notice = Some(*which);
            let state = self.button_state(SshViewAction::TailnetNotice);
            draw_button(
                ctx,
                layers,
                font,
                &mut self.widgets,
                palette,
                ButtonSpec {
                    label,
                    action: SshViewAction::TailnetNotice,
                    rect: rect(
                        button_x,
                        card.origin.y + (card.size.height - ctx.px(BTN_H)) / 2.0,
                        button_w,
                        ctx.px(BTN_H),
                    ),
                    state,
                    kind: WidgetKind::Button,
                    variant: if *which == NoticeButton::Settings {
                        ButtonVariant::Secondary
                    } else {
                        ButtonVariant::Primary
                    },
                },
            )?;
            right = button_x - ctx.px(16.0);
        }
        let text_x = icon_x + icon + ctx.px(16.0);
        let text_w = (right - text_x).max(0.0);
        ctx.draw_text(
            layers,
            font,
            text_x,
            title_y,
            &notice.title,
            palette.text,
            text_w,
        )?;
        ctx.draw_text(
            layers,
            font,
            text_x,
            title_y + line_h + ctx.px(4.0),
            &notice.subtitle,
            palette.muted_text,
            text_w,
        )?;
        self.widgets.clip_since(widgets_start, viewport);
        Ok(())
    }

    /// A labelled text field: the host form's, or the Tailscale panel's.
    #[allow(clippy::too_many_arguments)]
    fn paint_input_row(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        palette: UiPalette,
        tokens: UiTokens,
        cursor_on: bool,
        label: &str,
        focus: Focus,
        action: SshViewAction,
        shown: &str,
        placeholder: &str,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<f32> {
        let line_h = ctx.metrics.cell_size.height as f32;
        ctx.draw_text(layers, font, x, y, label, palette.muted_text, width)?;
        let input_y = y + line_h + ctx.px(LABEL_GAP);
        let text_pad = ctx.px(12.0);
        self.resolve_pending_caret_click(ctx, font, focus, shown, x + text_pad);
        let caret = self.caret_for(focus);
        let focused = self.focus == focus;
        draw_text_input(
            ctx,
            layers,
            font,
            &mut self.widgets,
            &self.interaction,
            palette,
            tokens,
            text_pad,
            cursor_on,
            TextInputSpec {
                placeholder,
                text: shown,
                rect: rect(x, input_y, width, ctx.px(INPUT_H)),
                focused,
                selected_all: false,
                action,
            },
            caret,
        )?;
        Ok(input_y + ctx.px(INPUT_H))
    }

    /// The Tailscale panel, in the inspector's place: the switch, how the
    /// node is doing, how it logs in, where it connects and under what
    /// name, and leaving for good.
    #[allow(clippy::too_many_arguments)]
    fn paint_tailnet_panel(
        &mut self,
        ctx: &DrawContext,
        title_ctx: &DrawContext,
        section_ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        title_font: &Rc<LoadedFont>,
        section_font: &Rc<LoadedFont>,
        palette: UiPalette,
        tokens: UiTokens,
        cursor_on: bool,
        area: RectF,
        view: &TailnetView,
    ) -> anyhow::Result<()> {
        let tr = crate::i18n::tr;
        self.form_area = area;
        let x = area.origin.x;
        let w = area.size.width;
        let pad = ctx.px(20.0);
        let inner = (w - pad * 2.0).max(0.0);
        let line_h = ctx.metrics.cell_size.height as f32;
        let section_h = section_ctx.metrics.cell_size.height as f32;
        let title_h = title_ctx.metrics.cell_size.height as f32;
        let field_h = Self::field_row_height(ctx);
        let gap = ctx.px(FIELD_GAP);
        let card_gap = ctx.px(INSPECTOR_CARD_GAP);
        let toggle_w = ctx.px(64.0);
        let toggle_h = ctx.px(36.0);

        // The header does not scroll: the switch is what the panel is for.
        let band = title_h.max(toggle_h);
        let header_h = band + ctx.px(18.0);
        let body_top = area.origin.y + header_h;
        let body_height = (area.max_y() - body_top).max(0.0);
        self.form_scroll
            .set_extents(body_height, self.form_content_h);
        let top = body_top - self.form_scroll.offset;
        let mut y = top;

        let fields: [String; TAILNET_FIELD_COUNT] =
            std::array::from_fn(|field| self.tailnet.fields[field].text().to_string());
        let login = self.tailnet.login_url.is_some();
        let copied = self
            .tailnet
            .copied_until
            .is_some_and(|until| Instant::now() < until);
        let wrap = |text: &str, width: f32| -> Vec<String> {
            if width <= 0.0 {
                return Vec::new();
            }
            wrap_snippet_text_for_width(text, 6, false, |segment| {
                ctx.measure_text_width(font, segment) / width
            })
        };
        // The state by its dot, then what else holds.
        let tone = tailnet_status(view).map_or(Tone::Off, |(_, tone)| tone);
        let dot = ctx.px(9.0);
        let dot_room = dot + ctx.px(8.0);
        let state_lines = wrap(&tailnet_state_text(view), inner - dot_room);
        let mut notes: Vec<(Vec<String>, LinearRgba)> = Vec::new();
        if view.enabled && view.serves_ssh {
            notes.push((
                wrap(&tr("ssh-tailscale-serving-ssh"), inner),
                palette.muted_text,
            ));
        }
        if let Some(error) = &view.error {
            notes.push((
                wrap(
                    &tr_named("ssh-tailscale-save-error", &[("error", error.clone())]),
                    inner,
                ),
                palette.danger,
            ));
        }
        if let Some(error) = &view.read_error {
            notes.push((
                wrap(
                    &tr_named("ssh-tailscale-read-error", &[("error", error.clone())]),
                    inner,
                ),
                palette.danger,
            ));
        }
        let note_gap = ctx.px(6.0);
        let status_rows = state_lines.len() as f32 * line_h
            + notes
                .iter()
                .map(|(lines, _)| note_gap + lines.len() as f32 * line_h)
                .sum::<f32>();
        let login_lines = if login {
            wrap(&tr("ssh-tailscale-login-description"), inner)
        } else {
            Vec::new()
        };
        // Only with something to forget.
        let logout = view.has_identity || view.has_auth_key;
        let logout_lines = if logout {
            wrap(&tr("ssh-tailscale-logout-description"), inner)
        } else {
            Vec::new()
        };

        // Recorded and replayed clipped to the body, as the host form is.
        let mut heap = HeapQuadAllocator::default();
        let body_widgets_start = self.widgets.len();
        {
            let mut body_layers = TripleLayerQuadAllocator::Heap(&mut heap);

            // ---- Status
            let status_h = Self::inspector_card_height(ctx, section_h, &[status_rows]);
            let mut fy = Self::paint_inspector_card(
                ctx,
                section_ctx,
                &mut body_layers,
                section_font,
                palette,
                tokens,
                &tr("ssh-tailscale-status"),
                rect(x, y, w, status_h),
            )?;
            ctx.draw_disc(
                &mut body_layers,
                1,
                x + pad + dot / 2.0,
                fy + line_h / 2.0,
                dot,
                tone.color(palette),
            )?;
            for line in &state_lines {
                ctx.draw_text(
                    &mut body_layers,
                    font,
                    x + pad + dot_room,
                    fy,
                    line,
                    palette.text,
                    inner - dot_room,
                )?;
                fy += line_h;
            }
            for (lines, color) in &notes {
                fy += note_gap;
                for line in lines {
                    ctx.draw_text(&mut body_layers, font, x + pad, fy, line, *color, inner)?;
                    fy += line_h;
                }
            }
            y += status_h + card_gap;

            // ---- Log in: the link while the control server waits for
            // someone to approve this machine, and a key instead of it.
            let mut login_rows = Vec::new();
            if login {
                login_rows
                    .push(login_lines.len() as f32 * line_h + ctx.px(LABEL_GAP) + ctx.px(BTN_H));
            }
            login_rows.push(field_h);
            let login_h = Self::inspector_card_height(ctx, section_h, &login_rows);
            let mut fy = Self::paint_inspector_card(
                ctx,
                section_ctx,
                &mut body_layers,
                section_font,
                palette,
                tokens,
                &tr("ssh-tailscale-login-section"),
                rect(x, y, w, login_h),
            )?;
            if login {
                for line in &login_lines {
                    ctx.draw_text(
                        &mut body_layers,
                        font,
                        x + pad,
                        fy,
                        line,
                        palette.muted_text,
                        inner,
                    )?;
                    fy += line_h;
                }
                fy += ctx.px(LABEL_GAP);
                let open_label = tr("ssh-tailscale-login-open");
                let copy_label = tr(if copied {
                    "ssh-tailscale-copied"
                } else {
                    "ssh-tailscale-login-copy"
                });
                let button_gap = ctx.px(10.0);
                let open_w = (ctx.measure_text_width(font, &open_label) + ctx.px(56.0)).min(inner);
                let copy_w = (ctx.measure_text_width(font, &copy_label) + ctx.px(56.0))
                    .min((inner - open_w - button_gap).max(0.0));
                for (label, action, variant, button_x, button_w) in [
                    (
                        &open_label,
                        SshViewAction::OpenTailnetLogin,
                        ButtonVariant::Primary,
                        x + pad,
                        open_w,
                    ),
                    (
                        &copy_label,
                        SshViewAction::CopyTailnetLogin,
                        ButtonVariant::Secondary,
                        x + pad + open_w + button_gap,
                        copy_w,
                    ),
                ] {
                    if button_w <= 0.0 {
                        continue;
                    }
                    let state = self.button_state(action);
                    draw_button(
                        ctx,
                        &mut body_layers,
                        font,
                        &mut self.widgets,
                        palette,
                        ButtonSpec {
                            label,
                            action,
                            rect: rect(button_x, fy, button_w, ctx.px(BTN_H)),
                            state,
                            kind: WidgetKind::Button,
                            variant,
                        },
                    )?;
                }
                fy += ctx.px(BTN_H) + gap;
            }
            // Typed characters show as dots, one each, so the caret and the
            // selection still land between them.
            let typed_key = "\u{2022}".repeat(fields[TAILNET_AUTH_KEY].chars().count());
            let key_hint = tr(if view.has_auth_key {
                "ssh-tailscale-auth-key-saved"
            } else {
                "ssh-tailscale-auth-key-placeholder"
            });
            self.paint_input_row(
                ctx,
                &mut body_layers,
                font,
                palette,
                tokens,
                cursor_on,
                &tr("ssh-tailscale-auth-key"),
                Focus::Tailnet(TAILNET_AUTH_KEY),
                SshViewAction::FocusTailnetField(TAILNET_AUTH_KEY),
                &typed_key,
                &key_hint,
                x + pad,
                fy,
                inner,
            )?;
            y += login_h + card_gap;

            // ---- Connection: where the node signs in, and as what.
            let connection_h = Self::inspector_card_height(ctx, section_h, &[field_h, field_h]);
            let mut fy = Self::paint_inspector_card(
                ctx,
                section_ctx,
                &mut body_layers,
                section_font,
                palette,
                tokens,
                &tr("ssh-group-connection"),
                rect(x, y, w, connection_h),
            )?;
            fy = self.paint_input_row(
                ctx,
                &mut body_layers,
                font,
                palette,
                tokens,
                cursor_on,
                &tr("ssh-tailscale-control"),
                Focus::Tailnet(TAILNET_CONTROL_URL),
                SshViewAction::FocusTailnetField(TAILNET_CONTROL_URL),
                &fields[TAILNET_CONTROL_URL],
                &tr("ssh-tailscale-control-placeholder"),
                x + pad,
                fy,
                inner,
            )? + gap;
            self.paint_input_row(
                ctx,
                &mut body_layers,
                font,
                palette,
                tokens,
                cursor_on,
                &tr("ssh-tailscale-hostname"),
                Focus::Tailnet(TAILNET_HOSTNAME),
                SshViewAction::FocusTailnetField(TAILNET_HOSTNAME),
                &fields[TAILNET_HOSTNAME],
                &tr("ssh-tailscale-hostname-placeholder"),
                x + pad,
                fy,
                inner,
            )?;
            y += connection_h + card_gap;

            // ---- Log out
            if logout {
                let label = tr("ssh-tailscale-logout");
                let label_pad = ctx.px(12.0);
                let label_w = ctx.measure_text_width(font, &label);
                let action = SshViewAction::TailnetLogout;
                let hit = rect(
                    x,
                    y,
                    (label_w + label_pad * 2.0).min(w),
                    line_h + ctx.px(14.0),
                );
                self.widgets.push(hit, WidgetKind::Button, action);
                if self.button_state(action) != ControlState::Normal {
                    ctx.draw_rounded_rect(
                        &mut body_layers,
                        0,
                        hit.origin.x,
                        hit.origin.y,
                        hit.size.width,
                        hit.size.height,
                        palette.control_hover_bg,
                        ctx.px(8.0),
                    )?;
                }
                ctx.draw_text(
                    &mut body_layers,
                    font,
                    x + label_pad,
                    y + ctx.px(7.0),
                    &label,
                    palette.danger,
                    label_w,
                )?;
                y += hit.size.height + ctx.px(4.0);
                for line in &logout_lines {
                    ctx.draw_text(
                        &mut body_layers,
                        font,
                        x + label_pad,
                        y,
                        line,
                        palette.muted_text,
                        inner,
                    )?;
                    y += line_h;
                }
            }
            y += ctx.px(PAD);
        }
        self.form_content_h = y - top;
        let clip = QuadClipRect::from_top_left_pixels(
            area.origin.x,
            body_top,
            area.max_x(),
            area.max_y(),
            &ctx.dimensions,
        );
        self.widgets.clip_since(
            body_widgets_start,
            rect(area.origin.x, body_top, area.size.width, body_height),
        );
        heap.apply_to_clipped(layers, clip, 1.0)?;

        // The header: the name, and the switch.
        let hy = area.origin.y;
        title_ctx.draw_text_on_layer(
            layers,
            2,
            title_font,
            x,
            hy + (band - title_h) / 2.0,
            &tr("ssh-group-tailscale"),
            palette.text,
            (w - toggle_w - ctx.px(12.0)).max(0.0),
        )?;
        draw_toggle(
            ctx,
            layers,
            &mut self.widgets,
            &self.interaction,
            palette,
            rect(
                x + w - toggle_w,
                hy + (band - toggle_h) / 2.0,
                toggle_w,
                toggle_h,
            ),
            view.enabled,
            SshViewAction::ToggleTailnet,
        )?;
        Ok(())
    }
}

/// Close the view and open a disconnected remote thread on the main window.
fn open_thread_response(project_id: String) -> ContentViewResponse {
    ContentViewResponse::Run(Box::new(move |tw: &mut TermWindow| {
        let window = tw.window.as_ref().cloned();
        if let Some(window) = window {
            if tw.open_ssh_host_thread_without_connecting(project_id, &window) {
                tw.close_content_view();
            }
        }
    }))
}

impl ContentView for SshHostsView {
    fn title(&self) -> String {
        crate::i18n::tr("ssh-hosts-title")
    }

    fn tab_key(&self) -> Option<String> {
        Some(SSH_HOSTS_CONTENT_VIEW_KEY.to_string())
    }

    fn wants_cursor_blink(&self) -> bool {
        // A text field is always focused while this view is open.
        true
    }

    /// What each way to connect does, and the card introducing Connect, read
    /// as text under the labels rather than as more labels.
    fn typography(&self) -> ContentViewTypography {
        ContentViewTypography::RegularCaption
    }

    fn paint(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
        title_font: &Rc<LoadedFont>,
        section_font: &Rc<LoadedFont>,
        caption_font: &Rc<LoadedFont>,
        cursor_on: bool,
    ) -> anyhow::Result<()> {
        self.paint_impl(
            ctx,
            layers,
            area,
            palette,
            font,
            title_font,
            section_font,
            caption_font,
            cursor_on,
        )
    }

    fn run_remote_host_command(
        &mut self,
        host_id: &str,
        command: RemoteHostCommand,
    ) -> ContentViewResponse {
        self.run_host_command(host_id, command)
    }

    fn on_mouse(&mut self, x: f32, y: f32, kind: WMEK) -> ContentViewResponse {
        let response = self.on_mouse_impl(x, y, kind);
        self.commit_left_tailnet_fields();
        response
    }

    fn on_pointer(&mut self, x: f32, y: f32, event: &MouseEvent) -> ContentViewResponse {
        if let Some(response) = self.drag_inspector_edge(x, y, event) {
            return response;
        }
        self.on_mouse(x, y, event.kind.clone())
    }

    fn cursor(&self, x: f32, y: f32) -> Option<MouseCursor> {
        (self.inspector_drag.is_some() || contains(self.inspector_edge, x, y))
            .then_some(MouseCursor::SizeLeftRight)
    }

    fn holds_pointer(&self) -> bool {
        self.inspector_drag.is_some()
    }

    fn on_wheel(&mut self, x: f32, y: f32, event: &MouseEvent) -> ContentViewResponse {
        let scale = self.last_ui_scale;
        let scroll = if self.point_in_host_list(x, y) {
            &mut self.scroll
        } else if contains(self.form_area, x, y) {
            &mut self.form_scroll
        } else {
            return ContentViewResponse::Ignored;
        };
        if apply_wheel(event, scroll, scale) {
            ContentViewResponse::Redraw
        } else {
            ContentViewResponse::Ignored
        }
    }

    fn next_frame_time(&self) -> Option<Instant> {
        if self.scroll_animating {
            return Some(Instant::now());
        }
        // The tailnet changes elsewhere: the page reads it again every
        // couple of seconds while it shows. And "Copied" goes back.
        let poll = (self.window.is_some() && crate::tailnet::available())
            .then(crate::tailnet::next_poll_at)
            .flatten();
        let copied = self
            .tailnet
            .copied_until
            .filter(|until| *until > Instant::now());
        poll.into_iter().chain(copied).min()
    }

    fn on_key(&mut self, key: KeyCode, mods: KeyModifiers) -> ContentViewResponse {
        let response = self.on_key_impl(key, mods);
        self.commit_left_tailnet_fields();
        response
    }

    fn on_paste(&mut self, text: &str) -> ContentViewResponse {
        self.on_paste_impl(text)
    }

    fn begin_new_remote_host(&mut self) {
        // Same reset the `+` button performs; the list refreshes on paint.
        self.open_new_form();
    }

    fn show_tailscale_panel(&mut self) {
        self.open_tailnet_panel();
    }

    fn copy_text(&self) -> Option<String> {
        // A key typed into the Tailscale panel never leaves it.
        if self.focus == Focus::Tailnet(TAILNET_AUTH_KEY) {
            return None;
        }
        let input = self.focused_input()?;
        let text = input
            .caret_selected_text()
            .unwrap_or_else(|| input.text().to_string());
        (!text.is_empty()).then_some(text)
    }

    fn cut_text(&mut self) -> Option<String> {
        if self.focus == Focus::Tailnet(TAILNET_AUTH_KEY) {
            return None;
        }
        self.take_focused_selection()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        focus_after_collapsing_advanced, ssh_options_from_rows, tab_stops_for, text_input_with,
        ContentViewResponse, Focus, HostForm, RemoteHostCommand, SshHostsView, BASE_FIELD_COUNT,
        FIELD_HOST, FIELD_NAME, FIELD_PORT, FIELD_USER, FIELD_WORKSPACE, TAILNET_AUTH_KEY,
        TAILNET_CONTROL_URL, TAILNET_FIELD_COUNT, TAILNET_HOSTNAME,
    };
    use crate::ssh_hosts::{SshHostEntry, SshHostSource, SshHostSpec, SshNetwork};
    use wezterm_term::{KeyCode, KeyModifiers};

    fn spec(label: &str, host: &str) -> SshHostSpec {
        SshHostSpec {
            label: label.to_string(),
            host: host.to_string(),
            port: Some(2222),
            other_addresses: Vec::new(),
            network: Default::default(),
            username: Some("deploy".to_string()),
            identity_file: Some("~/.ssh/id_ed25519".to_string()),
            password: None,
            ssh_options: Default::default(),
            multiplexing: false,
            default_workspace: None,
            detect_os: true,
            detected_distro: None,
            use_mosh: false,
            mosh_server_command: super::ssh_hosts::DEFAULT_MOSH_SERVER_COMMAND.to_string(),
            shell_integration: false,
            shell_integration_applied: false,
            shell_integration_unsupported: None,
            shell_integration_failure: None,
            files_auto_connect: false,
            icon: None,
        }
    }

    fn wheel(precise_y: Option<f32>, notches: i16) -> window::MouseEvent {
        window::MouseEvent {
            kind: window::MouseEventKind::VertWheel(notches),
            coords: window::Point::new(10, 10),
            screen_coords: window::ScreenPoint::new(10, 10),
            mouse_buttons: window::MouseButtons::NONE,
            modifiers: window::Modifiers::NONE,
            precise_scroll_delta: precise_y.map(|y| window::PreciseScrollDelta { x: 0.0, y }),
            scroll_phase: None,
            momentum_phase: None,
            precise_wheel_lines: None,
        }
    }

    #[test]
    fn the_host_list_follows_a_trackpad_and_glides_on_a_wheel() {
        use super::ContentView;
        let mut view = SshHostsView::new();
        view.list_area = crate::ui::rect(0.0, 0.0, 400.0, 300.0);
        view.scroll.set_extents(300.0, 3000.0);

        // A trackpad reports a few pixels at a time, alongside a wheel amount
        // of 0; read in whole notches, the list would not move at all.
        let response = view.on_wheel(10.0, 10.0, &wheel(Some(-7.5), 0));
        assert!(matches!(response, ContentViewResponse::Redraw));
        assert_eq!(view.scroll.offset, 7.5);

        // A notched wheel sets off a glide instead of jumping a whole step.
        view.on_wheel(10.0, 10.0, &wheel(None, -1));
        assert_eq!(view.scroll.offset, 7.5);
        assert!(view.scroll.velocity > 0.0);
    }

    #[test]
    fn a_failure_shows_only_while_the_change_still_waits() {
        use crate::ssh_hosts::{ShellIntegrationFailure, ShellIntegrationRecord};
        let failed = |failure| ShellIntegrationRecord {
            applied: false,
            unsupported: None,
            failure: Some(failure),
        };
        let unwritable = || ShellIntegrationFailure::Unwritable("~/.zshrc".to_string());

        // Turned on, and the last connection could not write the file.
        let text = SshHostsView::shell_integration_status(true, failed(unwritable()));
        assert!(text.contains("~/.zshrc"), "{}", text);
        let other = ShellIntegrationFailure::Other("Exited with code 2".to_string());
        let text = SshHostsView::shell_integration_status(true, failed(other));
        assert!(text.contains("Exited with code 2"), "{}", text);

        // Turned back off: nothing waits, so nothing has failed.
        let text = SshHostsView::shell_integration_status(false, failed(unwritable()));
        assert!(!text.contains("~/.zshrc"), "{}", text);
    }

    #[test]
    fn duplicate_opens_a_prefilled_new_host_form() {
        let mut view = SshHostsView::new();
        view.filtered = vec![SshHostEntry {
            id: "ssh-original".to_string(),
            source: SshHostSource::ThinkTerm,
            spec: SshHostSpec {
                icon: Some(crate::ssh_hosts::HostIcon::Cloud),
                ..spec("prod-1", "192.0.2.7")
            },
        }];

        view.password_visible = true;
        let response = view.run_host_command("ssh-original", RemoteHostCommand::Duplicate);
        assert!(
            !view.password_visible,
            "a duplicate must mask its stored password"
        );
        assert!(matches!(response, ContentViewResponse::Redraw));

        let form = view
            .form
            .as_ref()
            .expect("Duplicate opens the new-host form; it does not save a record on its own");
        assert!(form.editing.is_none(), "a duplicate is a new host, not an edit");
        assert_eq!(form.fields[FIELD_NAME].text(), "prod-1 copy");
        assert_eq!(form.fields[FIELD_PORT].text(), "2222");
        assert_eq!(form.fields[FIELD_USER].text(), "deploy");
        // A record is keyed by `user@host:port`, so a copy carrying the
        // original's host could never be saved -- it would collide with the
        // host it was copied from. Host is the one field left for the user.
        assert_eq!(form.fields[FIELD_HOST].text(), "");
        assert_eq!(view.focus, Focus::Field(FIELD_HOST));
        assert_eq!(form.icon, Some(crate::ssh_hosts::HostIcon::Cloud));
    }

    fn row(key: &str, value: &str) -> (super::TextInputState, super::TextInputState) {
        (
            text_input_with(key.to_string()),
            text_input_with(value.to_string()),
        )
    }

    #[test]
    fn ssh_options_are_trimmed_and_blank_keys_dropped() {
        let rows = vec![
            row("  ProxyJump ", " user@bastion "),
            row("", "orphaned-value"),
            row("   ", ""),
            row("ServerAliveInterval", "30"),
        ];
        let options = ssh_options_from_rows(&rows);
        assert_eq!(options.len(), 2);
        assert_eq!(
            options.get("ProxyJump").map(String::as_str),
            Some("user@bastion")
        );
        assert_eq!(
            options.get("ServerAliveInterval").map(String::as_str),
            Some("30")
        );
    }

    #[test]
    fn a_present_key_with_an_empty_value_is_kept() {
        // `Option=` is meaningful to ssh_config; only a blank *key* is noise.
        let options = ssh_options_from_rows(&[row("Compression", "")]);
        assert_eq!(options.get("Compression").map(String::as_str), Some(""));
    }

    #[test]
    fn collapsing_advanced_pulls_focus_out_of_hidden_rows() {
        // Rows inside Advanced stop being drawn, so focus must not stay on one:
        // `focused_input_mut` would otherwise keep routing keystrokes into an
        // invisible field.
        assert_eq!(
            focus_after_collapsing_advanced(Focus::OptionKey(2)),
            Focus::Field(FIELD_HOST)
        );
        assert_eq!(
            focus_after_collapsing_advanced(Focus::OptionValue(0)),
            Focus::Field(FIELD_HOST)
        );
        // Anything already visible keeps its place.
        assert_eq!(
            focus_after_collapsing_advanced(Focus::Field(FIELD_WORKSPACE)),
            Focus::Field(FIELD_WORKSPACE)
        );
        assert_eq!(
            focus_after_collapsing_advanced(Focus::Search),
            Focus::Search
        );
    }

    #[test]
    fn hidden_option_rows_are_not_reachable_for_editing() {
        let mut form = HostForm::default();
        form.options = vec![row("ProxyJump", "bastion")];
        form.advanced_open = false;
        // Tab order is the visible surface; a collapsed section contributes
        // nothing, which is the same invariant `focused_input_mut` enforces.
        assert_eq!(tab_stops_for(&form).len(), BASE_FIELD_COUNT);
    }

    fn tailnet_machine(name: &str, address: &str) -> crate::tailnet::Machine {
        crate::tailnet::Machine {
            name: name.to_string(),
            target: format!("{name}.example.ts.net"),
            fqdn: format!("{name}.example.ts.net"),
            addresses: vec![address.parse().unwrap()],
            online: Some(true),
            ..Default::default()
        }
    }

    #[test]
    fn a_machine_saved_as_a_host_opens_like_that_host_s_card() {
        let mut view = SshHostsView::new();
        view.saved_hosts = vec![SshHostEntry {
            id: "ssh-saved".to_string(),
            source: SshHostSource::ThinkTerm,
            spec: spec("server-b", "100.64.0.7"),
        }];
        view.tailnet.machines = vec![
            tailnet_machine("server-b", "100.64.0.7"),
            tailnet_machine("build-box", "100.64.0.8"),
        ];
        assert!(view.saved_host_for(&view.tailnet.machines[0]).is_none());
        view.saved_hosts[0].spec.network = SshNetwork::Tailnet;
        assert!(view.saved_host_for(&view.tailnet.machines[0]).is_some());
        assert!(view.saved_host_for(&view.tailnet.machines[1]).is_none());

        // Saved: selected, its form in the inspector, as a host card's first
        // click does; connecting takes a second.
        view.selection_active = true;
        assert!(matches!(
            view.open_tailnet_machine(0),
            ContentViewResponse::Redraw
        ));
        let form = view.form.as_ref().expect("the saved host's form");
        assert_eq!(form.editing.as_deref(), Some("ssh-saved"));
        assert_eq!(form.fields[FIELD_HOST].text(), "100.64.0.7");
        assert_eq!(view.tailnet.selected.as_deref(), Some("ssh-saved"));
        assert!(!view.selection_active);
        // Clicked again, what was typed into its form stays.
        view.form.as_mut().unwrap().fields[FIELD_USER].set_text_end("typed".to_string());
        view.open_tailnet_machine(0);
        assert_eq!(view.form.as_ref().unwrap().fields[FIELD_USER].text(), "typed");

        // Not saved: a new host, its name and address filled in, waiting
        // for whom to sign in as.
        assert!(matches!(
            view.open_tailnet_machine(1),
            ContentViewResponse::Redraw
        ));
        assert!(view.tailnet.selected.is_none());
        let form = view.form.as_ref().expect("a new host's form");
        assert!(form.editing.is_none());
        assert_eq!(form.network, SshNetwork::Tailnet);
        assert_eq!(form.fields[FIELD_NAME].text(), "build-box");
        assert_eq!(form.fields[FIELD_HOST].text(), "build-box.example.ts.net");
        assert_eq!(view.focus, Focus::Field(FIELD_USER));

        // A host that reaches it by one of its other addresses is it too.
        let mut devbox = spec("devbox", "192.0.2.5");
        devbox.network = SshNetwork::Tailnet;
        devbox.other_addresses = vec!["build-box".to_string()];
        view.saved_hosts.push(SshHostEntry {
            id: "ssh-devbox".to_string(),
            source: SshHostSource::ThinkTerm,
            spec: devbox,
        });
        assert_eq!(
            view.saved_host_for(&view.tailnet.machines[1])
                .map(|entry| entry.id.as_str()),
            Some("ssh-devbox")
        );
    }

    #[test]
    fn a_new_host_uses_the_system_until_its_network_is_selected() {
        let mut view = SshHostsView::new();
        view.open_new_form();
        assert_eq!(view.form.as_ref().unwrap().network, SshNetwork::System);
        // Greyed out while Tailscale is off.
        view.apply(super::SshViewAction::SetNetwork(SshNetwork::Tailnet));
        assert_eq!(view.form.as_ref().unwrap().network, SshNetwork::System);
        view.tailnet_on = true;
        view.apply(super::SshViewAction::SetNetwork(SshNetwork::Tailnet));
        assert_eq!(view.form.as_ref().unwrap().network, SshNetwork::Tailnet);
        view.apply(super::SshViewAction::SetMode(super::HostMode::Mosh));
        let form = view.form.as_ref().unwrap();
        assert!(!form.use_mosh);
        assert!(form.error.is_some());
        view.apply(super::SshViewAction::SetNetwork(SshNetwork::System));
        view.apply(super::SshViewAction::SetMode(super::HostMode::Mosh));
        assert!(view.form.as_ref().unwrap().use_mosh);
        view.apply(super::SshViewAction::SetNetwork(SshNetwork::Tailnet));
        assert_eq!(view.form.as_ref().unwrap().network, SshNetwork::System);
    }

    #[test]
    fn a_new_host_connects_with_thinkterm_until_told_otherwise() {
        use super::HostMode;
        let mut view = SshHostsView::new();
        view.open_new_form();
        let form = view.form.as_ref().unwrap();
        assert_eq!(HostMode::of(form.use_mosh, form.multiplexing), HostMode::Connect);
        assert!(form.files_auto_connect && form.shell_integration && form.detect_os);
        for mode in [HostMode::Ssh, HostMode::Mosh, HostMode::Connect] {
            view.apply(super::SshViewAction::SetMode(mode));
            let form = view.form.as_ref().unwrap();
            assert_eq!(HostMode::of(form.use_mosh, form.multiplexing), mode);
            assert!(!(form.use_mosh && form.multiplexing));
        }
    }

    #[test]
    fn a_host_from_ssh_config_is_added_on_connect() {
        use super::HostMode;
        let mut view = SshHostsView::new();
        let mut system = spec("staging", "198.51.100.20");
        system.username = Some("user".into());
        system.port = Some(2222);
        view.open_connect_form_from(&system);
        let form = view.form.as_ref().unwrap();
        assert!(form.editing.is_none());
        assert_eq!(form.fields[super::FIELD_NAME].text(), "staging");
        assert_eq!(form.fields[super::FIELD_HOST].text(), "198.51.100.20");
        assert_eq!(form.fields[super::FIELD_USER].text(), "user");
        assert_eq!(form.fields[super::FIELD_PORT].text(), "2222");
        assert_eq!(HostMode::of(form.use_mosh, form.multiplexing), HostMode::Connect);
    }

    #[test]
    fn a_host_from_ssh_config_without_a_port_leaves_it_to_the_config() {
        let mut view = SshHostsView::new();
        let mut system = spec("backup", "backup.example");
        system.port = None;
        view.filtered = vec![SshHostEntry {
            id: "system-backup".to_string(),
            source: SshHostSource::System,
            spec: system,
        }];
        view.selection_active = true;
        view.apply(super::SshViewAction::AddWithConnect(0));
        let form = view.form.as_ref().unwrap();
        assert_eq!(form.fields[super::FIELD_HOST].text(), "backup.example");
        assert_eq!(form.fields[super::FIELD_PORT].text(), "");
        assert!(!view.selection_active);
    }

    #[test]
    fn choosing_a_mode_clears_the_error_of_the_last_one() {
        use super::HostMode;
        let mut view = SshHostsView::new();
        view.open_new_form();
        view.form.as_mut().unwrap().network = SshNetwork::Tailnet;
        view.apply(super::SshViewAction::SetMode(HostMode::Mosh));
        let form = view.form.as_ref().unwrap();
        assert!(form.error.is_some());
        assert_eq!(HostMode::of(form.use_mosh, form.multiplexing), HostMode::Connect);
        view.apply(super::SshViewAction::SetMode(HostMode::Ssh));
        assert!(view.form.as_ref().unwrap().error.is_none());
    }

    #[test]
    fn a_choice_is_made_on_the_saved_host_as_save_makes_it() {
        use super::{HostChoice, HostMode};
        let saved = spec("server-a", "192.0.2.10");

        let mut moved = saved.clone();
        HostChoice::Network(SshNetwork::Tailnet).apply_to(&mut moved);
        assert_eq!(moved.network, SshNetwork::Tailnet);
        // Nothing else of the host goes with it.
        moved.network = saved.network;
        assert_eq!(moved, saved);

        let mut mosh = saved.clone();
        mosh.multiplexing = true;
        mosh.mosh_server_command = String::new();
        HostChoice::Mode(HostMode::Mosh).apply_to(&mut mosh);
        assert_eq!(
            HostMode::of(mosh.use_mosh, mosh.multiplexing),
            HostMode::Mosh
        );
        // Never on Mosh without a server command to start.
        assert_eq!(
            mosh.mosh_server_command,
            super::ssh_hosts::DEFAULT_MOSH_SERVER_COMMAND
        );
        HostChoice::Mode(HostMode::Connect).apply_to(&mut mosh);
        assert_eq!(
            HostMode::of(mosh.use_mosh, mosh.multiplexing),
            HostMode::Connect
        );

        let mut toggled = saved.clone();
        HostChoice::DetectOs(false).apply_to(&mut toggled);
        HostChoice::ShellIntegration(true).apply_to(&mut toggled);
        HostChoice::FilesAutoConnect(true).apply_to(&mut toggled);
        assert!(!toggled.detect_os && toggled.shell_integration && toggled.files_auto_connect);
    }

    #[test]
    fn a_choice_that_could_not_be_saved_shows_what_is_saved() {
        use super::{HostChoice, HostMode};
        let saved = spec("server-a", "192.0.2.10");
        let mut form = HostForm::default();
        form.network = SshNetwork::Tailnet;
        form.use_mosh = true;
        form.detect_os = false;
        HostChoice::Network(SshNetwork::Tailnet).show_saved(&saved, &mut form);
        HostChoice::Mode(HostMode::Mosh).show_saved(&saved, &mut form);
        HostChoice::DetectOs(false).show_saved(&saved, &mut form);
        assert_eq!(form.network, SshNetwork::System);
        assert_eq!(
            HostMode::of(form.use_mosh, form.multiplexing),
            HostMode::Ssh
        );
        assert!(form.detect_os);
    }

    #[test]
    fn a_host_moved_off_the_tailnet_takes_the_ring_to_its_card() {
        let mut view = SshHostsView::new();
        let entry = |id: &str, label: &str| SshHostEntry {
            id: id.to_string(),
            source: SshHostSource::ThinkTerm,
            spec: spec(label, "100.64.0.7"),
        };
        view.filtered = vec![
            entry("ssh-other", "server-a"),
            entry("ssh-saved", "server-b"),
        ];
        view.selection_active = false;
        view.tailnet.selected = Some("ssh-saved".to_string());

        // Still on the tailnet: its machine keeps the ring.
        view.keep_ring_on("ssh-saved", "ssh-saved", true);
        assert_eq!(view.tailnet.selected.as_deref(), Some("ssh-saved"));
        assert!(!view.selection_active);

        // Off it, keyed anew: its card takes the ring.
        view.keep_ring_on("ssh-saved", "ssh-moved", false);
        assert!(view.tailnet.selected.is_none());
        assert!(view.selection_active);
        assert_eq!(view.selected, 1);

        // A host no machine had the ring for leaves the rings as they are.
        view.selection_active = false;
        view.keep_ring_on("ssh-other", "ssh-moved", false);
        assert!(!view.selection_active);
    }

    #[test]
    fn the_tailscale_panel_and_a_host_form_take_turns() {
        use super::ContentView;
        if !crate::tailnet::available() {
            return;
        }
        let mut view = SshHostsView::new();
        view.open_new_form();
        view.show_tailscale_panel();
        assert!(view.tailnet.panel_open);
        assert!(view.form.is_none());
        view.open_new_form();
        assert!(!view.tailnet.panel_open);
        assert!(view.form.is_some());

        view.show_tailscale_panel();
        view.focus = Focus::Tailnet(TAILNET_HOSTNAME);
        // Escape closes the panel, not the page.
        assert!(matches!(
            view.on_key(KeyCode::Escape, KeyModifiers::NONE),
            ContentViewResponse::Redraw
        ));
        assert!(!view.tailnet.panel_open);
        assert_eq!(view.focus, Focus::Search);
    }

    #[test]
    fn a_tailscale_field_goes_out_only_once_typed_into() {
        if !crate::tailnet::available() {
            return;
        }
        let mut view = SshHostsView::new();
        view.open_tailnet_panel();
        view.focus = Focus::Tailnet(TAILNET_CONTROL_URL);
        // Moving the caret through it is no edit.
        view.edit_focused(|input| input.caret_move_home(false));
        assert_eq!(view.tailnet.dirty, [false; TAILNET_FIELD_COUNT]);
        view.edit_focused(|input| input.caret_insert("https://headscale.example", false));
        assert!(view.tailnet.dirty[TAILNET_CONTROL_URL]);
        // Escape leaves without sending it.
        use super::ContentView;
        view.on_key(KeyCode::Escape, KeyModifiers::NONE);
        assert!(!view.tailnet.panel_open);
        assert_eq!(view.tailnet.dirty, [false; TAILNET_FIELD_COUNT]);
        assert_eq!(view.tailnet.changes, [None; TAILNET_FIELD_COUNT]);
    }

    #[test]
    fn an_auth_key_typed_into_the_panel_is_never_copied() {
        use super::ContentView;
        if !crate::tailnet::available() {
            return;
        }
        let mut view = SshHostsView::new();
        view.open_tailnet_panel();
        view.focus = Focus::Tailnet(TAILNET_AUTH_KEY);
        view.tailnet.fields[TAILNET_AUTH_KEY].set_text_end("tskey-auth-example".to_string());
        view.tailnet.fields[TAILNET_AUTH_KEY].caret_select_all();
        assert_eq!(view.copy_text(), None);
        assert_eq!(view.cut_text(), None);
        assert_eq!(
            view.tailnet.fields[TAILNET_AUTH_KEY].text(),
            "tskey-auth-example"
        );
        // The other fields copy as any field does.
        view.focus = Focus::Tailnet(TAILNET_HOSTNAME);
        view.tailnet.fields[TAILNET_HOSTNAME].set_text_end("devbox".to_string());
        assert_eq!(view.copy_text().as_deref(), Some("devbox"));
    }

    #[test]
    fn tab_order_covers_option_rows_only_while_advanced_is_open() {
        let mut form = HostForm::default();
        form.options = vec![row("A", "1"), row("B", "2")];

        form.advanced_open = false;
        assert_eq!(tab_stops_for(&form).len(), BASE_FIELD_COUNT);

        form.advanced_open = true;
        let stops = tab_stops_for(&form);
        assert_eq!(stops.len(), BASE_FIELD_COUNT + 4);
        assert_eq!(stops[BASE_FIELD_COUNT], Focus::OptionKey(0));
        assert_eq!(stops[BASE_FIELD_COUNT + 1], Focus::OptionValue(0));
        assert_eq!(stops[BASE_FIELD_COUNT + 3], Focus::OptionValue(1));
    }
}
