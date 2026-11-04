use super::*;
use crate::connect_access as controller;
use std::collections::BTreeSet;
use thinkterm_reverse_access::{Error as AccessError, GrantScope, Permission};

// Keep the selected-terminal editor available for a future UI iteration.
const SELECT_TERMINALS_IN_UI: bool = false;

#[derive(Default)]
pub(super) struct UiState {
    source: Option<mux::domain::DomainId>,
    host: Option<mux::domain::DomainId>,
    host_chosen: bool,
    editor: Option<u64>,
    panes: BTreeSet<u64>,
    input: bool,
}

impl SettingsWindow {
    pub(super) fn leave_reverse_editor(&mut self) {
        if let Some(id) = self.reverse_access.editor.take() {
            controller::leave(id);
        }
        self.reverse_access.panes.clear();
    }

    pub(super) fn reverse_source_options(&self) -> Vec<(String, SettingsAction, bool)> {
        controller::sources()
            .into_iter()
            .map(|source| {
                let label = if source.eligible {
                    source.name
                } else {
                    format!(
                        "{} — {}",
                        source.name,
                        crate::i18n::tr("settings-reverse-ineligible-short")
                    )
                };
                (
                    label,
                    SettingsAction::ReverseSource(source.id),
                    self.reverse_access.source == Some(source.id),
                )
            })
            .collect()
    }

    pub(super) fn reverse_host_options(&self) -> Vec<(String, SettingsAction, bool)> {
        controller::hosts()
            .into_iter()
            .map(|host| {
                (
                    host.name,
                    SettingsAction::ReverseHost(host.id),
                    self.reverse_access.host == host.id,
                )
            })
            .collect()
    }

    pub(super) fn apply_reverse_action(&mut self, action: SettingsAction) {
        use controller::Control;
        let result = (|| -> thinkterm_reverse_access::Result<()> {
            match action {
                SettingsAction::ReverseSourceMenu | SettingsAction::ReverseHostMenu => {
                    let menu = if action == SettingsAction::ReverseSourceMenu {
                        SettingsDropdown::ReverseSource
                    } else {
                        SettingsDropdown::ReverseHost
                    };
                    self.ui.open_dropdown = (self.ui.open_dropdown != Some(menu)).then_some(menu);
                }
                SettingsAction::ReverseSource(source) => {
                    self.leave_reverse_editor();
                    self.reverse_access.source = Some(source);
                    self.ui.open_dropdown = None;
                }
                SettingsAction::ReverseHost(host) => {
                    self.leave_reverse_editor();
                    self.reverse_access.host = host;
                    self.reverse_access.host_chosen = true;
                    self.ui.open_dropdown = None;
                }
                SettingsAction::ReverseSetup => {
                    self.leave_reverse_editor();
                    let source = self.reverse_access.source.ok_or(AccessError::Unavailable)?;
                    self.reverse_access.editor = Some(if SELECT_TERMINALS_IN_UI {
                        controller::begin(source, self.reverse_access.host)?
                    } else {
                        controller::begin_all_local(
                            source,
                            if self.reverse_access.input {
                                Permission::ReadAndInput
                            } else {
                                Permission::Read
                            },
                            u64::from(self.native_settings.agents.reverse_access_minutes) * 60,
                        )?
                    });
                }
                SettingsAction::ReversePane(pane) => {
                    self.cancel_reverse_authentication();
                    if !self.reverse_access.panes.remove(&pane) {
                        if self.reverse_access.panes.len()
                            >= thinkterm_reverse_access::MAX_TARGETS_PER_GRANT
                        {
                            return Err(AccessError::TooLarge);
                        }
                        self.reverse_access.panes.insert(pane);
                    }
                }
                SettingsAction::ReversePermission(input) => {
                    self.cancel_reverse_authentication();
                    self.reverse_access.input = input;
                }
                SettingsAction::ReverseApprove => {
                    let id = self.reverse_access.editor.ok_or(AccessError::Unavailable)?;
                    if SELECT_TERMINALS_IN_UI && self.reverse_access.panes.is_empty() {
                        return Err(AccessError::Unavailable);
                    }
                    controller::command(
                        id,
                        if SELECT_TERMINALS_IN_UI {
                            Control::Approve {
                                panes: self.reverse_access.panes.iter().copied().collect(),
                                permission: if self.reverse_access.input {
                                    Permission::ReadAndInput
                                } else {
                                    Permission::Read
                                },
                                seconds: u64::from(
                                    self.native_settings.agents.reverse_access_minutes,
                                ) * 60,
                                renewal: None,
                            }
                        } else {
                            Control::ApproveAllLocal {
                                permission: if self.reverse_access.input {
                                    Permission::ReadAndInput
                                } else {
                                    Permission::Read
                                },
                                seconds: u64::from(
                                    self.native_settings.agents.reverse_access_minutes,
                                ) * 60,
                                renewal: None,
                            }
                        },
                    )?;
                }
                SettingsAction::ReverseRefresh => {
                    self.cancel_reverse_authentication();
                    self.reverse_access.panes.clear();
                    controller::command(
                        self.reverse_access.editor.ok_or(AccessError::Unavailable)?,
                        Control::Choices { after: None },
                    )?;
                }
                SettingsAction::ReverseClose(id) => {
                    controller::close(id);
                    if self.reverse_access.editor == Some(id) {
                        self.reverse_access.editor = None;
                        self.reverse_access.panes.clear();
                    }
                }
                SettingsAction::ReverseCancel(id) => controller::command(id, Control::Cancel)?,
                SettingsAction::ReverseRevoke(id, grant) => {
                    controller::command(id, Control::Revoke { grant })?
                }
                SettingsAction::ReverseRenew(id, grant) => {
                    let view = controller::views()
                        .into_iter()
                        .find(|view| view.id == id)
                        .ok_or(AccessError::Unavailable)?;
                    let grant = view
                        .state
                        .status
                        .grants
                        .into_iter()
                        .find(|active| active.grant == grant)
                        .ok_or(AccessError::Unavailable)?;
                    controller::command(
                        id,
                        if grant.scope == GrantScope::SelectedTerminals {
                            Control::Approve {
                                panes: grant.targets.iter().map(|target| target.0).collect(),
                                permission: grant.permission,
                                seconds: u64::from(
                                    self.native_settings.agents.reverse_access_minutes,
                                ) * 60,
                                renewal: Some(grant.grant),
                            }
                        } else {
                            Control::ApproveAllLocal {
                                permission: grant.permission,
                                seconds: u64::from(
                                    self.native_settings.agents.reverse_access_minutes,
                                ) * 60,
                                renewal: Some(grant.grant),
                            }
                        },
                    )?;
                }
                SettingsAction::ReverseMinutes(change) => {
                    let mut settings = crate::native_settings::load();
                    settings.agents.reverse_access_minutes = if change == 0 {
                        30
                    } else {
                        (i32::from(settings.agents.reverse_access_minutes) + i32::from(change))
                            .clamp(1, 120) as u16
                    };
                    crate::native_settings::save(&settings)
                        .map_err(|_| AccessError::Unavailable)?;
                    self.set_native_settings(settings);
                }
                _ => return Err(AccessError::Protocol),
            }
            Ok(())
        })();
        if result.is_err() {
            self.status = crate::i18n::tr("settings-reverse-error");
        }
    }

    fn cancel_reverse_authentication(&self) {
        if let Some(id) = self.reverse_access.editor {
            let _ = controller::command(id, controller::Control::Cancel);
        }
    }

    pub(super) fn paint_reverse_access_card(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        above: f32,
        width: f32,
    ) -> anyhow::Result<f32> {
        let sources = controller::sources();
        let hosts = controller::hosts();
        if !sources
            .iter()
            .any(|source| Some(source.id) == self.reverse_access.source)
        {
            self.reverse_access.source = sources
                .iter()
                .find(|source| source.eligible)
                .or_else(|| sources.first())
                .map(|source| source.id);
        }
        if !self.reverse_access.host_chosen {
            self.reverse_access.host = hosts.first().and_then(|host| host.id);
            self.reverse_access.host_chosen = true;
        }
        let views = controller::views();
        if !SELECT_TERMINALS_IN_UI && self.reverse_access.editor.is_none() {
            self.reverse_access.editor = views
                .iter()
                .find(|view| {
                    Some(view.source_id) == self.reverse_access.source && !view.state.closed
                })
                .map(|view| view.id);
        }
        if let Some(window) = self.window.clone() {
            controller::arm(window);
        }
        let selected = sources
            .iter()
            .find(|source| Some(source.id) == self.reverse_access.source);
        let source_label = selected
            .map(|source| source.name.clone())
            .unwrap_or_else(|| crate::i18n::tr("settings-reverse-no-connection"));
        let host_label = hosts
            .iter()
            .find(|host| host.id == self.reverse_access.host)
            .map(|host| host.name.clone())
            .unwrap_or_else(|| crate::i18n::tr("settings-reverse-unavailable"));
        let eligible = selected.is_some_and(|source| source.eligible);
        let row_x = x + self.ui_px(36.0);
        let row_width = width - self.ui_px(72.0);
        let heading = crate::i18n::tr("settings-reverse-heading");
        let y = self.paint_card_heading(layers, x, above, width, &heading)?;
        // Still being tried, and said so after its heading, as sharing is.
        let body_font = Rc::clone(&self.body_font);
        let heading_width = self.measure_text_width(&body_font, &heading).min(width);
        self.paint_beta_badge(
            layers,
            x + heading_width + self.ui_px(14.0),
            above + self.settings_section_card_gap(),
            x + width,
        )?;
        self.paint_card_as(
            layers,
            x,
            y,
            width,
            CardLayout {
                description_lines: 4,
                ..self.card_layout()
            },
            |this, layers, top| {
                let mut rows = RowCursor::new(top, this);
                rows.add(this.paint_label_row(
                    layers,
                    row_x,
                    rows.y,
                    row_width,
                    &crate::i18n::tr("settings-reverse-intro"),
                    &crate::i18n::tr("settings-reverse-description"),
                    false,
                )?);
                let minutes = this.native_settings.agents.reverse_access_minutes;
                rows.add(this.paint_font_size_stepper_row(
                    layers,
                    row_x,
                    rows.y,
                    row_width,
                    &crate::i18n::tr("settings-reverse-duration"),
                    &crate::i18n::tr("settings-reverse-duration-description"),
                    f64::from(minutes),
                    Some(&settings_tr(
                        "settings-reverse-minutes",
                        &[("minutes", minutes.to_string())],
                    )),
                    SettingsAction::ReverseMinutes(0),
                    SettingsAction::ReverseMinutes(-1),
                    SettingsAction::ReverseMinutes(1),
                    rows.rule(),
                )?);
                rows.add(this.paint_choice_dropdown_row(
                    layers,
                    row_x,
                    rows.y,
                    row_width,
                    rows.rule(),
                    SettingsDropdown::ReverseSource,
                    SettingsAction::ReverseSourceMenu,
                    "settings-reverse-source",
                    source_label,
                )?);
                if !SELECT_TERMINALS_IN_UI
                    && !views.iter().any(|view| {
                        Some(view.id) == this.reverse_access.editor
                            && !view.state.status.grants.is_empty()
                    })
                {
                    let options = vec![
                        (
                            crate::i18n::tr("settings-reverse-read"),
                            SettingsAction::ReversePermission(false),
                        ),
                        (
                            crate::i18n::tr("settings-reverse-input"),
                            SettingsAction::ReversePermission(true),
                        ),
                    ];
                    rows.add(this.paint_segmented_setting_row(
                        layers,
                        row_x,
                        rows.y,
                        row_width,
                        &crate::i18n::tr("settings-reverse-permission"),
                        &crate::i18n::tr("settings-reverse-permission-description"),
                        &options,
                        usize::from(this.reverse_access.input),
                        rows.rule(),
                    )?);
                }
                if eligible {
                    if SELECT_TERMINALS_IN_UI {
                        rows.add(this.paint_choice_dropdown_row(
                            layers,
                            row_x,
                            rows.y,
                            row_width,
                            rows.rule(),
                            SettingsDropdown::ReverseHost,
                            SettingsAction::ReverseHostMenu,
                            "settings-reverse-host",
                            host_label,
                        )?);
                    }
                    if this.reverse_access.editor.is_none() {
                        rows.add(this.paint_action_setting_row(
                            layers,
                            row_x,
                            rows.y,
                            row_width,
                            &crate::i18n::tr(if SELECT_TERMINALS_IN_UI {
                                "settings-reverse-select"
                            } else {
                                "settings-reverse-all-local"
                            }),
                            &crate::i18n::tr(if SELECT_TERMINALS_IN_UI {
                                "settings-reverse-select-description"
                            } else {
                                "settings-reverse-all-description"
                            }),
                            &crate::i18n::tr(if SELECT_TERMINALS_IN_UI {
                                "settings-reverse-choose"
                            } else {
                                "settings-reverse-authenticate"
                            }),
                            SettingsAction::ReverseSetup,
                            rows.rule(),
                        )?);
                    }
                } else {
                    rows.add(this.paint_label_row(
                        layers,
                        row_x,
                        rows.y,
                        row_width,
                        &crate::i18n::tr(if selected.is_some() {
                            "settings-reverse-ineligible-short"
                        } else {
                            "settings-reverse-all-local"
                        }),
                        &crate::i18n::tr(if selected.is_some() {
                            "settings-reverse-ineligible"
                        } else {
                            "settings-reverse-connect-first"
                        }),
                        rows.rule(),
                    )?);
                }
                if let Some(view) = views
                    .iter()
                    .find(|view| Some(view.id) == this.reverse_access.editor)
                {
                    let active: BTreeSet<_> = view
                        .state
                        .status
                        .grants
                        .iter()
                        .flat_map(|grant| grant.targets.iter().map(|target| target.0))
                        .collect();
                    this.reverse_access
                        .panes
                        .retain(|pane| !active.contains(pane));
                    if view.starting {
                        rows.add(this.paint_label_row(
                            layers,
                            row_x,
                            rows.y,
                            row_width,
                            &crate::i18n::tr("settings-reverse-loading"),
                            "",
                            rows.rule(),
                        )?);
                    } else if !view.state.closed
                        && view.state.error.is_none()
                        && (SELECT_TERMINALS_IN_UI || view.state.status.grants.is_empty())
                    {
                        if SELECT_TERMINALS_IN_UI {
                            for pane in &view.state.choices {
                                if active.contains(&pane.pane) {
                                    continue;
                                }
                                rows.add(this.paint_toggle_setting_row(
                                    layers,
                                    row_x,
                                    rows.y,
                                    row_width,
                                    &settings_tr(
                                        "settings-reverse-pane",
                                        &[("pane", pane.pane.to_string())],
                                    ),
                                    &pane.title,
                                    this.reverse_access.panes.contains(&pane.pane),
                                    SettingsAction::ReversePane(pane.pane),
                                    rows.rule(),
                                )?);
                            }
                        }
                        if SELECT_TERMINALS_IN_UI {
                            let options = vec![
                                (
                                    crate::i18n::tr("settings-reverse-read"),
                                    SettingsAction::ReversePermission(false),
                                ),
                                (
                                    crate::i18n::tr("settings-reverse-input"),
                                    SettingsAction::ReversePermission(true),
                                ),
                            ];
                            rows.add(this.paint_segmented_setting_row(
                                layers,
                                row_x,
                                rows.y,
                                row_width,
                                &crate::i18n::tr("settings-reverse-permission"),
                                &crate::i18n::tr("settings-reverse-permission-description"),
                                &options,
                                usize::from(this.reverse_access.input),
                                rows.rule(),
                            )?);
                        }
                        if (!SELECT_TERMINALS_IN_UI || !this.reverse_access.panes.is_empty())
                            && !view.state.status.authenticating
                        {
                            rows.add(this.paint_action_setting_row(
                                layers,
                                row_x,
                                rows.y,
                                row_width,
                                &crate::i18n::tr(if SELECT_TERMINALS_IN_UI {
                                    "settings-reverse-approve"
                                } else {
                                    "settings-reverse-all-local"
                                }),
                                &crate::i18n::tr("settings-reverse-approve-description"),
                                &crate::i18n::tr("settings-reverse-authenticate"),
                                SettingsAction::ReverseApprove,
                                rows.rule(),
                            )?);
                        }
                        if SELECT_TERMINALS_IN_UI {
                            rows.add(this.paint_action_setting_row(
                                layers,
                                row_x,
                                rows.y,
                                row_width,
                                &crate::i18n::tr("settings-reverse-refresh"),
                                &crate::i18n::tr("settings-reverse-refresh-description"),
                                &crate::i18n::tr("settings-reverse-refresh-button"),
                                SettingsAction::ReverseRefresh,
                                rows.rule(),
                            )?);
                        }
                    }
                }
                for view in &views {
                    let heading = if SELECT_TERMINALS_IN_UI {
                        format!("{} · {}", view.source, view.host)
                    } else {
                        view.source.clone()
                    };
                    if view.state.status.authenticating {
                        rows.add(this.paint_action_setting_row(
                            layers,
                            row_x,
                            rows.y,
                            row_width,
                            &heading,
                            &crate::i18n::tr("settings-reverse-authenticating"),
                            &crate::i18n::tr("settings-reverse-cancel"),
                            SettingsAction::ReverseCancel(view.id),
                            rows.rule(),
                        )?);
                    }
                    for grant in &view.state.status.grants {
                        let seconds = grant.remaining_ms / 1000;
                        let scope = settings_tr(
                            if grant.scope == GrantScope::AllLocalTerminals {
                                "settings-reverse-all-active"
                            } else {
                                "settings-reverse-active"
                            },
                            &[
                                ("count", grant.targets.len().to_string()),
                                (
                                    "panes",
                                    grant
                                        .targets
                                        .iter()
                                        .map(|target| target.0.to_string())
                                        .collect::<Vec<_>>()
                                        .join(", "),
                                ),
                                ("time", format!("{}:{:02}", seconds / 60, seconds % 60)),
                                (
                                    "permission",
                                    crate::i18n::tr(if grant.permission == Permission::Read {
                                        "settings-reverse-read"
                                    } else {
                                        "settings-reverse-input"
                                    }),
                                ),
                            ],
                        );
                        rows.add(this.paint_action_setting_row(
                            layers,
                            row_x,
                            rows.y,
                            row_width,
                            &heading,
                            &scope,
                            &crate::i18n::tr("settings-reverse-revoke"),
                            SettingsAction::ReverseRevoke(view.id, grant.grant),
                            rows.rule(),
                        )?);
                        rows.add(this.paint_action_setting_row(
                            layers,
                            row_x,
                            rows.y,
                            row_width,
                            &crate::i18n::tr("settings-reverse-renew"),
                            &crate::i18n::tr("settings-reverse-renew-description"),
                            &crate::i18n::tr("settings-reverse-authenticate"),
                            SettingsAction::ReverseRenew(view.id, grant.grant),
                            rows.rule(),
                        )?);
                    }
                    if view.state.closed
                        || view.state.error.is_some()
                        || view.state.status.error.is_some()
                    {
                        let message =
                            crate::i18n::tr(if view.state.closed && view.state.error.is_none() {
                                "settings-reverse-ended"
                            } else {
                                "settings-reverse-error"
                            });
                        if view.state.status.grants.is_empty() {
                            rows.add(this.paint_action_setting_row(
                                layers,
                                row_x,
                                rows.y,
                                row_width,
                                &heading,
                                &message,
                                &crate::i18n::tr("settings-reverse-dismiss"),
                                SettingsAction::ReverseClose(view.id),
                                rows.rule(),
                            )?);
                        } else {
                            // The grant is fine: only this message goes.
                            rows.add(this.paint_action_setting_row(
                                layers,
                                row_x,
                                rows.y,
                                row_width,
                                &heading,
                                &message,
                                &crate::i18n::tr("settings-reverse-dismiss"),
                                SettingsAction::ReverseCancel(view.id),
                                rows.rule(),
                            )?);
                        }
                    }
                }
                Ok(rows.bottom)
            },
        )
    }
}
