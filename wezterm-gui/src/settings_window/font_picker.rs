use super::*;

const PAGE_SIZE: usize = 5;

#[derive(Debug, Clone, Default)]
pub(super) struct FontCatalog {
    names: Option<Vec<String>>,
    generation: u64,
    loading: bool,
}

impl FontCatalog {
    pub(super) fn invalidate(&mut self) {
        self.names = None;
        self.generation = self.generation.wrapping_add(1);
    }

    fn begin(&mut self) -> Option<u64> {
        if self.loading || self.names.is_some() {
            return None;
        }
        self.loading = true;
        Some(self.generation)
    }

    fn finish(
        &mut self,
        generation: u64,
        result: anyhow::Result<Vec<String>>,
    ) -> anyhow::Result<bool> {
        self.loading = false;
        if generation != self.generation {
            return Ok(false);
        }
        self.names = Some(result?);
        Ok(true)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FontTarget {
    Interface,
    Terminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PreviewKey {
    dpi: usize,
    generation: usize,
    system_fallback: bool,
}

impl PreviewKey {
    fn new(fonts: &FontConfiguration, config: &config::ConfigHandle) -> Self {
        Self {
            dpi: fonts.get_dpi(),
            generation: config.generation(),
            system_fallback: fonts.config().fontconfig_prefer_system_fallback,
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct FontPicker {
    pub target: FontTarget,
    pub search: TextInputState,
    pub draft: Option<String>,
    pub page: usize,
    preview: Option<Rc<LoadedFont>>,
    preview_key: Option<PreviewKey>,
    filtered: Vec<usize>,
    filter_query: Option<String>,
    catalog_len: usize,
}

impl FontPicker {
    fn new(target: FontTarget, draft: Option<String>) -> Self {
        Self {
            target,
            search: TextInputState::new(),
            draft,
            page: 0,
            preview: None,
            preview_key: None,
            filtered: Vec::new(),
            filter_query: None,
            catalog_len: 0,
        }
    }

    pub(super) fn clear_preview(&mut self) {
        self.preview = None;
        self.preview_key = None;
    }

    fn load_preview(&mut self, fonts: &FontConfiguration, config: &config::ConfigHandle) {
        let key = PreviewKey::new(fonts, config);
        if self.preview_key == Some(key) {
            return;
        }
        let loaded = match self.target {
            FontTarget::Interface => fonts.ui_font_preview(self.draft.as_deref(), 14.0),
            FontTarget::Terminal => {
                let config = config.adjusted(|adjusted| {
                    if let Some(family) = &self.draft {
                        adjusted.font =
                            crate::native_settings::terminal_font_style(config, family);
                    }
                });
                fonts.terminal_font_uncached(&config, 14.0)
            }
        };
        let loaded = loaded.and_then(|font| {
            if let Some(family) = &self.draft {
                let requested = config::FontAttributes::new(family);
                anyhow::ensure!(
                    font.clone_handles()
                        .iter()
                        .any(|font| font.matches_name(&requested)),
                    "font unavailable"
                );
            }
            Ok(font)
        });
        self.preview_key = Some(key);
        self.preview = loaded.ok();
    }
}

fn ranked_indices(names: &[String], query: &str, target: FontTarget) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    let mut found: Vec<usize> = names
        .iter()
        .enumerate()
        .filter(|(_, name)| name.to_lowercase().contains(&query))
        .map(|(index, _)| index)
        .collect();
    if target == FontTarget::Terminal {
        // Keep the familiar terminal families near the top without hiding
        // other installed choices. This is a suggestion, not a font filter.
        found.sort_by_cached_key(|&index| {
            let name = names[index].to_lowercase();
            !(name.contains("mono")
                || matches!(
                    name.as_str(),
                    "menlo" | "consolas" | "courier new" | "cascadia code"
                ))
        });
    }
    found
}

impl SettingsWindow {
    pub(super) fn open_font_picker(&mut self, target: FontTarget) {
        let was_open = self
            .ui
            .font_picker
            .as_ref()
            .is_some_and(|picker| picker.target == target);
        self.close_font_picker();
        if was_open {
            return;
        }
        let draft = match target {
            FontTarget::Interface => self.native_settings.chrome.ui_font_family.clone(),
            FontTarget::Terminal => self.native_settings.terminal.font_family.clone(),
        };
        self.ui.font_picker = Some(FontPicker::new(target, draft));
        self.set_focused_input(Some(SettingsAction::FontPickerSearch));
        self.load_font_catalog();
    }

    pub(super) fn invalidate_font_catalog(&mut self) {
        self.ui.font_catalog.invalidate();
        if let Some(picker) = &mut self.ui.font_picker {
            picker.filter_query = None;
            picker.filtered.clear();
            self.load_font_catalog();
        }
    }

    fn load_font_catalog(&mut self) {
        let Some(generation) = self.ui.font_catalog.begin() else {
            return;
        };
        let job = self.fonts.font_family_catalog_job();
        let instance_id = self.instance_id;
        std::thread::spawn(move || {
            let result = job();
            promise::spawn::spawn_into_main_thread(async move {
                if let Some(settings) = settings_window_for_instance(instance_id) {
                    let mut settings = settings.borrow_mut();
                    match settings.ui.font_catalog.finish(generation, result) {
                        Ok(true) => {}
                        Ok(false) => {
                            if settings.ui.font_picker.is_some() {
                                settings.load_font_catalog();
                            }
                            return;
                        }
                        Err(error) => {
                            log::warn!("font family catalogue: {error:#}");
                            settings.status = crate::i18n::tr("settings-font-list-error");
                        }
                    }
                    if let Some(window) = settings.window.as_ref() {
                        window.invalidate();
                    }
                }
            })
            .detach();
        });
    }

    pub(super) fn close_font_picker(&mut self) {
        if self.ui.font_picker.take().is_some() {
            self.invalidate_shaped_text();
            self.ui.stale_glyphs = true;
        }
        if self.ui.interaction.focused == Some(SettingsAction::FontPickerSearch) {
            self.set_focused_input(None);
        }
    }

    pub(super) fn choose_font(&mut self, index: Option<(u16, u64)>) {
        let family = match index {
            None => None,
            Some((index, generation)) => {
                if generation != self.ui.font_catalog.generation {
                    return;
                }
                let Some(name) = self
                    .ui
                    .font_catalog
                    .names
                    .as_ref()
                    .and_then(|names| names.get(index as usize))
                else {
                    return;
                };
                Some(name.clone())
            }
        };
        if let Some(picker) = &mut self.ui.font_picker {
            picker.draft = family;
            picker.clear_preview();
        }
        self.invalidate_shaped_text();
        self.ui.stale_glyphs = true;
    }

    pub(super) fn apply_font_picker(&mut self) {
        let Some(picker) = &self.ui.font_picker else {
            return;
        };
        if picker.preview_key != Some(PreviewKey::new(&self.fonts, &configuration()))
            || picker.preview.is_none()
        {
            return;
        }
        let target = picker.target;
        let family = picker.draft.clone();
        let mut terminal_font_changed = false;
        let result = match target {
            FontTarget::Interface => crate::native_settings::update_ui_state(|settings| {
                settings.chrome.ui_font_family = family;
            }),
            FontTarget::Terminal => crate::native_settings::update(|settings| {
                terminal_font_changed = settings.terminal.font_family != family;
                settings.terminal.font_family = family;
            })
            .map(drop),
        };
        match result {
            Ok(()) => {
                self.close_font_picker();
                let saved = crate::native_settings::load();
                if target == FontTarget::Interface {
                    crate::native_settings::apply_ui_font_to_app(&saved);
                } else {
                    crate::native_settings::apply_to_app_with_font_changes(
                        &saved,
                        crate::native_settings::FontChanges {
                            family: terminal_font_changed,
                            ..Default::default()
                        },
                    );
                }
                self.set_native_settings(saved);
                self.status = crate::i18n::tr("settings-font-applied");
            }
            Err(error) => {
                self.status = settings_tr(
                    "settings-status-terminal-error",
                    &[("error", format!("{error:#}"))],
                );
            }
        }
    }

    pub(super) fn paint_font_choice(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        target: FontTarget,
        rule: bool,
    ) -> anyhow::Result<f32> {
        let title = crate::i18n::tr(match target {
            FontTarget::Interface => "settings-interface-font",
            FontTarget::Terminal => "settings-terminal-font-family",
        });
        let description = crate::i18n::tr(match target {
            FontTarget::Interface => "settings-interface-font-description",
            FontTarget::Terminal => "settings-font-choice-description",
        });
        let current = match target {
            FontTarget::Interface => self.native_settings.chrome.ui_font_family.as_deref(),
            FontTarget::Terminal => self.native_settings.terminal.font_family.as_deref(),
        }
        .map(str::to_string)
        .unwrap_or_else(|| crate::i18n::tr("settings-font-default"));
        if rule {
            self.paint_separator(layers, x, y - self.ui_px(28.0), width)?;
        }
        let (cx, cy, cw) = self.dropdown_control_geometry(x, y, width);
        let control = self.dropdown_pill_rect(cx, cy, cw, &current);
        let action = SettingsAction::OpenFontPicker(target);
        let palette = self.palette();
        let open = self
            .ui
            .font_picker
            .as_ref()
            .is_some_and(|picker| picker.target == target);
        let hovered = self.ui.interaction.hovered == Some(action);
        self.ui_context.push(control, WidgetKind::Button, action);
        self.paint_dropdown_pill(
            layers,
            control,
            &current,
            if hovered {
                palette.control_hover_bg
            } else {
                palette.control_bg
            },
            if open {
                palette.nav_selected_bg
            } else {
                palette.control_border
            },
        )?;
        let font = Rc::clone(&self.ui_font);
        let text_width = (cx - x - self.ui_px(24.0)).max(width * 0.45);
        self.draw_text(layers, &font, x, y, &title, palette.text, text_width)?;
        let extra = self.draw_row_description(layers, x, y, &description, text_width)?;
        if !self
            .ui
            .font_picker
            .as_ref()
            .is_some_and(|picker| picker.target == target)
        {
            return Ok(extra);
        }
        let top = y + self.settings_row_visual_height() + extra + self.ui_px(20.0);
        let bottom = self.paint_font_picker(layers, x, top, width)?;
        Ok(bottom - (y + self.settings_row_visual_height()))
    }

    fn paint_font_picker(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        top: f32,
        width: f32,
    ) -> anyhow::Result<f32> {
        let config = configuration();
        if self.fonts.config().generation() != config.generation() {
            self.reload_configured_fonts(&config)?;
        }
        let picker = self.ui.font_picker.as_ref().unwrap();
        let query = picker.search.text().to_string();
        let target = picker.target;
        let draft = picker.draft.clone();
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let row_height = self.dropdown_row_height();
        let gap = self.ui_px(20.0);
        let padding = self.ui_px(16.0);
        let side_by_side = width >= self.ui_px(800.0);
        let list_width = if side_by_side {
            (width * 0.44).min(self.ui_px(520.0))
        } else {
            width
        };
        let search_rect = rect(x, top, list_width, self.ui_px(CONTROL_HEIGHT));
        self.paint_text_input(
            layers,
            0,
            TextInputSpec {
                text: &query,
                placeholder: &crate::i18n::tr("settings-font-search"),
                rect: search_rect,
                focused: self.ui.interaction.focused == Some(SettingsAction::FontPickerSearch),
                selected_all: self.ui.font_picker.as_ref().unwrap().search.selected_all,
                action: SettingsAction::FontPickerSearch,
            },
        )?;
        self.draw_svg_icon(
            layers,
            SvgIcon::Search,
            x + self.ui_px(18.0),
            top + self.ui_px(16.0),
            self.ui_px(24.0),
            palette.muted_text,
        )?;
        let list_y = search_rect.max_y() + self.ui_px(12.0);
        let list_height = row_height * (PAGE_SIZE + 1) as f32 + padding * 2.0;
        let card = self.floating_card_palette();
        self.draw_rounded_frame(
            layers,
            0,
            x,
            list_y,
            list_width,
            list_height,
            card.control_bg,
            card.control_border,
            self.ui_px(DROPDOWN_RADIUS),
        )?;
        let picker = self.ui.font_picker.as_mut().unwrap();
        let names = self.ui.font_catalog.names.as_deref().unwrap_or(&[]);
        if picker.filter_query.as_deref() != Some(query.as_str())
            || picker.catalog_len != names.len()
        {
            picker.filtered = ranked_indices(names, &query, target);
            picker.filter_query = Some(query.clone());
            picker.catalog_len = names.len();
        }
        let result_count = picker.filtered.len();
        let last_page = result_count.saturating_sub(1) / PAGE_SIZE;
        let page = picker.page.min(last_page);
        picker.page = page;
        let indices: Vec<usize> = picker
            .filtered
            .iter()
            .skip(page * PAGE_SIZE)
            .take(PAGE_SIZE)
            .copied()
            .collect();
        let mut options = vec![(
            crate::i18n::tr("settings-font-default"),
            SettingsAction::FontPickerDefault,
            draft.is_none(),
        )];
        for &index in &indices {
            let name = names[index].clone();
            let selected = draft.as_deref() == Some(name.as_str());
            options.push((
                name,
                SettingsAction::FontPickerChoose(index as u16, self.ui.font_catalog.generation),
                selected,
            ));
        }
        for (index, (label, action, selected)) in options.iter().enumerate() {
            let row = rect(
                x,
                list_y + padding + index as f32 * row_height,
                list_width,
                row_height,
            );
            if *selected {
                self.draw_rounded_rect(
                    layers,
                    0,
                    row.min_x() + self.ui_px(8.0),
                    row.min_y() + self.ui_px(2.0),
                    row.size.width - self.ui_px(16.0),
                    row.size.height - self.ui_px(4.0),
                    crate::ui::color::mix(card.control_bg, card.text, 0.10),
                    self.ui_px(DROPDOWN_ROW_RADIUS),
                )?;
            }
            let label_y = self.control_text_y(row.min_y(), row_height);
            let color = self
                .paint_dropdown_row_on_layer(layers, &card, row, label_y, *action, *selected, 0)?;
            let inset = self.dropdown_label_inset();
            let label = self.text_with_ellipsis(&ui_font, label, list_width - inset - padding);
            self.draw_text(
                layers,
                &ui_font,
                x + inset,
                label_y,
                &label,
                color,
                list_width - inset - padding,
            )?;
        }
        if result_count == 0 {
            self.draw_text(
                layers,
                &ui_font,
                x + padding,
                list_y + padding + row_height + gap,
                &crate::i18n::tr(if self.ui.font_catalog.loading {
                    "settings-font-loading"
                } else {
                    "settings-font-no-results"
                }),
                palette.muted_text,
                list_width - padding * 2.0,
            )?;
        }
        let pagination_y = list_y + list_height + self.ui_px(8.0);
        let small = self.ui_px(44.0);
        self.paint_icon_button(
            layers,
            x,
            pagination_y,
            small,
            SvgIcon::ArrowLeft,
            SettingsAction::FontPickerPage(-1),
        )?;
        self.paint_icon_button(
            layers,
            x + list_width - small,
            pagination_y,
            small,
            SvgIcon::ArrowRight,
            SettingsAction::FontPickerPage(1),
        )?;
        self.draw_text(
            layers,
            &ui_font,
            x + small + padding,
            self.control_text_y(pagination_y, small),
            &format!("{} / {}", page + 1, last_page + 1),
            palette.muted_text,
            list_width - small * 2.0 - padding * 2.0,
        )?;
        let list_bottom = pagination_y + small;
        let preview_x = if side_by_side {
            x + list_width + gap
        } else {
            x
        };
        let preview_y = if side_by_side { top } else { list_bottom + gap };
        let preview_width = if side_by_side {
            width - list_width - gap
        } else {
            width
        };
        let preview_height = if side_by_side {
            list_bottom - top
        } else {
            self.ui_px(270.0)
        };
        self.draw_rounded_frame(
            layers,
            0,
            preview_x,
            preview_y,
            preview_width,
            preview_height,
            palette.card_bg,
            palette.control_border,
            self.ui_px(DROPDOWN_RADIUS),
        )?;
        self.draw_text(
            layers,
            &ui_font,
            preview_x + padding,
            preview_y + padding,
            &crate::i18n::tr("settings-font-preview"),
            palette.muted_text,
            preview_width - padding * 2.0,
        )?;
        self.draw_text(
            layers,
            &ui_font,
            preview_x + padding,
            preview_y + padding + self.ui_px(38.0),
            &draft
                .clone()
                .unwrap_or_else(|| crate::i18n::tr("settings-font-default")),
            palette.text,
            preview_width - padding * 2.0,
        )?;
        self.ui
            .font_picker
            .as_mut()
            .unwrap()
            .load_preview(&self.fonts, &config);
        let preview = self.ui.font_picker.as_ref().unwrap().preview.clone();
        let mut y = preview_y + self.ui_px(120.0);
        if let Some(font) = preview {
            let samples = match target {
                FontTarget::Interface => [
                    "ThinkTerm",
                    "Aa Bb 0123456789",
                    "中文预览 · 日本語",
                    "O0 Il1  → ≠ {} []",
                ],
                FontTarget::Terminal => [
                    "~/project $ git status",
                    "On branch main",
                    "中文预览 · 日本語",
                    "O0 Il1  → ≠ {} []",
                ],
            };
            for text in samples {
                self.draw_text(
                    layers,
                    &font,
                    preview_x + padding,
                    y,
                    text,
                    palette.text,
                    preview_width - padding * 2.0,
                )?;
                y += (font.metrics().cell_height.get() as f32).max(self.ui_px(34.0));
            }
        } else {
            self.draw_text(
                layers,
                &ui_font,
                preview_x + padding,
                y,
                &crate::i18n::tr("settings-font-unavailable"),
                palette.muted_text,
                preview_width - padding * 2.0,
            )?;
        }
        let footer_y = list_bottom.max(preview_y + preview_height) + gap;
        let apply_label = crate::i18n::tr("settings-font-apply");
        let cancel_label = crate::i18n::tr("settings-font-cancel");
        let apply_width = self
            .button_width_for_label(&apply_label, 130.0)
            .min(width * 0.35);
        let cancel_width = self
            .button_width_for_label(&cancel_label, 130.0)
            .min(width * 0.35);
        let apply_x = x + width - apply_width;
        self.draw_button(
            layers,
            apply_x - gap - cancel_width,
            footer_y,
            cancel_width,
            &cancel_label,
            SettingsAction::FontPickerCancel,
        )?;
        if self.ui.font_picker.as_ref().unwrap().preview.is_none() {
            self.draw_button_enabled(
                layers,
                apply_x,
                footer_y,
                apply_width,
                &apply_label,
                SettingsAction::FontPickerApply,
                false,
            )?;
        } else {
            let ctx = crate::ui::draw::DrawContext::new(
                self.render_state
                    .as_ref()
                    .context("settings render state")?,
                self.dimensions,
                &self.metrics,
            );
            let action = SettingsAction::FontPickerApply;
            let state = if self.ui.interaction.pressed == Some(action) {
                ControlState::Pressed
            } else if self.ui.interaction.hovered == Some(action) {
                ControlState::Hovered
            } else {
                ControlState::Normal
            };
            let button_rect = rect(apply_x, footer_y, apply_width, self.ui_px(CONTROL_HEIGHT));
            crate::ui::widgets::draw_button_on_layer(
                &ctx,
                layers,
                &ui_font,
                &mut self.ui_context,
                self.chrome_palette,
                ButtonSpec {
                    label: &apply_label,
                    rect: button_rect,
                    action,
                    state,
                    kind: WidgetKind::Button,
                    variant: ButtonVariant::Primary,
                },
                0,
            )?;
        }
        Ok(footer_y + self.ui_px(CONTROL_HEIGHT) + gap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_drops_old_directory_results_and_releases_loaded_names() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../assets/fonts/FiraCode-Regular.ttf");
        std::fs::copy(source, directory.path().join("example.ttf"))?;
        let empty = config::ConfigHandle::default_config().adjusted(|cfg| {
            cfg.font_locator = config::FontLocatorSelection::ConfigDirsOnly;
            cfg.font_dirs.clear();
        });
        let added = empty.adjusted(|cfg| cfg.font_dirs.push(directory.path().into()));
        let fonts = FontConfiguration::new(Some(empty.clone()), 96)?;
        let mut catalog = FontCatalog::default();
        let old = catalog.begin().unwrap();
        let old_job = fonts.font_family_catalog_job();
        fonts.config_changed(&added)?;
        catalog.invalidate();
        assert!(catalog.begin().is_none());
        assert!(!catalog.finish(old, old_job())?);
        let fresh = catalog.begin().unwrap();
        assert!(catalog.finish(fresh, fonts.font_family_catalog_job()())?);
        assert!(catalog
            .names
            .as_ref()
            .unwrap()
            .iter()
            .any(|family| family == "Fira Code"));
        assert!(catalog.begin().is_none());
        fonts.config_changed(&empty)?;
        catalog.invalidate();
        assert!(catalog.names.is_none());
        let fresh = catalog.begin().unwrap();
        assert!(catalog.finish(fresh, fonts.font_family_catalog_job()())?);
        assert!(!catalog
            .names
            .as_ref()
            .unwrap()
            .iter()
            .any(|family| family == "Fira Code"));
        Ok(())
    }

    #[test]
    fn default_preview_tracks_config_and_fallback_at_the_same_dpi() -> anyhow::Result<()> {
        let _turn = wezterm_font::harfbuzz_turn();
        let configs = config::Configuration::new();
        let with_family = |family: &str| {
            configs.get().adjusted(|cfg| {
                cfg.font_locator = config::FontLocatorSelection::ConfigDirsOnly;
                cfg.font_dirs = vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../assets/fonts")];
                cfg.font.set_primary_family(family);
            })
        };
        let base = with_family("Roboto");
        let fonts = FontConfiguration::new(Some(base.clone()), 96)?;
        let mut picker = FontPicker::new(FontTarget::Terminal, None);
        picker.load_preview(&fonts, &base);
        let original = picker.preview.clone().expect("initial preview");
        assert_eq!(original.clone_handles()[0].names().family, "Roboto");

        configs.use_defaults();
        let reloaded = with_family("JetBrains Mono");
        fonts.config_changed(&reloaded)?;
        picker.load_preview(&fonts, &reloaded);
        let changed = picker.preview.clone().expect("preview after reload");
        assert!(!Rc::ptr_eq(&original, &changed));
        assert_eq!(changed.clone_handles()[0].names().family, "JetBrains Mono");

        let policy = reloaded.adjusted(|cfg| cfg.fontconfig_prefer_system_fallback = true);
        fonts.config_changed(&policy)?;
        picker.load_preview(&fonts, &reloaded);
        let fallback = picker
            .preview
            .as_ref()
            .expect("preview after fallback change");
        assert!(!Rc::ptr_eq(&changed, fallback));
        Ok(())
    }

    #[test]
    fn font_preview_tracks_display_dpi_without_reloading_at_the_same_scale() -> anyhow::Result<()> {
        let _turn = wezterm_font::harfbuzz_turn();
        let config = config::ConfigHandle::default_config();
        for target in [FontTarget::Interface, FontTarget::Terminal] {
            let fonts = FontConfiguration::new(Some(config.clone()), 96)?;
            let mut picker = FontPicker::new(target, Some("JetBrains Mono".into()));
            picker.load_preview(&fonts, &config);
            let original = picker.preview.clone().expect("font preview");
            picker.load_preview(&fonts, &config);
            assert!(Rc::ptr_eq(&original, picker.preview.as_ref().unwrap()));

            fonts.change_scaling(fonts.get_font_scale(), 192);
            picker.load_preview(&fonts, &config);
            let scaled = picker.preview.clone().expect("scaled font preview");
            assert!(!Rc::ptr_eq(&original, &scaled));
            assert!(scaled.metrics().cell_height > original.metrics().cell_height);

            fonts.change_scaling(fonts.get_font_scale(), 96);
            picker.load_preview(&fonts, &config);
            let restored = picker.preview.as_ref().expect("restored font preview");
            assert_eq!(
                restored.metrics().cell_height,
                original.metrics().cell_height
            );
            assert!(!Rc::ptr_eq(&scaled, restored));
        }
        Ok(())
    }

    #[test]
    fn search_preserves_catalog_indices_and_prioritizes_terminal_families() {
        let names = vec![
            "Example Sans".into(),
            "Example Mono".into(),
            "Other Face".into(),
        ];
        assert_eq!(
            ranked_indices(&names, "example", FontTarget::Terminal),
            vec![1, 0]
        );
        assert_eq!(
            ranked_indices(&names, "example", FontTarget::Interface),
            vec![0, 1]
        );
        assert!(ranked_indices(&names, "absent", FontTarget::Terminal).is_empty());
    }
}
