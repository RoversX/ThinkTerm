//! In-window SSH hosts manager: a full-content-area "page" (left host list +
//! right edit form) that replaces the terminal area when toggled from the
//! sidebar `link-2` button. Draws with the shared `ui::draw`/`ui::widgets`
//! infrastructure. The view owns its state + hit-testing and exposes a tiny
//! [`SshViewOutcome`] so `TermWindow` integration stays minimal.

use crate::quad::TripleLayerQuadAllocator;
use crate::ssh_hosts::{self, SshHostEntry, SshHostSource, SshHostSpec};
use crate::termwindow::content_view::{ContentView, ContentViewResponse};
use crate::termwindow::ui::icons::{distro_to_icon, SvgIcon};
use crate::termwindow::TermWindow;
use crate::ui::anim::Easing;
use crate::ui::{
    char_index_for_x, contains, draw_button, draw_icon_button, draw_scrollbar, draw_text_input,
    draw_toggle, rect, text_width_to_char, wheel_delta_pixels, ButtonSpec, ControlState,
    DrawContext, EditModifiers, InputCaret, InteractionState, ScrollState, TextInputSpec,
    TextInputState, UiContext, UiPalette, UiTokens, WidgetKind,
};
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
const FIELD_LABEL_KEYS: [&str; BASE_FIELD_COUNT] = [
    "ssh-field-name",
    "ssh-field-host",
    "ssh-field-port",
    "ssh-field-user",
    "ssh-field-password",
    "ssh-field-identity",
    "ssh-field-workspace",
];

/// Heading emitted before the field at the given index, so the form reads as
/// Connection / Authentication / Session rather than one flat column.
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

const FIELD_GROUP_HEADING_KEYS: [(usize, &str); 3] = [
    (FIELD_NAME, "ssh-group-connection"),
    (FIELD_PASSWORD, "ssh-group-authentication"),
    (FIELD_WORKSPACE, "ssh-group-session"),
];

fn ssh_error(id: &'static str, error: impl Into<String>) -> String {
    let mut args = FluentArgs::new();
    args.set("error", error.into());
    crate::i18n::tr_args(id, &args)
}

const PAD: f32 = 20.0;
const LEFT_DEFAULT_W: f32 = 500.0;
const LEFT_MIN_W: f32 = 360.0;
const LEFT_MAX_W: f32 = 680.0;
const RIGHT_MIN_W: f32 = 360.0;
const SPLIT_HANDLE_W: f32 = 14.0;
const ROW_MIN_H: f32 = 92.0;
const ROW_GAP: f32 = 14.0;
const GROUP_ROW_H: f32 = 44.0;
const INPUT_H: f32 = 44.0;
const BTN_H: f32 = 42.0;
const HOST_ROW_RADIUS: f32 = 14.0;
const HOST_ACTION_RIGHT_PAD: f32 = 14.0;
const HOST_ACTION_GAP: f32 = 8.0;
const HOST_ACTION_BTN: f32 = 32.0;
const LIST_FADE_HEIGHT: f32 = 32.0;

/// Clickable targets inside the view. `usize` payloads index into the current
/// filtered host list, so the action type stays `Copy` (needed by `UiContext`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SshViewAction {
    Search,
    Connect(usize),
    Edit(usize),
    Delete(usize),
    New,
    FocusField(usize),
    ToggleDetect,
    ToggleMosh,
    ToggleMux,
    Save,
    SaveAndConnect,
    Cancel,
    ToggleSystemHosts,
    ResizeLeftPane,
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
    /// Split position in design pixels; painted via ctx.px.
    left_width: f32,
    dragging_left_pane: bool,
    last_area_x: f32,
    last_area_w: f32,
    /// ui scale captured at paint time so mouse handlers (no DrawContext)
    /// can convert pointer pixels back into design pixels.
    last_ui_scale: f32,
    list_area: RectF,
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
            left_width: LEFT_DEFAULT_W,
            dragging_left_pane: false,
            last_area_x: 0.0,
            last_area_w: 0.0,
            last_ui_scale: 1.0,
            list_area: rect(0.0, 0.0, 0.0, 0.0),
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

        let project_id = match &form.editing {
            Some(id) => match ssh_hosts::try_update_host(id, spec) {
                Ok(true) => id.clone(),
                Ok(false) => {
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
                if self.dragging_left_pane {
                    let scale = self.last_ui_scale.max(0.01);
                    self.left_width = Self::clamp_left_width(
                        (x - self.last_area_x) / scale,
                        self.last_area_w / scale,
                    );
                    return ContentViewResponse::Redraw;
                }
                if self.interaction.hovered != hit {
                    self.interaction.hovered = hit;
                    ContentViewResponse::Redraw
                } else {
                    ContentViewResponse::Ignored
                }
            }
            WMEK::Press(MousePress::Left) => {
                self.interaction.pressed = hit;
                if hit == Some(SshViewAction::ResizeLeftPane) {
                    self.dragging_left_pane = true;
                }
                ContentViewResponse::Redraw
            }
            WMEK::Release(MousePress::Left) => {
                if self.dragging_left_pane {
                    self.dragging_left_pane = false;
                    self.interaction.pressed = None;
                    return ContentViewResponse::Redraw;
                }
                let pressed = self.interaction.pressed.take();
                if let (Some(a), Some(b)) = (hit, pressed) {
                    if a == b {
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
            SshViewAction::Edit(i) => {
                if let Some(entry) = self.filtered.get(i) {
                    let id = entry.id.clone();
                    if entry.source == SshHostSource::ThinkTerm {
                        self.open_edit_form(&id);
                    }
                }
                ContentViewResponse::Redraw
            }
            SshViewAction::Delete(i) => {
                let Some(entry) = self
                    .filtered
                    .get(i)
                    .filter(|entry| entry.source == SshHostSource::ThinkTerm)
                else {
                    return ContentViewResponse::Redraw;
                };
                let id = entry.id.clone();
                // Read what this host brought in while the record naming it is
                // still here. A ThinkTerm Connect host mirrors whole Spaces
                // from its mux server, and those are keyed by domain name, not
                // by host id — so `remove_project` below, which matches the
                // host id, never sees them. Left behind they are unreachable:
                // nothing resolves the domain to connect, and every rename or
                // delete is refused because it has to go through a server this
                // device can no longer name.
                let orphaned_spaces = workspace_threads::space_ids_for_domains(
                    &ssh_hosts::domain_names_for_host(&entry.spec),
                );

                if let Err(err) = ssh_hosts::try_remove_host(&id) {
                    log::error!("failed to delete SSH host {id}: {err:#}");
                    self.refresh();
                    return ContentViewResponse::Redraw;
                }
                let _ = workspace_threads::remove_project(&id);
                self.refresh();

                if orphaned_spaces.is_empty() {
                    return ContentViewResponse::Redraw;
                }
                // Through the window, because one of them may be the Space
                // this very window is showing; that path moves it off first.
                // Local removal: the server keeps its Spaces, so re-adding the
                // host brings them back.
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
                self.refresh();
                ContentViewResponse::Redraw
            }
            SshViewAction::ResizeLeftPane => ContentViewResponse::Redraw,
            SshViewAction::Connect(i) => match self.filtered.get(i) {
                Some(entry) => open_thread_response(entry.id.clone()),
                None => ContentViewResponse::Redraw,
            },
        }
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
                }
                ContentViewResponse::Redraw
            }
            (KeyCode::Tab, KeyModifiers::SHIFT) | (KeyCode::UpArrow, KeyModifiers::NONE) => {
                if in_form {
                    self.step_field(-1);
                } else {
                    self.selected = self.selected.saturating_sub(1);
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
            self.selected = 0;
            self.refresh();
        }
    }

    fn take_focused_selection(&mut self) -> Option<String> {
        let mut taken = None;
        self.edit_focused(|input| taken = input.caret_take_selected_text());
        taken
    }

    // ---- paint ------------------------------------------------------------

    fn clamp_left_width(width: f32, total_width: f32) -> f32 {
        let available = (total_width - RIGHT_MIN_W).max(260.0);
        let max_width = available.min(LEFT_MAX_W).max(260.0);
        let min_width = LEFT_MIN_W.min(max_width);
        width.clamp(min_width, max_width)
    }

    fn paint_impl(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
        cursor_on: bool,
    ) -> anyhow::Result<()> {
        let tokens = UiTokens::for_dpi(ctx.dimensions.dpi);
        self.refresh();
        self.widgets.clear();

        let ox = area.origin.x;
        let oy = area.origin.y;
        let w = area.size.width;
        let h = area.size.height;
        self.last_area_x = ox;
        self.last_area_w = w;
        self.last_ui_scale = ctx.px(1.0).max(0.01);
        self.left_width = Self::clamp_left_width(self.left_width, w / self.last_ui_scale);

        // Background.
        ctx.draw_rect(layers, 0, ox, oy, w, h, palette.window_bg)?;

        let left_w = ctx.px(self.left_width);
        let left_x = ox + ctx.px(PAD);
        let inner_left_w = left_w - ctx.px(PAD * 2.0);

        let search_y = oy + ctx.px(PAD + 48.0);

        // Host list.
        let list_top = search_y + ctx.px(INPUT_H + 14.0);
        let list_bottom = oy + h - ctx.px(PAD);
        self.list_area = rect(
            left_x,
            list_top,
            inner_left_w,
            (list_bottom - list_top).max(0.0),
        );
        if self.filtered.is_empty() && self.system_host_count == 0 {
            self.scroll.set_extents(self.list_area.size.height, 0.0);
            ctx.draw_text(
                layers,
                font,
                left_x,
                list_top + ctx.px(8.0),
                &crate::i18n::tr("ssh-no-hosts"),
                palette.muted_text,
                inner_left_w,
            )?;
        } else {
            let filtered = self.filtered.clone();
            let row_h = Self::host_row_height(ctx);
            let think_count = filtered
                .iter()
                .filter(|entry| entry.source == SshHostSource::ThinkTerm)
                .count();
            let visible_system_count =
                if !self.system_hosts_collapsed || !self.search.text().trim().is_empty() {
                    filtered
                        .iter()
                        .filter(|entry| entry.source == SshHostSource::System)
                        .count()
                } else {
                    0
                };
            let mut content_h = Self::rows_height(think_count, row_h, ctx.px(ROW_GAP));
            if self.system_host_count > 0 {
                if content_h > 0.0 {
                    content_h += ctx.px(ROW_GAP + 4.0);
                } else {
                    content_h += ctx.px(4.0);
                }
                content_h += ctx.px(GROUP_ROW_H + 10.0);
                content_h += Self::rows_height(visible_system_count, row_h, ctx.px(ROW_GAP));
            }
            self.scroll
                .set_extents(self.list_area.size.height, content_h);

            let mut row_y = list_top - self.scroll.offset;
            for (i, entry) in filtered
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.source == SshHostSource::ThinkTerm)
            {
                if row_y > list_bottom {
                    break;
                }
                if Self::row_visible(row_y, row_h, list_top, list_bottom) {
                    self.paint_host_row(
                        ctx,
                        layers,
                        font,
                        palette,
                        left_x,
                        row_y,
                        inner_left_w,
                        i,
                        entry,
                        list_top,
                        list_bottom,
                    )?;
                }
                row_y += row_h + ctx.px(ROW_GAP);
            }

            if self.system_host_count > 0 {
                row_y += ctx.px(4.0);
                if Self::row_visible(row_y, ctx.px(GROUP_ROW_H), list_top, list_bottom) {
                    self.paint_system_group_header(
                        ctx,
                        layers,
                        font,
                        palette,
                        left_x,
                        row_y,
                        inner_left_w,
                        list_top,
                        list_bottom,
                    )?;
                    row_y += ctx.px(GROUP_ROW_H + 10.0);
                }
                if !self.system_hosts_collapsed || !self.search.text().trim().is_empty() {
                    for (i, entry) in filtered
                        .iter()
                        .enumerate()
                        .filter(|(_, entry)| entry.source == SshHostSource::System)
                    {
                        if row_y > list_bottom {
                            break;
                        }
                        if Self::row_visible(row_y, row_h, list_top, list_bottom) {
                            self.paint_host_row(
                                ctx,
                                layers,
                                font,
                                palette,
                                left_x,
                                row_y,
                                inner_left_w,
                                i,
                                entry,
                                list_top,
                                list_bottom,
                            )?;
                        }
                        row_y += row_h + ctx.px(ROW_GAP);
                    }
                }
            }
        }

        // Mask scrolled rows back out of the fixed header/search area, then
        // repaint the header controls above the list. This mirrors the main
        // workspace sidebar and avoids rows bleeding over "SSH Hosts".
        self.paint_list_mask(
            ctx,
            layers,
            ox,
            oy,
            left_w,
            (list_top - oy).max(0.0),
            palette,
        )?;
        self.paint_list_mask(
            ctx,
            layers,
            ox,
            list_bottom,
            left_w,
            (oy + h - list_bottom).max(0.0),
            palette,
        )?;
        self.paint_list_fades(ctx, layers, self.list_area, palette)?;
        self.paint_header_controls(
            ctx,
            layers,
            font,
            left_x,
            oy + ctx.px(PAD),
            search_y,
            inner_left_w,
            palette,
            tokens,
            cursor_on,
        )?;
        if self.scroll.has_overflow() {
            draw_scrollbar(ctx, layers, palette, tokens, self.list_area, self.scroll)?;
        }

        // Right pane: form or empty hint.
        let right_x = ox + left_w + ctx.px(PAD);
        let right_w = (ox + w - ctx.px(PAD)) - right_x;
        // Vertical separator.
        let split_x = ox + left_w;
        ctx.draw_rect(
            layers,
            0,
            split_x,
            oy + ctx.px(PAD),
            if self.dragging_left_pane { 2.0 } else { 1.0 },
            h - ctx.px(PAD * 2.0),
            if self.dragging_left_pane {
                palette.selected_bg
            } else {
                palette.separator
            },
        )?;
        self.widgets.push(
            rect(
                split_x - ctx.px(SPLIT_HANDLE_W / 2.0),
                oy,
                ctx.px(SPLIT_HANDLE_W),
                h,
            ),
            WidgetKind::ResizeHandle,
            SshViewAction::ResizeLeftPane,
        );

        if self.form.is_some() {
            let form_top = oy + ctx.px(PAD);
            self.paint_form(
                ctx,
                layers,
                font,
                palette,
                tokens,
                cursor_on,
                right_x,
                form_top,
                right_w,
                (oy + h) - form_top,
            )?;
        } else {
            ctx.draw_text(
                layers,
                font,
                right_x,
                oy + h / 2.0 - 10.0,
                &crate::i18n::tr("ssh-select-host"),
                palette.muted_text,
                right_w.max(0.0),
            )?;
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_header_controls(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        left_x: f32,
        title_y: f32,
        search_y: f32,
        inner_left_w: f32,
        palette: UiPalette,
        tokens: UiTokens,
        cursor_on: bool,
    ) -> anyhow::Result<()> {
        ctx.draw_text_on_layer(
            layers,
            2,
            font,
            left_x,
            title_y,
            &crate::i18n::tr("ssh-hosts-title"),
            palette.text,
            inner_left_w,
        )?;

        let search_rect = rect(
            left_x,
            search_y,
            inner_left_w - ctx.px(INPUT_H + 8.0),
            ctx.px(INPUT_H),
        );
        self.widgets
            .push(search_rect, WidgetKind::TextInput, SshViewAction::Search);
        let search_focused = matches!(self.focus, Focus::Search);
        let search_border = if search_focused {
            palette.selected_bg
        } else if self.interaction.hovered == Some(SshViewAction::Search) {
            palette.separator
        } else {
            palette.control_border
        };
        ctx.draw_rounded_frame(
            layers,
            2,
            search_rect.origin.x,
            search_rect.origin.y,
            search_rect.size.width,
            search_rect.size.height,
            palette.control_bg,
            search_border,
            tokens.control_radius,
        )?;

        // Hand-rolled rather than `draw_text_input` because this box sits on
        // layer 2, above the list's scroll fade. Geometry mirrors that widget,
        // including the design-pixel conversions.
        let text_pad = ctx.px(12.0);
        let text_left = search_rect.origin.x + text_pad;
        let text_area = (search_rect.size.width - text_pad * 2.0).max(0.0);
        let inset_y = ctx.px(5.0);
        let highlight_y = search_rect.origin.y + inset_y;
        let highlight_h = (search_rect.size.height - inset_y * 2.0).max(0.0);
        // Must run before anything borrows `self.search` for the rest of the
        // frame, since it may move the caret.
        let search_text_owned = self.search.text().to_string();
        self.resolve_pending_caret_click(ctx, font, Focus::Search, &search_text_owned, text_left);
        let search_placeholder = crate::i18n::tr("ssh-search");
        let (search_text, search_color) = if search_text_owned.is_empty() && !search_focused {
            (search_placeholder.as_str(), palette.muted_text)
        } else {
            (search_text_owned.as_str(), palette.text)
        };
        let selection = search_focused
            .then(|| self.search.caret_selection_range())
            .flatten()
            .filter(|(start, end)| start != end);
        if let Some((start, end)) = selection {
            let start_x = text_width_to_char(ctx, font, &search_text_owned, start).min(text_area);
            let end_x = text_width_to_char(ctx, font, &search_text_owned, end).min(text_area);
            ctx.draw_rounded_rect(
                layers,
                2,
                text_left + start_x - ctx.px(4.0),
                highlight_y,
                (end_x - start_x) + ctx.px(8.0),
                highlight_h,
                palette.selected_bg.mul_alpha(0.56),
                (tokens.control_radius - ctx.px(5.0)).max(ctx.px(3.0)),
            )?;
        }
        ctx.draw_text_on_layer(
            layers,
            2,
            font,
            text_left,
            Self::control_text_y(ctx, search_rect.origin.y, search_rect.size.height),
            search_text,
            search_color,
            text_area,
        )?;
        if search_focused && selection.is_none() && cursor_on {
            let caret_dx = text_width_to_char(ctx, font, &search_text_owned, self.search.cursor)
                .min(text_area);
            let caret_width = ctx.px(3.0);
            ctx.draw_rect(
                layers,
                2,
                text_left + caret_dx - caret_width / 3.0,
                highlight_y,
                caret_width,
                highlight_h,
                palette.selected_bg,
            )?;
        }

        let add_x = left_x + inner_left_w - ctx.px(INPUT_H);
        let add_rect = rect(add_x, search_y, ctx.px(INPUT_H), ctx.px(INPUT_H));
        self.widgets
            .push(add_rect, WidgetKind::Button, SshViewAction::New);
        let add_hovered = self.interaction.hovered == Some(SshViewAction::New);
        let add_pressed = self.interaction.pressed == Some(SshViewAction::New);
        let add_bg = if add_pressed {
            palette.control_pressed_bg
        } else if add_hovered {
            palette.control_hover_bg
        } else {
            LinearRgba::TRANSPARENT
        };
        if add_bg.3 > 0.0 {
            ctx.draw_rounded_rect(
                layers,
                2,
                add_x,
                search_y,
                ctx.px(INPUT_H),
                ctx.px(INPUT_H),
                add_bg,
                ctx.px(8.0),
            )?;
        }
        let icon_size = (ctx.metrics.cell_size.height as f32 + 4.0).clamp(20.0, 30.0);
        ctx.draw_svg_icon(
            layers,
            SvgIcon::Plus,
            add_x + (ctx.px(INPUT_H) - icon_size) / 2.0,
            search_y + (ctx.px(INPUT_H) - icon_size) / 2.0,
            icon_size,
            if add_hovered || add_pressed {
                palette.text
            } else {
                palette.muted_text
            },
        )?;

        Ok(())
    }

    fn control_text_y(ctx: &DrawContext, y: f32, height: f32) -> f32 {
        let cell_height = ctx.metrics.cell_size.height as f32;
        y + ((height - cell_height) / 2.0).max(0.0)
    }

    fn paint_list_fades(
        &self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        palette: UiPalette,
    ) -> anyhow::Result<()> {
        if !self.scroll.has_overflow() || area.size.width <= 0.0 || area.size.height <= 0.0 {
            return Ok(());
        }

        let fade_height = ctx.px(LIST_FADE_HEIGHT).min(area.size.height).ceil() as usize;
        if fade_height == 0 {
            return Ok(());
        }

        if self.scroll.offset > 0.5 {
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

        if self.scroll.offset < self.scroll.max_offset() - 0.5 {
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
    fn paint_system_group_header(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        palette: UiPalette,
        x: f32,
        y: f32,
        width: f32,
        list_top: f32,
        list_bottom: f32,
    ) -> anyhow::Result<()> {
        let hit_y = y.max(list_top);
        let hit_h = (y + ctx.px(GROUP_ROW_H)).min(list_bottom) - hit_y;
        let rect = rect(x, hit_y, width, hit_h.max(0.0));
        self.widgets.push(
            rect,
            WidgetKind::SidebarRow,
            SshViewAction::ToggleSystemHosts,
        );
        let hovered = self.interaction.hovered == Some(SshViewAction::ToggleSystemHosts);
        if hovered {
            ctx.draw_rounded_rect(
                layers,
                0,
                x,
                y,
                width,
                ctx.px(GROUP_ROW_H),
                palette.sidebar_row_hover_bg,
                10.0,
            )?;
        }
        let icon = if self.system_hosts_collapsed && self.search.text().trim().is_empty() {
            SvgIcon::ChevronRight
        } else {
            SvgIcon::ChevronDown
        };
        ctx.draw_svg_icon(layers, icon, x + 12.0, y + 12.0, 20.0, palette.muted_text)?;
        let mut args = FluentArgs::new();
        args.set("count", self.system_host_count);
        let label = crate::i18n::tr_args("ssh-system-hosts", &args);
        ctx.draw_text(
            layers,
            font,
            x + ctx.px(42.0),
            y + ctx.px(8.0),
            &label,
            palette.muted_text,
            (width - 50.0).max(0.0),
        )?;
        Ok(())
    }

    fn host_row_height(ctx: &DrawContext) -> f32 {
        (ctx.metrics.cell_size.height as f32 * 2.15 + ctx.px(26.0)).max(ctx.px(ROW_MIN_H))
    }

    fn rows_height(count: usize, row_h: f32, row_gap: f32) -> f32 {
        if count == 0 {
            0.0
        } else {
            count as f32 * row_h + (count.saturating_sub(1)) as f32 * row_gap
        }
    }

    fn row_visible(y: f32, height: f32, list_top: f32, list_bottom: f32) -> bool {
        y + height >= list_top && y <= list_bottom
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_host_row(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        palette: UiPalette,
        x: f32,
        y: f32,
        width: f32,
        index: usize,
        entry: &SshHostEntry,
        list_top: f32,
        list_bottom: f32,
    ) -> anyhow::Result<()> {
        let spec = &entry.spec;
        let row_h = Self::host_row_height(ctx);
        let hit_y = y.max(list_top);
        let hit_h = (y + row_h).min(list_bottom) - hit_y;
        let row_rect = rect(x, hit_y, width, hit_h.max(0.0));
        self.widgets.push(
            row_rect,
            WidgetKind::SidebarRow,
            SshViewAction::Connect(index),
        );

        let selected = index == self.selected;
        let hovered = self.interaction.hovered == Some(SshViewAction::Connect(index));
        let bg = if selected {
            palette.sidebar_row_active_bg
        } else if hovered {
            palette.sidebar_row_hover_bg
        } else {
            LinearRgba::TRANSPARENT
        };
        if bg.3 > 0.0 {
            ctx.draw_rounded_rect(layers, 0, x, y, width, row_h, bg, ctx.px(HOST_ROW_RADIUS))?;
        }

        // OS / brand icon.
        let line_h = ctx.metrics.cell_size.height as f32;
        let icon_size = (line_h * 1.05).clamp(ctx.px(34.0), ctx.px(48.0));
        let icon_x = x + ctx.px(12.0);
        let icon_y = y + (row_h - icon_size) / 2.0;
        match spec.detected_distro.as_deref().and_then(distro_to_icon) {
            Some(brand) => ctx.draw_brand_icon(layers, brand, icon_x, icon_y, icon_size)?,
            None => ctx.draw_svg_icon(
                layers,
                SvgIcon::Server,
                icon_x,
                icon_y,
                icon_size,
                palette.text,
            )?,
        }

        let text_x = icon_x + icon_size + ctx.px(14.0);
        let action_space = if entry.source == SshHostSource::ThinkTerm {
            ctx.px(HOST_ACTION_RIGHT_PAD + HOST_ACTION_BTN * 2.0 + HOST_ACTION_GAP + 10.0)
        } else {
            ctx.px(16.0)
        };
        let text_w = width - (text_x - x) - action_space;
        let stack_h = line_h * 2.02;
        let title_y = y + ((row_h - stack_h) / 2.0).max(ctx.px(8.0)) - ctx.px(2.0);
        let subtitle_y = (title_y + line_h * 1.02).min(y + row_h - line_h - ctx.px(8.0));
        ctx.draw_text(
            layers,
            font,
            text_x,
            title_y,
            &spec.label,
            palette.text,
            text_w.max(0.0),
        )?;
        let subtitle = {
            let user = spec.username.as_deref().unwrap_or("");
            let source = if spec.use_mosh {
                "mosh"
            } else if spec.multiplexing {
                "thinkterm connect"
            } else {
                match entry.source {
                    SshHostSource::ThinkTerm => "ssh",
                    SshHostSource::System => "system ssh",
                }
            };
            if user.is_empty() {
                source.to_string()
            } else {
                format!("{source}, {user}")
            }
        };
        ctx.draw_text(
            layers,
            font,
            text_x,
            subtitle_y,
            &subtitle,
            palette.muted_text,
            text_w.max(0.0),
        )?;

        // Edit + delete icon buttons (right aligned). System SSH config hosts
        // are read-only here: connect only, no write-back.
        if entry.source == SshHostSource::System {
            return Ok(());
        }
        let btn = ctx.px(HOST_ACTION_BTN);
        let btn_y = y + (row_h - btn) / 2.0;
        draw_icon_button(
            ctx,
            layers,
            &mut self.widgets,
            &self.interaction,
            palette,
            x + width - ctx.px(HOST_ACTION_RIGHT_PAD) - btn * 2.0 - ctx.px(HOST_ACTION_GAP),
            btn_y,
            btn,
            SvgIcon::SlidersHorizontal,
            SshViewAction::Edit(index),
        )?;
        draw_icon_button(
            ctx,
            layers,
            &mut self.widgets,
            &self.interaction,
            palette,
            x + width - ctx.px(HOST_ACTION_RIGHT_PAD) - btn,
            btn_y,
            btn,
            SvgIcon::Trash2,
            SshViewAction::Delete(index),
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_form(
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
        height: f32,
    ) -> anyhow::Result<()> {
        let form = self.form.as_ref().unwrap();
        let editing = form.editing.is_some();
        let detect_os = form.detect_os;
        let use_mosh = form.use_mosh;
        let multiplexing = form.multiplexing;
        let fields = form.fields.clone();
        let options = form.options.clone();
        let advanced_open = form.advanced_open;
        let error = form.error.clone();
        let focus = self.focus;

        // A form pinned to the left of a wide column reads badly, so cap the
        // width and centre what is left over.
        let field_w = width.min(ctx.px(560.0));
        let x = x + ((width - field_w) / 2.0).max(0.0);
        self.form_area = rect(x, y, field_w, height);
        self.form_scroll.set_extents(height, self.form_content_h);
        let top = y - self.form_scroll.offset;
        let mut cur_y = top;

        ctx.draw_text(
            layers,
            font,
            x,
            cur_y,
            &crate::i18n::tr(if editing {
                "ssh-edit-host"
            } else {
                "ssh-new-host"
            }),
            palette.text,
            field_w,
        )?;
        cur_y += ctx.metrics.cell_size.height as f32 + ctx.px(18.0);

        let line_h = ctx.metrics.cell_size.height as f32;
        for (i, label_key) in FIELD_LABEL_KEYS.iter().enumerate() {
            if let Some(heading_key) = FIELD_GROUP_HEADING_KEYS
                .iter()
                .find_map(|(at, heading)| (*at == i).then_some(*heading))
            {
                cur_y += ctx.px(if i == 0 { 0.0 } else { 10.0 });
                ctx.draw_text(
                    layers,
                    font,
                    x,
                    cur_y,
                    &crate::i18n::tr(heading_key),
                    palette.text,
                    field_w,
                )?;
                cur_y += line_h + ctx.px(10.0);
            }
            ctx.draw_text(
                layers,
                font,
                x,
                cur_y,
                &crate::i18n::tr(label_key),
                palette.muted_text,
                field_w,
            )?;
            let input_y = cur_y + line_h + ctx.px(6.0);
            // Mask the password field.
            let masked = if i == FIELD_PASSWORD {
                "•".repeat(fields[i].text().chars().count())
            } else {
                String::new()
            };
            let shown: &str = if i == FIELD_PASSWORD {
                &masked
            } else {
                &fields[i].text()
            };
            // Resolved up front: the call below borrows `self.widgets` mutably.
            self.resolve_pending_caret_click(ctx, font, Focus::Field(i), shown, x + ctx.px(12.0));
            let field_caret = self.caret_for(Focus::Field(i));
            draw_text_input(
                ctx,
                layers,
                font,
                &mut self.widgets,
                &self.interaction,
                palette,
                tokens,
                ctx.px(12.0),
                cursor_on,
                TextInputSpec {
                    placeholder: "",
                    text: shown,
                    rect: rect(x, input_y, field_w, ctx.px(INPUT_H)),
                    focused: focus == Focus::Field(i),
                    selected_all: false,
                    action: SshViewAction::FocusField(i),
                },
                field_caret,
            )?;
            cur_y = input_y + ctx.px(INPUT_H + 18.0);
        }

        // Detect-OS toggle.
        let toggle_w = ctx.px(44.0);
        let toggle_h = ctx.px(24.0);
        ctx.draw_text(
            layers,
            font,
            x,
            cur_y + (toggle_h - line_h) / 2.0,
            &crate::i18n::tr("ssh-detect-os"),
            palette.text,
            field_w - toggle_w - ctx.px(12.0),
        )?;
        draw_toggle(
            ctx,
            layers,
            &mut self.widgets,
            palette,
            rect(x + field_w - toggle_w, cur_y, toggle_w, toggle_h),
            detect_os,
            SshViewAction::ToggleDetect,
        )?;
        cur_y += toggle_h + ctx.px(24.0);

        // Use-Mosh toggle: connect by launching the local `mosh` client instead
        // of the SSH domain.
        ctx.draw_text(
            layers,
            font,
            x,
            cur_y + (toggle_h - line_h) / 2.0,
            &crate::i18n::tr("ssh-use-mosh"),
            palette.text,
            field_w - toggle_w - ctx.px(12.0),
        )?;
        draw_toggle(
            ctx,
            layers,
            &mut self.widgets,
            palette,
            rect(x + field_w - toggle_w, cur_y, toggle_w, toggle_h),
            use_mosh,
            SshViewAction::ToggleMosh,
        )?;
        cur_y += toggle_h + ctx.px(24.0);

        if use_mosh {
            self.resolve_pending_caret_click(
                ctx,
                font,
                Focus::Field(FIELD_MOSH_SERVER),
                &fields[FIELD_MOSH_SERVER].text(),
                x + ctx.px(12.0),
            );
            let mosh_caret = self.caret_for(Focus::Field(FIELD_MOSH_SERVER));
            draw_text_input(
                ctx,
                layers,
                font,
                &mut self.widgets,
                &self.interaction,
                palette,
                tokens,
                ctx.px(12.0),
                cursor_on,
                TextInputSpec {
                    placeholder: ssh_hosts::DEFAULT_MOSH_SERVER_COMMAND,
                    text: &fields[FIELD_MOSH_SERVER].text(),
                    rect: rect(x, cur_y, field_w, ctx.px(INPUT_H)),
                    focused: focus == Focus::Field(FIELD_MOSH_SERVER),
                    selected_all: false,
                    action: SshViewAction::FocusField(FIELD_MOSH_SERVER),
                },
                mosh_caret,
            )?;
            cur_y += ctx.px(INPUT_H + 24.0);
        }

        // ThinkTerm-Connect toggle: attach the persistent remote mux domain
        // (`thinkterm connect`) instead of a direct SSH session. Requires a
        // thinkterm/wezterm binary on the remote host.
        ctx.draw_text(
            layers,
            font,
            x,
            cur_y + (toggle_h - line_h) / 2.0,
            &crate::i18n::tr("ssh-use-mux"),
            palette.text,
            field_w - toggle_w - ctx.px(12.0),
        )?;
        draw_toggle(
            ctx,
            layers,
            &mut self.widgets,
            palette,
            rect(x + field_w - toggle_w, cur_y, toggle_w, toggle_h),
            multiplexing,
            SshViewAction::ToggleMux,
        )?;
        cur_y += toggle_h + ctx.px(24.0);

        // Advanced: raw ssh_config overrides (ProxyJump, ServerAliveInterval,
        // …). `build_ssh_domain` already feeds these straight into the ssh
        // config, so nothing downstream needs to change.
        let disclosure = if advanced_open { "▾" } else { "▸" };
        let advanced_h = line_h + ctx.px(12.0);
        self.widgets.push(
            rect(x, cur_y, field_w, advanced_h),
            WidgetKind::Button,
            SshViewAction::ToggleAdvanced,
        );
        ctx.draw_text(
            layers,
            font,
            x,
            cur_y + (advanced_h - line_h) / 2.0,
            &format!("{disclosure}  {}", crate::i18n::tr("ssh-advanced")),
            palette.text,
            field_w,
        )?;
        cur_y += advanced_h + ctx.px(8.0);

        if advanced_open {
            let remove_w = ctx.px(INPUT_H);
            let gap = ctx.px(8.0);
            let pair_w = (field_w - remove_w - gap * 2.0).max(0.0);
            let key_w = pair_w * 0.42;
            let value_w = pair_w - key_w;
            for (index, (key, value)) in options.iter().enumerate() {
                let key_x = x;
                let value_x = key_x + key_w + gap;
                self.resolve_pending_caret_click(
                    ctx,
                    font,
                    Focus::OptionKey(index),
                    &key.text(),
                    key_x + ctx.px(12.0),
                );
                let key_caret = self.caret_for(Focus::OptionKey(index));
                draw_text_input(
                    ctx,
                    layers,
                    font,
                    &mut self.widgets,
                    &self.interaction,
                    palette,
                    tokens,
                    ctx.px(12.0),
                    cursor_on,
                    TextInputSpec {
                        placeholder: "ProxyJump",
                        text: &key.text(),
                        rect: rect(key_x, cur_y, key_w, ctx.px(INPUT_H)),
                        focused: focus == Focus::OptionKey(index),
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
                    value_x + ctx.px(12.0),
                );
                let value_caret = self.caret_for(Focus::OptionValue(index));
                draw_text_input(
                    ctx,
                    layers,
                    font,
                    &mut self.widgets,
                    &self.interaction,
                    palette,
                    tokens,
                    ctx.px(12.0),
                    cursor_on,
                    TextInputSpec {
                        placeholder: "user@bastion",
                        text: &value.text(),
                        rect: rect(value_x, cur_y, value_w, ctx.px(INPUT_H)),
                        focused: focus == Focus::OptionValue(index),
                        selected_all: false,
                        action: SshViewAction::FocusOptionValue(index),
                    },
                    value_caret,
                )?;
                draw_icon_button(
                    ctx,
                    layers,
                    &mut self.widgets,
                    &self.interaction,
                    palette,
                    value_x + value_w + gap,
                    cur_y,
                    remove_w,
                    SvgIcon::Trash2,
                    SshViewAction::RemoveOption(index),
                )?;
                cur_y += ctx.px(INPUT_H) + gap;
            }

            let add_label = crate::i18n::tr("ssh-add-option");
            let add_w = ctx.measure_text_width(font, &add_label) + ctx.px(36.0);
            // Resolved before `self.widgets` is borrowed mutably below.
            let add_state = self.button_state(SshViewAction::AddOption, false);
            draw_button(
                ctx,
                layers,
                font,
                &mut self.widgets,
                palette,
                ButtonSpec {
                    label: &add_label,
                    action: SshViewAction::AddOption,
                    rect: rect(x, cur_y, add_w, ctx.px(BTN_H)),
                    state: add_state,
                    kind: WidgetKind::Button,
                },
            )?;
            cur_y += ctx.px(BTN_H) + ctx.px(24.0);
        }

        if let Some(err) = &error {
            ctx.draw_text(
                layers,
                font,
                x,
                cur_y,
                &format!("⚠ {err}"),
                palette.text,
                field_w,
            )?;
            cur_y += line_h + ctx.px(12.0);
        }

        // Buttons sized to their (measured) label so nothing is truncated.
        let gap = ctx.px(12.0);
        let btn_w = |label: &str| ctx.measure_text_width(font, label) + ctx.px(36.0);
        let mut bx = x;
        for (label, action, primary) in [
            (
                crate::i18n::tr("ssh-save-open"),
                SshViewAction::SaveAndConnect,
                true,
            ),
            (crate::i18n::tr("ssh-save"), SshViewAction::Save, false),
            (crate::i18n::tr("ssh-cancel"), SshViewAction::Cancel, false),
        ] {
            let w = btn_w(&label);
            let spec = ButtonSpec {
                label: &label,
                action,
                rect: rect(bx, cur_y, w, ctx.px(BTN_H)),
                state: self.button_state(action, primary),
                kind: WidgetKind::Button,
            };
            draw_button(ctx, layers, font, &mut self.widgets, palette, spec)?;
            bx += w + gap;
        }
        cur_y += ctx.px(BTN_H) + ctx.px(PAD);

        self.form_content_h = cur_y - top;
        // These draws cannot clip, so hide anything that scrolled above the
        // column by repainting the strip over it (still inside our own area).
        if self.form_scroll.offset > 0.0 {
            ctx.draw_rect(
                layers,
                2,
                x,
                y - ctx.px(PAD),
                field_w,
                ctx.px(PAD),
                palette.window_bg,
            )?;
        }
        if self.form_scroll.has_overflow() {
            draw_scrollbar(
                ctx,
                layers,
                palette,
                tokens,
                self.form_area,
                self.form_scroll,
            )?;
        }
        Ok(())
    }

    fn button_state(&self, action: SshViewAction, primary: bool) -> ControlState {
        if self.interaction.pressed == Some(action) {
            ControlState::Pressed
        } else if self.interaction.hovered == Some(action) {
            ControlState::Hovered
        } else if primary {
            ControlState::Active
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
        _title_font: &Rc<LoadedFont>,
        _section_font: &Rc<LoadedFont>,
        cursor_on: bool,
    ) -> anyhow::Result<()> {
        self.paint_impl(ctx, layers, area, palette, font, cursor_on)
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
        Focus, HostForm, BASE_FIELD_COUNT, FIELD_HOST, FIELD_WORKSPACE,
    };

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
