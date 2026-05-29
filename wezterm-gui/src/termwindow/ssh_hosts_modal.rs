//! ThinkTerm SSH host manager — an in-window modal panel (Termius-style) that
//! lists saved SSH hosts as clickable cards and lets you add/edit/connect them.
//!
//! Rendering & input follow the same pattern as the command palette
//! (`palette.rs`): a box-model element tree built in [`Modal::computed_element`],
//! keyboard handling in [`Modal::key_down`], and clickable cards carry a
//! [`UIItemType::SshHosts`] item type so clicks are dispatched through the
//! normal mouse path (`mouseevent.rs`).

use crate::termwindow::box_model::*;
use crate::termwindow::modal::Modal;
use crate::termwindow::render::corners::{
    BOTTOM_LEFT_ROUNDED_CORNER, BOTTOM_RIGHT_ROUNDED_CORNER, TOP_LEFT_ROUNDED_CORNER,
    TOP_RIGHT_ROUNDED_CORNER,
};
use crate::termwindow::ui::icons::{distro_to_icon, SvgIcon};
use crate::termwindow::{DimensionContext, SshHostsAction, TermWindow, UIItemType};
use crate::utilsprites::RenderMetrics;
use config::keyassignment::KeyAssignment;
use config::Dimension;
use std::cell::{Ref, RefCell};
use std::rc::Rc;
use wezterm_term::{KeyCode, KeyModifiers, MouseEvent};
use window::color::LinearRgba;

/// Field order in the host form.
const FIELD_LABEL: usize = 0;
const FIELD_HOST: usize = 1;
const FIELD_PORT: usize = 2;
const FIELD_USER: usize = 3;
const FIELD_IDENTITY: usize = 4;
const FIELD_WORKSPACE: usize = 5;
const FIELD_COUNT: usize = 6;

const FIELD_LABELS: [&str; FIELD_COUNT] = [
    "Name",
    "Host",
    "Port",
    "User",
    "Identity file",
    "Workspace",
];

#[derive(Default, Clone)]
struct HostForm {
    /// Remote project id when editing an existing host.
    editing: Option<String>,
    /// Preserved spec when editing, so non-form fields survive a save.
    original: Option<crate::project_sessions::SshHostSpec>,
    fields: [String; FIELD_COUNT],
    focused: usize,
    /// Whether to run engine OS detection (`/etc/os-release`) after connecting.
    detect_os: bool,
    error: Option<String>,
}

enum View {
    Grid,
    Form(HostForm),
}

struct State {
    view: View,
    search: String,
    selected: usize,
}

pub struct SshHostsModal {
    element: RefCell<Option<Vec<ComputedElement>>>,
    state: RefCell<State>,
}

impl SshHostsModal {
    pub fn new() -> Self {
        Self {
            element: RefCell::new(None),
            state: RefCell::new(State {
                view: View::Grid,
                search: String::new(),
                selected: 0,
            }),
        }
    }

    /// Hosts (project_id, spec) filtered by the current search text.
    fn filtered_hosts(search: &str) -> Vec<(String, crate::project_sessions::SshHostSpec)> {
        let needle = search.trim().to_ascii_lowercase();
        crate::project_sessions::list_hosts()
            .into_iter()
            .filter(|(_, spec)| {
                if needle.is_empty() {
                    return true;
                }
                let hay = format!(
                    "{} {} {}",
                    spec.label,
                    spec.host,
                    spec.username.as_deref().unwrap_or("")
                )
                .to_ascii_lowercase();
                hay.contains(&needle)
            })
            .collect()
    }

    // --- Grid interactions -------------------------------------------------

    pub fn push_search_char(&self, c: char) {
        let mut state = self.state.borrow_mut();
        state.search.push(c);
        state.selected = 0;
        self.element.borrow_mut().take();
    }

    pub fn backspace_search(&self) {
        let mut state = self.state.borrow_mut();
        state.search.pop();
        state.selected = 0;
        self.element.borrow_mut().take();
    }

    pub fn move_selection(&self, delta: isize) {
        let mut state = self.state.borrow_mut();
        let count = Self::filtered_hosts(&state.search).len();
        if count == 0 {
            state.selected = 0;
        } else {
            let max = count - 1;
            let next = (state.selected as isize + delta).clamp(0, max as isize);
            state.selected = next as usize;
        }
        self.element.borrow_mut().take();
    }

    /// The remote project id of the currently highlighted host, if any.
    pub fn selected_host_id(&self) -> Option<String> {
        let state = self.state.borrow();
        Self::filtered_hosts(&state.search)
            .into_iter()
            .nth(state.selected)
            .map(|(id, _)| id)
    }

    // --- Form interactions -------------------------------------------------

    pub fn enter_new_form(&self) {
        let mut state = self.state.borrow_mut();
        let mut form = HostForm::default();
        form.fields[FIELD_PORT] = "22".to_string();
        form.detect_os = true;
        state.view = View::Form(form);
        self.element.borrow_mut().take();
    }

    pub fn enter_edit_form(&self, project_id: &str) {
        let Some(spec) = crate::project_sessions::host_spec(project_id) else {
            return;
        };
        let mut fields: [String; FIELD_COUNT] = Default::default();
        fields[FIELD_LABEL] = spec.label.clone();
        fields[FIELD_HOST] = spec.host.clone();
        fields[FIELD_PORT] = spec.port.map(|p| p.to_string()).unwrap_or_default();
        fields[FIELD_USER] = spec.username.clone().unwrap_or_default();
        fields[FIELD_IDENTITY] = spec.identity_file.clone().unwrap_or_default();
        fields[FIELD_WORKSPACE] = spec.default_workspace.clone().unwrap_or_default();
        let mut state = self.state.borrow_mut();
        let detect_os = spec.detect_os;
        state.view = View::Form(HostForm {
            editing: Some(project_id.to_string()),
            original: Some(spec),
            fields,
            focused: FIELD_HOST,
            detect_os,
            error: None,
        });
        self.element.borrow_mut().take();
    }

    pub fn cancel_to_grid(&self) {
        let mut state = self.state.borrow_mut();
        state.view = View::Grid;
        self.element.borrow_mut().take();
    }

    pub fn toggle_detect(&self) {
        let mut state = self.state.borrow_mut();
        if let View::Form(form) = &mut state.view {
            form.detect_os = !form.detect_os;
        }
        self.element.borrow_mut().take();
    }

    pub fn focus_field(&self, idx: usize) {
        let mut state = self.state.borrow_mut();
        if let View::Form(form) = &mut state.view {
            form.focused = idx.min(FIELD_COUNT - 1);
        }
        self.element.borrow_mut().take();
    }

    pub fn focus_step(&self, delta: isize) {
        let mut state = self.state.borrow_mut();
        if let View::Form(form) = &mut state.view {
            let next = (form.focused as isize + delta).rem_euclid(FIELD_COUNT as isize);
            form.focused = next as usize;
        }
        self.element.borrow_mut().take();
    }

    pub fn form_push_char(&self, c: char) {
        let mut state = self.state.borrow_mut();
        if let View::Form(form) = &mut state.view {
            form.fields[form.focused].push(c);
            form.error = None;
        }
        self.element.borrow_mut().take();
    }

    pub fn form_backspace(&self) {
        let mut state = self.state.borrow_mut();
        if let View::Form(form) = &mut state.view {
            form.fields[form.focused].pop();
            form.error = None;
        }
        self.element.borrow_mut().take();
    }

    pub fn is_form(&self) -> bool {
        matches!(self.state.borrow().view, View::Form(_))
    }

    /// Validate the form and create/update the host. On success returns the
    /// remote project id and switches back to the grid; on error records the
    /// message and stays on the form.
    pub fn persist_form(&self) -> Option<String> {
        let mut state = self.state.borrow_mut();
        let View::Form(form) = &mut state.view else {
            return None;
        };

        let host = form.fields[FIELD_HOST].trim().to_string();
        if host.is_empty() {
            form.error = Some("Host is required".to_string());
            self.element.borrow_mut().take();
            return None;
        }
        let port = match form.fields[FIELD_PORT].trim() {
            "" => None,
            value => match value.parse::<u16>() {
                Ok(p) => Some(p),
                Err(_) => {
                    form.error = Some("Port must be a number".to_string());
                    self.element.borrow_mut().take();
                    return None;
                }
            },
        };

        let opt = |s: &str| {
            let t = s.trim();
            (!t.is_empty()).then(|| t.to_string())
        };
        let label = match opt(&form.fields[FIELD_LABEL]) {
            Some(l) => l,
            None => host.clone(),
        };

        let mut spec = form.original.clone().unwrap_or_else(|| {
            crate::project_sessions::SshHostSpec {
                label: label.clone(),
                host: host.clone(),
                port,
                username: None,
                identity_file: None,
                ssh_options: Default::default(),
                multiplexing: true,
                default_workspace: None,
                detect_os: true,
                detected_distro: None,
            }
        });
        spec.label = label;
        spec.host = host;
        spec.port = port;
        spec.username = opt(&form.fields[FIELD_USER]);
        spec.identity_file = opt(&form.fields[FIELD_IDENTITY]);
        spec.default_workspace = opt(&form.fields[FIELD_WORKSPACE]);
        spec.detect_os = form.detect_os;

        let project_id = match &form.editing {
            Some(id) => {
                crate::project_sessions::update_host(id, spec);
                id.clone()
            }
            None => crate::project_sessions::create_host(spec),
        };

        state.view = View::Grid;
        self.element.borrow_mut().take();
        Some(project_id)
    }

    // --- Rendering ---------------------------------------------------------

    fn compute(&self, term_window: &mut TermWindow) -> anyhow::Result<Vec<ComputedElement>> {
        let font = term_window
            .fonts
            .command_palette_font()
            .expect("to resolve command palette font");
        let metrics = RenderMetrics::with_font_metrics(&font.metrics());

        let bg_linear = term_window.config.command_palette_bg_color.to_linear();
        let bg: InheritableColor = bg_linear.into();
        let fg: InheritableColor = term_window
            .config
            .command_palette_fg_color
            .to_linear()
            .into();

        let state = self.state.borrow();
        let mut children: Vec<Element> = vec![];

        match &state.view {
            View::Grid => {
                children.push(
                    heading(&font, "SSH Hosts").display(DisplayType::Block),
                );
                children.push(
                    Element::new(
                        &font,
                        ElementContent::Text(format!("Search: {}_", state.search)),
                    )
                    .display(DisplayType::Block)
                    .colors(text_colors(fg.clone())),
                );

                // "+ New Host" action row.
                children.push(
                    action_row(
                        &font,
                        SvgIcon::Plus,
                        "New Host".to_string(),
                        String::new(),
                        UIItemType::SshHosts(SshHostsAction::New),
                        false,
                        bg.clone(),
                        fg.clone(),
                    ),
                );

                let hosts = Self::filtered_hosts(&state.search);
                if hosts.is_empty() {
                    children.push(
                        Element::new(
                            &font,
                            ElementContent::Text(
                                "No SSH hosts yet. Click \"New Host\" to add one.".to_string(),
                            ),
                        )
                        .display(DisplayType::Block)
                        .colors(text_colors(fg.clone())),
                    );
                } else {
                    for (idx, (id, spec)) in hosts.iter().enumerate() {
                        let subtitle = host_subtitle(spec);
                        children.push(host_card(
                            &font,
                            id,
                            &spec.label,
                            &subtitle,
                            spec.detected_distro.as_deref(),
                            idx == state.selected,
                            bg.clone(),
                            fg.clone(),
                        ));
                    }
                }
            }
            View::Form(form) => {
                let title = if form.editing.is_some() {
                    "Edit Host"
                } else {
                    "New Host"
                };
                children.push(heading(&font, title).display(DisplayType::Block));

                for (idx, label) in FIELD_LABELS.iter().enumerate() {
                    let value = &form.fields[idx];
                    let focused = idx == form.focused;
                    let shown = if focused {
                        format!("{value}_")
                    } else {
                        value.clone()
                    };
                    let line = format!("{label:<14}{shown}");
                    children.push(
                        Element::new(&font, ElementContent::Text(line))
                            .display(DisplayType::Block)
                            .item_type(UIItemType::SshHosts(SshHostsAction::FocusField(idx)))
                            .colors(if focused {
                                selected_colors(bg.clone(), fg.clone())
                            } else {
                                text_colors(fg.clone())
                            })
                            .padding(cells(0.25, 0.25, 0.1, 0.1))
                            .min_width(Some(Dimension::Percent(1.))),
                    );
                }

                let detect_label = format!(
                    "Detect OS on connect: {}",
                    if form.detect_os { "ON" } else { "OFF" }
                );
                children.push(
                    Element::new(&font, ElementContent::Text(detect_label))
                        .display(DisplayType::Block)
                        .item_type(UIItemType::SshHosts(SshHostsAction::ToggleDetect))
                        .colors(text_colors(fg.clone()))
                        .padding(cells(0.25, 0.25, 0.3, 0.1))
                        .min_width(Some(Dimension::Percent(1.))),
                );

                if let Some(err) = &form.error {
                    children.push(
                        Element::new(&font, ElementContent::Text(format!("⚠ {err}")))
                            .display(DisplayType::Block)
                            .colors(text_colors(fg.clone())),
                    );
                }

                // Button row.
                let buttons = vec![
                    button(
                        &font,
                        "Save",
                        UIItemType::SshHosts(SshHostsAction::Save),
                        bg.clone(),
                        fg.clone(),
                    ),
                    button(
                        &font,
                        "Save & Connect",
                        UIItemType::SshHosts(SshHostsAction::SaveAndConnect),
                        bg.clone(),
                        fg.clone(),
                    ),
                    button(
                        &font,
                        "Cancel",
                        UIItemType::SshHosts(SshHostsAction::Cancel),
                        bg.clone(),
                        fg.clone(),
                    ),
                ];
                children.push(
                    Element::new(&font, ElementContent::Children(buttons))
                        .display(DisplayType::Block)
                        .padding(cells(0.25, 0.25, 0.4, 0.1)),
                );
            }
        }

        let dimensions = term_window.dimensions;
        let size = term_window.terminal_size;
        let (padding_left, padding_top) = term_window.padding_left_top();
        let top_bar_height = if term_window.show_tab_bar && !term_window.config.tab_bar_at_bottom {
            term_window.tab_bar_pixel_height().unwrap_or(0.)
        } else {
            0.
        };
        let border = term_window.get_os_border();
        let top_pixel_y = top_bar_height + padding_top + border.top.get() as f32;

        let desired_width = (size.cols * 2 / 3).max(80).min(size.cols);
        let avail_pixel_width =
            size.cols as f32 * term_window.render_metrics.cell_size.width as f32;
        let desired_pixel_width =
            desired_width as f32 * term_window.render_metrics.cell_size.width as f32;

        let element = Element::new(&font, ElementContent::Children(children))
            .colors(ElementColors {
                border: BorderColor::new(bg_linear),
                bg: bg.clone(),
                text: fg.clone(),
            })
            .margin(cells(0.5, 0.5, 0.5, 0.5))
            .padding(cells(0.75, 0.75, 0.5, 0.5))
            .border(BoxDimension::new(Dimension::Pixels(1.)))
            .border_corners(Some(rounded_corners()))
            .min_width(Some(Dimension::Pixels(desired_pixel_width)));

        let x_adjust = ((avail_pixel_width - padding_left) - desired_pixel_width) / 2.;

        let computed = term_window.compute_element(
            &LayoutContext {
                height: DimensionContext {
                    dpi: dimensions.dpi as f32,
                    pixel_max: dimensions.pixel_height as f32,
                    pixel_cell: metrics.cell_size.height as f32,
                },
                width: DimensionContext {
                    dpi: dimensions.dpi as f32,
                    pixel_max: dimensions.pixel_width as f32,
                    pixel_cell: metrics.cell_size.width as f32,
                },
                bounds: euclid::rect(
                    padding_left + x_adjust.max(0.),
                    top_pixel_y,
                    desired_pixel_width,
                    size.rows as f32 * term_window.render_metrics.cell_size.height as f32,
                ),
                metrics: &metrics,
                gl_state: term_window.render_state.as_ref().unwrap(),
                zindex: 100,
            },
            &element,
        )?;

        Ok(vec![computed])
    }
}

impl Modal for SshHostsModal {
    fn perform_assignment(
        &self,
        _assignment: &KeyAssignment,
        _term_window: &mut TermWindow,
    ) -> bool {
        false
    }

    fn mouse_event(&self, _event: MouseEvent, _term_window: &mut TermWindow) -> anyhow::Result<()> {
        // Clicks are routed via UIItemType::SshHosts in mouseevent.rs.
        Ok(())
    }

    fn key_down(
        &self,
        key: KeyCode,
        mods: KeyModifiers,
        term_window: &mut TermWindow,
    ) -> anyhow::Result<bool> {
        let in_form = self.is_form();
        match (key, mods) {
            (KeyCode::Escape, _) => {
                if in_form {
                    self.cancel_to_grid();
                } else {
                    term_window.cancel_modal();
                    return Ok(true);
                }
            }
            (KeyCode::Tab, KeyModifiers::NONE) | (KeyCode::DownArrow, KeyModifiers::NONE) => {
                if in_form {
                    self.focus_step(1);
                } else {
                    self.move_selection(1);
                }
            }
            (KeyCode::Tab, KeyModifiers::SHIFT) | (KeyCode::UpArrow, KeyModifiers::NONE) => {
                if in_form {
                    self.focus_step(-1);
                } else {
                    self.move_selection(-1);
                }
            }
            (KeyCode::Enter, _) => {
                if in_form {
                    self.persist_form();
                } else if let Some(id) = self.selected_host_id() {
                    term_window.cancel_modal();
                    term_window.connect_ssh_host(id, None);
                    return Ok(true);
                }
            }
            (KeyCode::Backspace, _) => {
                if in_form {
                    self.form_backspace();
                } else {
                    self.backspace_search();
                }
            }
            (KeyCode::Char(c), KeyModifiers::NONE) | (KeyCode::Char(c), KeyModifiers::SHIFT) => {
                if in_form {
                    self.form_push_char(c);
                } else {
                    self.push_search_char(c);
                }
            }
            _ => return Ok(false),
        }
        term_window.invalidate_modal();
        Ok(true)
    }

    fn computed_element(
        &self,
        term_window: &mut TermWindow,
    ) -> anyhow::Result<Ref<'_, [ComputedElement]>> {
        if self.element.borrow().is_none() {
            let computed = self.compute(term_window)?;
            self.element.borrow_mut().replace(computed);
        }
        Ok(Ref::map(self.element.borrow(), |v| {
            v.as_ref().unwrap().as_slice()
        }))
    }

    fn reconfigure(&self, _term_window: &mut TermWindow) {
        self.element.borrow_mut().take();
    }
}

// --- element helpers -------------------------------------------------------

fn cells(left: f64, right: f64, top: f64, bottom: f64) -> BoxDimension {
    BoxDimension {
        left: Dimension::Cells(left as f32),
        right: Dimension::Cells(right as f32),
        top: Dimension::Cells(top as f32),
        bottom: Dimension::Cells(bottom as f32),
    }
}

fn rounded_corners() -> Corners {
    Corners {
        top_left: SizedPoly {
            width: Dimension::Cells(0.3),
            height: Dimension::Cells(0.3),
            poly: TOP_LEFT_ROUNDED_CORNER,
        },
        top_right: SizedPoly {
            width: Dimension::Cells(0.3),
            height: Dimension::Cells(0.3),
            poly: TOP_RIGHT_ROUNDED_CORNER,
        },
        bottom_left: SizedPoly {
            width: Dimension::Cells(0.3),
            height: Dimension::Cells(0.3),
            poly: BOTTOM_LEFT_ROUNDED_CORNER,
        },
        bottom_right: SizedPoly {
            width: Dimension::Cells(0.3),
            height: Dimension::Cells(0.3),
            poly: BOTTOM_RIGHT_ROUNDED_CORNER,
        },
    }
}

fn text_colors(fg: InheritableColor) -> ElementColors {
    ElementColors {
        border: BorderColor::default(),
        bg: LinearRgba::TRANSPARENT.into(),
        text: fg,
    }
}

fn selected_colors(bg: InheritableColor, fg: InheritableColor) -> ElementColors {
    ElementColors {
        border: BorderColor::default(),
        bg: fg,
        text: bg,
    }
}

fn heading(font: &Rc<wezterm_font::LoadedFont>, text: &str) -> Element {
    Element::new(font, ElementContent::Text(text.to_string()))
        .padding(cells(0.0, 0.0, 0.0, 0.4))
}

fn host_subtitle(spec: &crate::project_sessions::SshHostSpec) -> String {
    let mut parts = vec!["ssh".to_string()];
    if let Some(user) = &spec.username {
        if !user.is_empty() {
            parts.push(user.clone());
        }
    }
    parts.push(spec.host.clone());
    parts.join(", ")
}

#[allow(clippy::too_many_arguments)]
fn action_row(
    font: &Rc<wezterm_font::LoadedFont>,
    icon: SvgIcon,
    label: String,
    _subtitle: String,
    item: UIItemType,
    selected: bool,
    bg: InheritableColor,
    fg: InheritableColor,
) -> Element {
    let row = vec![
        Element::new(
            font,
            ElementContent::Icon {
                icon,
                size: Dimension::Cells(1.2),
            },
        )
        .min_width(Some(Dimension::Cells(2.))),
        Element::new(font, ElementContent::Text(label)),
    ];
    Element::new(font, ElementContent::Children(row))
        .display(DisplayType::Block)
        .item_type(item)
        .colors(if selected {
            selected_colors(bg, fg)
        } else {
            text_colors(fg)
        })
        .padding(cells(0.25, 0.25, 0.2, 0.2))
        .min_width(Some(Dimension::Percent(1.)))
}

#[allow(clippy::too_many_arguments)]
fn host_card(
    font: &Rc<wezterm_font::LoadedFont>,
    project_id: &str,
    label: &str,
    subtitle: &str,
    distro: Option<&str>,
    selected: bool,
    bg: InheritableColor,
    fg: InheritableColor,
) -> Element {
    // Show the detected OS brand logo when known, otherwise a generic server.
    let icon_el = match distro.and_then(distro_to_icon) {
        Some(brand) => Element::new(
            font,
            ElementContent::BrandIcon {
                icon: brand,
                size: Dimension::Cells(1.4),
            },
        ),
        None => Element::new(
            font,
            ElementContent::Icon {
                icon: SvgIcon::Server,
                size: Dimension::Cells(1.4),
            },
        ),
    }
    .min_width(Some(Dimension::Cells(2.5)));

    let text_col = vec![
        Element::new(font, ElementContent::Text(label.to_string())).display(DisplayType::Block),
        Element::new(font, ElementContent::Text(subtitle.to_string())).display(DisplayType::Block),
    ];
    let edit = Element::new(font, ElementContent::Text("  Edit".to_string()))
        .float(Float::Right)
        .item_type(UIItemType::SshHosts(SshHostsAction::Edit(
            project_id.to_string(),
        )));
    let delete = Element::new(font, ElementContent::Text("  Delete".to_string()))
        .float(Float::Right)
        .item_type(UIItemType::SshHosts(SshHostsAction::Delete(
            project_id.to_string(),
        )));
    let row = vec![
        icon_el,
        Element::new(font, ElementContent::Children(text_col)),
        delete,
        edit,
    ];
    Element::new(font, ElementContent::Children(row))
        .display(DisplayType::Block)
        .item_type(UIItemType::SshHosts(SshHostsAction::Connect(
            project_id.to_string(),
        )))
        .colors(if selected {
            selected_colors(bg, fg)
        } else {
            text_colors(fg)
        })
        .padding(cells(0.25, 0.25, 0.25, 0.25))
        .min_width(Some(Dimension::Percent(1.)))
}

fn button(
    font: &Rc<wezterm_font::LoadedFont>,
    label: &str,
    item: UIItemType,
    bg: InheritableColor,
    fg: InheritableColor,
) -> Element {
    Element::new(font, ElementContent::Text(format!("[ {label} ]")))
        .item_type(item)
        .colors(text_colors(fg.clone()))
        .hover_colors(Some(selected_colors(bg, fg)))
        .padding(cells(0.5, 0.5, 0.1, 0.1))
}
