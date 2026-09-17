//! In-window remote hosts manager: a full-content-area "page" (left host list
//! + right edit form) that replaces the terminal area when toggled from the
//! sidebar globe button. Named "Remote Hosts" in the UI because it also
//! covers Mosh and ThinkTerm Connect; the code stays `ssh_hosts` since every
//! mode still dials out over SSH. Draws with the shared `ui::draw`/`ui::widgets`
//! infrastructure. The view owns its state + hit-testing and exposes a tiny
//! [`SshViewOutcome`] so `TermWindow` integration stays minimal.

use crate::quad::{HeapQuadAllocator, QuadClipRect, TripleLayerQuadAllocator};
use crate::ssh_hosts::{self, SshHostEntry, SshHostSource, SshHostSpec};
use crate::termwindow::content_view::{ContentView, ContentViewResponse, RemoteHostCommand};
use crate::termwindow::ui::icons::{distro_to_icon, SvgIcon};
use crate::termwindow::TermWindow;
use crate::ui::anim::Easing;
use crate::ui::{
    card_grid, card_is_warm, card_rect, char_index_for_x, contains, draw_button,
    draw_button_on_layer, draw_icon_button, draw_scrollbar, draw_text_input,
    draw_text_input_on_layer, draw_toggle, rect, row_visible, shared_grid_columns,
    wheel_delta_pixels, ButtonSpec, ButtonVariant, CardGrid, ControlState, DrawContext,
    EditModifiers, InputCaret, InteractionState, RowAlign, ScrollState, TextInputSpec,
    TextInputState, UiContext, UiPalette, UiTokens, WidgetKind,
};
use crate::utilsprites::RenderMetrics;
use crate::workspace_threads;
use fluent_bundle::FluentArgs;
use std::rc::Rc;
use wezterm_font::LoadedFont;
use wezterm_term::{KeyCode, KeyModifiers};
use window::color::LinearRgba;
use window::{MouseEventKind as WMEK, MousePress, RectF};

pub(crate) const SSH_HOSTS_CONTENT_VIEW_KEY: &str = "ssh-hosts";

const FIELD_NAME: usize = 0;
const FIELD_HOST: usize = 1;
const FIELD_PORT: usize = 2;
const FIELD_USER: usize = 3;
const FIELD_PASSWORD: usize = 4;
const FIELD_IDENTITY: usize = 5;
const FIELD_WORKSPACE: usize = 6;
const FIELD_MOSH_SERVER: usize = 7;
const BASE_FIELD_COUNT: usize = 7;
const FIELD_COUNT: usize = 8;

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

const PAD: f32 = 24.0;
const TOOLBAR_H: f32 = 54.0;
const TOOLBAR_GAP: f32 = 20.0;
/// The inspector never grows with the window. A form wider than this is
/// harder to read, not easier, so every extra pixel goes to the grid --
/// which is the half that actually has more to show.
const INSPECTOR_W: f32 = 480.0;
const INSPECTOR_GAP: f32 = 20.0;
const CARD_MIN_W: f32 = 340.0;
/// How wide a card may grow before the grid adds another column. Generous
/// on purpose: cap it too low and a full row ends up narrower than the page,
/// which is how a grid ends up with a margin it never asked for.
const CARD_MAX_W: f32 = 560.0;
const CARD_GAP: f32 = 18.0;
const CARD_RADIUS: f32 = 36.0;
const CARD_PAD: f32 = 22.0;
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

/// Clickable targets inside the view. `usize` payloads index into the current
/// filtered host list, so the action type stays `Copy` (needed by `UiContext`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SshViewAction {
    Search,
    /// A host card: first click selects it, a second one connects.
    Card(usize),
    New,
    /// Connect to the host the inspector is showing.
    Connect,
    FocusField(usize),
    ToggleDetect,
    ToggleMosh,
    ToggleMux,
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
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Focus {
    Search,
    Field(usize),
    /// `ssh_options` row `n`, key column.
    OptionKey(usize),
    /// `ssh_options` row `n`, value column.
    OptionValue(usize),
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
    detect_os: bool,
    use_mosh: bool,
    multiplexing: bool,
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
    widgets: UiContext<SshViewAction>,
    interaction: InteractionState<SshViewAction>,
    scroll: ScrollState,
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
            list_area: rect(0.0, 0.0, 0.0, 0.0),
            card_rects: Vec::new(),
            password_visible: false,
            last_click: None,
            selection_active: false,
            pending_caret_click: None,
            form_scroll: ScrollState::new(),
            form_content_h: 0.0,
            form_area: rect(0.0, 0.0, 0.0, 0.0),
            widgets: UiContext::default(),
            interaction: InteractionState::default(),
            scroll: ScrollState::new(),
        };
        view.refresh();
        view
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
        self.filtered = ssh_hosts::list_all_hosts()
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
        let mut form = HostForm::default();
        form.fields[FIELD_PORT].set_text_end("22".to_string());
        form.fields[FIELD_MOSH_SERVER]
            .set_text_end(ssh_hosts::DEFAULT_MOSH_SERVER_COMMAND.to_string());
        form.detect_os = true;
        self.form = Some(form);
        self.focus = Focus::Field(FIELD_HOST);
        self.reset_form_scroll();
    }

    fn open_edit_form(&mut self, project_id: &str) {
        if ssh_hosts::is_system_host_id(project_id) {
            return;
        }
        let Some(spec) = ssh_hosts::host_spec(project_id) else {
            return;
        };
        // Revealing a password is a decision about one host. Carrying it into
        // the next one would put a stored password on screen unasked.
        self.password_visible = false;
        let mut fields: [String; FIELD_COUNT] = Default::default();
        fields[FIELD_NAME] = spec.label.clone();
        fields[FIELD_HOST] = spec.host.clone();
        fields[FIELD_PORT] = spec.port.map(|p| p.to_string()).unwrap_or_default();
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
        let detect_os = spec.detect_os;
        let use_mosh = spec.use_mosh;
        let multiplexing = spec.multiplexing;
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
            detect_os,
            use_mosh,
            multiplexing,
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
        });
        spec.label = label;
        spec.host = host;
        spec.port = port;
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
            Some(id) => match ssh_hosts::try_update_host(id, spec) {
                // Not `id`: moving the host to another endpoint re-keys the
                // record, and the caller selects what comes back.
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

    // ---- input ------------------------------------------------------------

    fn on_mouse_impl(&mut self, x: f32, y: f32, kind: WMEK) -> ContentViewResponse {
        let hit = self.widgets.hit_test(x, y).map(|t| t.action);
        match kind {
            WMEK::VertWheel(amount) => {
                let scroll = if self.point_in_host_list(x, y) {
                    Some(&mut self.scroll)
                } else if contains(self.form_area, x, y) {
                    Some(&mut self.form_scroll)
                } else {
                    None
                };
                let Some(scroll) = scroll else {
                    return ContentViewResponse::Ignored;
                };
                let old = scroll.offset;
                scroll.scroll_by(wheel_delta_pixels(amount, self.last_ui_scale));
                if (scroll.offset - old).abs() > 0.01 {
                    ContentViewResponse::Redraw
                } else {
                    ContentViewResponse::Ignored
                }
            }
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
                        if let SshViewAction::Card(index) = a {
                            if let Some(entry) = self.filtered.get(index) {
                                self.last_click =
                                    Some((entry.id.clone(), x, y, std::time::Instant::now()));
                            }
                        }
                        self.pending_caret_click = match a {
                            SshViewAction::Search => Some((Focus::Search, x)),
                            SshViewAction::FocusField(n) => Some((Focus::Field(n), x)),
                            SshViewAction::FocusOptionKey(n) => Some((Focus::OptionKey(n), x)),
                            SshViewAction::FocusOptionValue(n) => Some((Focus::OptionValue(n), x)),
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
            SshViewAction::New => {
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
            SshViewAction::ToggleDetect => {
                if let Some(form) = self.form.as_mut() {
                    form.detect_os = !form.detect_os;
                }
                ContentViewResponse::Redraw
            }
            SshViewAction::ToggleMosh => {
                if let Some(form) = self.form.as_mut() {
                    form.use_mosh = !form.use_mosh;
                    if form.use_mosh {
                        // Mosh and ThinkTerm Connect are alternative
                        // transports; only one can drive the connection.
                        form.multiplexing = false;
                    }
                    if form.use_mosh && form.fields[FIELD_MOSH_SERVER].text().trim().is_empty() {
                        form.fields[FIELD_MOSH_SERVER]
                            .set_text_end(ssh_hosts::DEFAULT_MOSH_SERVER_COMMAND.to_string());
                    }
                    if !form.use_mosh && self.focus == Focus::Field(FIELD_MOSH_SERVER) {
                        self.focus = Focus::Field(FIELD_WORKSPACE);
                    }
                }
                ContentViewResponse::Redraw
            }
            SshViewAction::ToggleMux => {
                if let Some(form) = self.form.as_mut() {
                    form.multiplexing = !form.multiplexing;
                    if form.multiplexing {
                        form.use_mosh = false;
                        if self.focus == Focus::Field(FIELD_MOSH_SERVER) {
                            self.focus = Focus::Field(FIELD_WORKSPACE);
                        }
                    }
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
        }
    }

    /// Show a host in the inspector. A host that came from `~/.ssh/config`
    /// has no record of ours to edit, so selecting one only highlights it.
    fn select_host(&mut self, index: usize) {
        self.selected = index;
        self.selection_active = true;
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
        }
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
        }
    }

    /// Run `f` against the focused field, then re-filter the host list if the
    /// search box was the one edited.
    fn edit_focused(&mut self, f: impl FnOnce(&mut TextInputState)) {
        let Some(input) = self.focused_input_mut() else {
            return;
        };
        f(input);
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

    /// What a card says under the host name: where it connects, and -- only
    /// when it is not plain SSH -- how.
    ///
    /// Spelled out after the endpoint rather than shown as a corner tag: a
    /// two-word tag next to the title has to be read as a label anyway, and
    /// "connect" as a tag collides with the verb.
    fn card_subtitle(entry: &SshHostEntry) -> String {
        let endpoint = ssh_hosts::endpoint(&entry.spec);
        let mode = if entry.spec.use_mosh {
            Some(crate::i18n::tr("ssh-mode-mosh"))
        } else if entry.spec.multiplexing {
            Some(crate::i18n::tr("ssh-mode-connect"))
        } else {
            None
        };
        match mode {
            Some(mode) => format!("{endpoint}  ·  {mode}"),
            None => endpoint,
        }
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
        cursor_on: bool,
    ) -> anyhow::Result<()> {
        let tokens = UiTokens::for_dpi(ctx.dimensions.dpi);
        self.refresh();
        self.widgets.clear();
        self.card_rects.clear();

        // Headings need their own context: text sits on the baseline of the
        // metrics the *context* carries, not of the font handed to draw_text,
        // so a larger font drawn through the body context lands on the body
        // baseline and looks a few pixels high.
        let title_metrics = RenderMetrics::with_font_metrics(&title_font.metrics());
        let section_metrics = RenderMetrics::with_font_metrics(&section_font.metrics());
        let title_ctx = ctx.with_metrics(&title_metrics);
        let section_ctx = ctx.with_metrics(&section_metrics);

        let ox = area.origin.x;
        let oy = area.origin.y;
        let w = area.size.width;
        let h = area.size.height;
        self.last_ui_scale = ctx.px(1.0).max(0.01);

        ctx.draw_rect(layers, 0, ox, oy, w, h, palette.window_bg)?;

        let pad = ctx.px(PAD);
        // The inspector takes its fixed width off the top and the grid lays
        // out in what is left. That is the whole responsive story: widen the
        // window and the leftover width becomes more columns, not a wider
        // form floating in the middle of the page.
        let inspector_w = if self.form.is_some() {
            ctx.px(INSPECTOR_W).min(((w - pad * 2.0) * 0.6).max(0.0))
        } else {
            0.0
        };
        let inspector_gap = if inspector_w > 0.0 {
            ctx.px(INSPECTOR_GAP)
        } else {
            0.0
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
            layers,
            font,
            section_font,
            palette,
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

        if inspector_w > 0.0 {
            let inspector = rect(
                ox + w - pad - inspector_w,
                oy + pad,
                inspector_w,
                (h - pad * 2.0).max(0.0),
            );
            self.paint_inspector(
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
            )?;
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
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        section_font: &Rc<LoadedFont>,
        palette: UiPalette,
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
        if self.filtered.is_empty() && self.system_host_count == 0 {
            self.scroll.set_extents(viewport.size.height, 0.0);
            return self.paint_empty_state(ctx, layers, font, palette, viewport);
        }

        let (own, system) = self.grouped_indices();
        // A lone "Hosts" caption over the only group on the page is noise;
        // the captions earn their place once there is a second group to tell
        // apart from the first.
        let grouped = self.system_host_count > 0;
        let counts = [own.len(), system.len()];
        let columns = Self::card_columns(ctx, &counts, content_w);
        let gap = ctx.px(CARD_GAP);
        let card_h = Self::card_height(ctx);
        let header_h = ctx.px(GROUP_HEADER_H);
        let max_w = ctx.px(CARD_MAX_W);

        let own_grid = card_grid(own.len(), content_w, columns, max_w, gap, card_h);
        let system_grid = card_grid(system.len(), content_w, columns, max_w, gap, card_h);

        let title_gap = ctx.px(GROUP_TITLE_GAP);
        let mut content_h = own_grid.height(gap);
        if grouped {
            content_h += (header_h + title_gap) * 2.0 + ctx.px(GROUP_GAP) + system_grid.height(gap);
        }
        self.scroll.set_extents(viewport.size.height, content_h);

        let mut y = list_top - self.scroll.offset;
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
                viewport,
            )?;
            y += header_h + title_gap;
        }
        self.paint_cards(
            ctx, layers, font, palette, &own, own_grid, content_x, content_w, y, gap, viewport,
        )?;
        y += own_grid.height(gap);

        if grouped {
            y += ctx.px(GROUP_GAP);
            let mut args = FluentArgs::new();
            args.set("count", self.system_host_count);
            let label = crate::i18n::tr_args("ssh-system-hosts", &args);
            self.paint_group_header(
                section_ctx,
                layers,
                section_font,
                palette,
                &label,
                Some(SshViewAction::ToggleSystemHosts),
                content_x,
                y,
                content_w,
                viewport,
            )?;
            y += header_h + title_gap;
            self.paint_cards(
                ctx,
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
            self.paint_host_card(ctx, layers, font, palette, index, card, viewport)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_host_card(
        &mut self,
        ctx: &DrawContext,
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
        // Selection is a ring, not a wash: the card already carries a logo
        // and two lines of text, and lightening it as well says the same
        // thing twice.
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
                ctx.px(CARD_RADIUS) - 1.0,
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
        let stack = line_h * 2.0 + ctx.px(4.0);
        let title_y = card.origin.y + ((card.size.height - stack) / 2.0).max(ctx.px(8.0));
        ctx.draw_text(
            layers,
            font,
            text_x,
            title_y,
            &entry.spec.label,
            palette.text,
            text_w,
        )?;
        ctx.draw_text(
            layers,
            font,
            text_x,
            title_y + line_h + ctx.px(4.0),
            &Self::card_subtitle(&entry),
            palette.muted_text,
            text_w,
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_group_header(
        &mut self,
        section_ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        section_font: &Rc<LoadedFont>,
        palette: UiPalette,
        label: &str,
        toggle: Option<SshViewAction>,
        x: f32,
        y: f32,
        width: f32,
        viewport: RectF,
    ) -> anyhow::Result<()> {
        let height = section_ctx.px(GROUP_HEADER_H);
        if !row_visible(y, height, viewport) {
            return Ok(());
        }
        let line_h = section_ctx.metrics.cell_size.height as f32;
        let mut text_x = x;
        if let Some(action) = toggle {
            let hit_h = (y + height).min(viewport.max_y()) - y.max(viewport.min_y());
            self.widgets.push(
                rect(x, y.max(viewport.min_y()), width, hit_h.max(0.0)),
                WidgetKind::Button,
                action,
            );
            let chevron = section_ctx.px(18.0);
            let icon = if self.system_group_expanded() {
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
            (width - (text_x - x)).max(0.0),
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
        let line_h = ctx.metrics.cell_size.height as f32;
        ctx.draw_text(layers, font, x, y, label, palette.muted_text, width)?;
        let input_y = y + line_h + ctx.px(LABEL_GAP);
        let text_pad = ctx.px(12.0);
        self.resolve_pending_caret_click(ctx, font, Focus::Field(index), shown, x + text_pad);
        let caret = self.caret_for(Focus::Field(index));
        let focused = self.focus == Focus::Field(index);
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
                placeholder: "",
                text: shown,
                rect: rect(x, input_y, width, ctx.px(INPUT_H)),
                focused,
                selected_all: false,
                action: SshViewAction::FocusField(index),
            },
            caret,
        )?;
        Ok(input_y + ctx.px(INPUT_H))
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

    #[allow(clippy::too_many_arguments)]
    fn paint_inspector(
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
    ) -> anyhow::Result<()> {
        let Some(form) = self.form.as_ref() else {
            return Ok(());
        };
        let editing = form.editing.clone();
        let detect_os = form.detect_os;
        let use_mosh = form.use_mosh;
        let multiplexing = form.multiplexing;
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

        let body_top = area.origin.y + header_h;
        let body_height = (area.max_y() - body_top).max(0.0);
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
            let conn_h = Self::inspector_card_height(ctx, section_h, &[field_h, field_h, field_h]);
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
            let mut session_rows = vec![field_h, toggle_h, toggle_h, toggle_h];
            if use_mosh {
                session_rows.push(field_h);
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
                &crate::i18n::tr("ssh-use-mosh"),
                use_mosh,
                SshViewAction::ToggleMosh,
                x + pad,
                fy,
                inner,
            )? + gap;
            fy = self.paint_toggle_row(
                ctx,
                &mut body_layers,
                font,
                palette,
                &crate::i18n::tr("ssh-use-mux"),
                multiplexing,
                SshViewAction::ToggleMux,
                x + pad,
                fy,
                inner,
            )? + gap;
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

            // ---- footer. Creating a host offers "Save & Open" as the primary
            // action, since there is no Connect button above yet.
            let button_gap = ctx.px(10.0);
            let mut buttons: Vec<(String, SshViewAction, ButtonVariant)> = Vec::new();
            if editing.is_none() {
                buttons.push((
                    crate::i18n::tr("ssh-save-open"),
                    SshViewAction::SaveAndConnect,
                    ButtonVariant::Primary,
                ));
                buttons.push((
                    crate::i18n::tr("ssh-save"),
                    SshViewAction::Save,
                    ButtonVariant::Secondary,
                ));
            } else {
                buttons.push((
                    crate::i18n::tr("ssh-save"),
                    SshViewAction::Save,
                    ButtonVariant::Primary,
                ));
            }
            buttons.push((
                crate::i18n::tr("ssh-cancel"),
                SshViewAction::Cancel,
                ButtonVariant::Secondary,
            ));
            // The column is narrow and fixed. Rather than squeeze the labels
            // until they ellipsize -- which is how a button stops saying what it
            // does -- lay them out at their natural width while they fit, and
            // stack them full-width when they do not.
            let natural: Vec<f32> = buttons
                .iter()
                .map(|(label, _, _)| ctx.measure_text_width(font, label) + ctx.px(56.0))
                .collect();
            let row_fits = natural.iter().sum::<f32>()
                + button_gap * buttons.len().saturating_sub(1) as f32
                <= w;
            let mut bx = x;
            for ((label, action, variant), natural_w) in buttons.iter().zip(natural.iter()) {
                let each = if row_fits { *natural_w } else { w };
                let state = self.button_state(*action);
                draw_button(
                    ctx,
                    &mut body_layers,
                    font,
                    &mut self.widgets,
                    palette,
                    ButtonSpec {
                        label,
                        action: *action,
                        rect: rect(bx, y, each, ctx.px(BTN_H)),
                        state,
                        kind: WidgetKind::Button,
                        variant: *variant,
                    },
                )?;
                if row_fits {
                    bx += each + button_gap;
                } else {
                    y += ctx.px(BTN_H) + ctx.px(8.0);
                }
            }
            // A stacked column already advanced past its last button.
            if row_fits {
                y += ctx.px(BTN_H);
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

    fn paint(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
        title_font: &Rc<LoadedFont>,
        section_font: &Rc<LoadedFont>,
        _caption_font: &Rc<LoadedFont>,
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
        self.on_mouse_impl(x, y, kind)
    }

    fn on_key(&mut self, key: KeyCode, mods: KeyModifiers) -> ContentViewResponse {
        self.on_key_impl(key, mods)
    }

    fn on_paste(&mut self, text: &str) -> ContentViewResponse {
        self.on_paste_impl(text)
    }

    fn begin_new_remote_host(&mut self) {
        // Same reset the `+` button performs; the list refreshes on paint.
        self.open_new_form();
    }

    fn copy_text(&self) -> Option<String> {
        let input = self.focused_input()?;
        let text = input
            .caret_selected_text()
            .unwrap_or_else(|| input.text().to_string());
        (!text.is_empty()).then_some(text)
    }

    fn cut_text(&mut self) -> Option<String> {
        self.take_focused_selection()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        focus_after_collapsing_advanced, ssh_options_from_rows, tab_stops_for, text_input_with,
        ContentViewResponse, Focus, HostForm, RemoteHostCommand, SshHostsView, BASE_FIELD_COUNT,
        FIELD_HOST, FIELD_NAME, FIELD_PORT, FIELD_USER, FIELD_WORKSPACE,
    };
    use crate::ssh_hosts::{SshHostEntry, SshHostSource, SshHostSpec};

    fn spec(label: &str, host: &str) -> SshHostSpec {
        SshHostSpec {
            label: label.to_string(),
            host: host.to_string(),
            port: Some(2222),
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
        }
    }

    #[test]
    fn duplicate_opens_a_prefilled_new_host_form() {
        let mut view = SshHostsView::new();
        view.filtered = vec![SshHostEntry {
            id: "ssh-original".to_string(),
            source: SshHostSource::ThinkTerm,
            spec: spec("prod-1", "10.0.0.7"),
        }];

        let response = view.run_host_command("ssh-original", RemoteHostCommand::Duplicate);
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
