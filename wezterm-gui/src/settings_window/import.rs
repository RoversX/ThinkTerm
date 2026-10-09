use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ImportStep {
    Source,
    Review,
    Result,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ImportSource {
    WezTerm,
    /// The folders VS Code and the editors built from it have opened.
    Editors,
    #[cfg(unix)]
    Session(&'static str),
}

/// What first-run setup and the Import page call the editors' source.
pub(crate) const EDITORS_SOURCE: &str = "editors";

impl ImportSource {
    pub(super) fn all() -> Vec<Self> {
        let mut sources = vec![Self::WezTerm];
        if !crate::editor_projects::installed().is_empty() {
            sources.push(Self::Editors);
        }
        #[cfg(unix)]
        sources.extend(
            thinkterm_import::sources()
                .into_iter()
                .map(|source| Self::Session(source.id)),
        );
        sources
    }

    /// The source a caller outside Settings names: an import source's id, or
    /// `None` for WezTerm's settings.
    pub(super) fn named(id: Option<&str>) -> Option<Self> {
        match id {
            None => Some(Self::WezTerm),
            Some(EDITORS_SOURCE) => Some(Self::Editors),
            #[cfg(unix)]
            Some(id) => thinkterm_import::sources()
                .into_iter()
                .find(|source| source.id == id)
                .map(|source| Self::Session(source.id)),
            #[cfg(not(unix))]
            Some(_) => None,
        }
    }

    pub(super) fn initial() -> Self {
        #[cfg(unix)]
        if let Some(source) = session_import::remembered_source() {
            return source;
        }
        let selected = std::env::var("THINKTERM_SETTINGS_SECTION").ok();
        if selected.as_deref() == Some(EDITORS_SOURCE) {
            return Self::Editors;
        }
        #[cfg(unix)]
        if let Some(selected) = selected {
            if let Some(source) = thinkterm_import::sources()
                .into_iter()
                .find(|s| s.id.eq_ignore_ascii_case(&selected))
            {
                return Self::Session(source.id);
            }
        }
        Self::WezTerm
    }

    /// Session imports take over another program's live terminals, which
    /// is new enough to carry a Beta badge.
    fn is_beta(self) -> bool {
        match self {
            Self::WezTerm | Self::Editors => false,
            #[cfg(unix)]
            Self::Session(_) => true,
        }
    }

    pub(super) fn label(self) -> String {
        match self {
            Self::WezTerm => "WezTerm".to_string(),
            Self::Editors => crate::i18n::tr("settings-import-editors"),
            #[cfg(unix)]
            Self::Session(id) => thinkterm_import::source(id)
                .map(|s| s.info().name)
                .unwrap_or(id)
                .to_string(),
        }
    }

    fn description(self) -> String {
        crate::i18n::tr(match self {
            Self::WezTerm => "settings-import-wezterm-description",
            Self::Editors => "settings-import-editors-description",
            #[cfg(unix)]
            Self::Session(_) => "settings-import-session-description",
        })
    }

    fn icon(self) -> Option<BrandIcon> {
        match self {
            Self::WezTerm => Some(BrandIcon::WezTerm),
            // Drawn as its editors' marks, see `paint_editor_stack`.
            Self::Editors => None,
            #[cfg(unix)]
            Self::Session(id) => {
                BrandIcon::for_import_source(thinkterm_import::source(id).ok()?.info().icon)
            }
        }
    }

    #[cfg(unix)]
    pub(super) fn provider(self) -> anyhow::Result<&'static dyn thinkterm_import::ImportSource> {
        match self {
            Self::Session(id) => thinkterm_import::source(id),
            Self::WezTerm => anyhow::bail!("The selected source imports settings, not sessions"),
            Self::Editors => anyhow::bail!("The selected source imports folders, not sessions"),
        }
    }

    pub(super) fn text(self, key: &str) -> String {
        self.text_args(key, &[])
    }

    pub(super) fn text_args(self, key: &str, values: &[(&str, String)]) -> String {
        let mut args = FluentArgs::new();
        for (name, value) in values {
            args.set(*name, value.as_str());
        }
        let label = self.label();
        args.set("source", label.as_str());
        crate::i18n::tr_args(key, &args)
    }
}

pub(super) struct ImportButton {
    pub(super) label: String,
    pub(super) action: SettingsAction,
    pub(super) primary: bool,
    pub(super) enabled: bool,
}

struct ImportSourceLayout {
    columns: usize,
    card_width: f32,
    card_height: f32,
    total_height: f32,
    descriptions: Vec<Vec<String>>,
}

impl SettingsWindow {
    pub(super) fn import_busy(&self) -> bool {
        #[cfg(unix)]
        if matches!(self.ui.import_source, ImportSource::Session(_)) {
            return self.ui.session_import.busy();
        }
        false
    }

    /// Choose `source` and take the page's first step, as clicking it and
    /// Continue would. An import in flight keeps the page.
    pub(super) fn start_import(&mut self, source: ImportSource, window: &Window) {
        if self.import_busy() {
            return;
        }
        self.perform_action(SettingsAction::SelectImportSource(source), window);
        self.perform_import_navigation(SettingsAction::ImportContinue, window);
    }

    pub(super) fn perform_import_navigation(&mut self, action: SettingsAction, window: &Window) {
        if self.import_busy() {
            return;
        }
        self.ui.content_scroll.reset();
        self.ui.open_dropdown = None;
        match action {
            SettingsAction::ImportContinue => {
                self.ui.import_step = ImportStep::Review;
                self.ui.import_result = None;
                match self.ui.import_source {
                    ImportSource::WezTerm => {
                        self.perform_action(SettingsAction::LoadWezTermSource, window)
                    }
                    ImportSource::Editors => {
                        let space = OPENED_FROM_SPACE.with(|slot| slot.borrow().clone());
                        self.start_editor_import(space);
                    }
                    #[cfg(unix)]
                    ImportSource::Session(_) => {
                        self.perform_session_import_action(SettingsAction::SessionImportDetect)
                    }
                }
            }
            SettingsAction::ImportBack => {
                #[cfg(unix)]
                if matches!(self.ui.import_source, ImportSource::Session(_))
                    && self.ui.session_import.has_preview()
                {
                    self.perform_session_import_action(SettingsAction::SessionImportBack);
                    window.invalidate();
                    return;
                }
                self.ui.import_step = ImportStep::Source;
            }
            SettingsAction::ImportStartOver => {
                self.ui.import_step = ImportStep::Source;
                self.ui.import_result = None;
                self.ui.editor_import.release();
                #[cfg(unix)]
                {
                    self.ui.session_import.abandon();
                    self.ui.session_import = session_import::ImportUi::default();
                }
            }
            _ => {}
        }
        window.invalidate();
    }

    pub(super) fn paint_import(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        width: f32,
    ) -> anyhow::Result<()> {
        let measure = width.min(self.ui_px(1040.0));
        let x = x + (width - measure) / 2.0;
        let width = measure;
        let scroll = self.ui.content_scroll.offset;
        let step_bottom =
            self.paint_import_steps(layers, x, self.ui_px(CONTENT_TITLE_Y) - scroll, width)?;
        let top = step_bottom + self.ui_px(64.0);
        let bottom = match self.ui.import_step {
            ImportStep::Source => {
                let title = crate::i18n::tr("settings-import-choose-title");
                let description = crate::i18n::tr("settings-import-description");
                let layout = self.import_source_layout(width);
                let heading_height = self.import_heading_height(width, &title, &description);
                let body_height = heading_height + self.ui_px(40.0) + layout.total_height;
                let body_end = self.content_bottom() - scroll - self.ui_px(172.0);
                let centered_top = top + ((body_end - top - body_height) / 2.0).max(0.0);
                let heading = self.paint_import_heading(
                    layers,
                    x,
                    centered_top,
                    width,
                    &title,
                    &description,
                )?;
                let body = self.paint_import_sources(
                    layers,
                    x,
                    heading + self.ui_px(40.0),
                    width,
                    &layout,
                )?;
                self.paint_import_footer(
                    layers,
                    x,
                    body + self.ui_px(40.0),
                    width,
                    None,
                    &[ImportButton {
                        label: crate::i18n::tr("settings-import-continue"),
                        action: SettingsAction::ImportContinue,
                        primary: true,
                        enabled: true,
                    }],
                )?
            }
            ImportStep::Review => match self.ui.import_source {
                ImportSource::WezTerm => self.paint_wezterm_import(layers, x, top, width)?,
                ImportSource::Editors => self.paint_editor_import(layers, x, top, width)?,
                #[cfg(unix)]
                ImportSource::Session(_) => self.paint_session_import(layers, x, top, width)?,
            },
            ImportStep::Result => match self.ui.import_source {
                ImportSource::WezTerm => {
                    let message = self.ui.import_result.clone().unwrap_or_default();
                    let bottom = self.paint_import_result(
                        layers,
                        x,
                        top + self.ui_px(64.0),
                        width,
                        &message,
                    )?;
                    self.paint_import_footer(
                        layers,
                        x,
                        bottom + self.ui_px(40.0),
                        width,
                        Some(SettingsAction::ImportStartOver),
                        &[ImportButton {
                            label: crate::i18n::tr("settings-open-thinkterm-config"),
                            action: SettingsAction::OpenThinkTermConfigFile,
                            primary: true,
                            enabled: true,
                        }],
                    )?
                }
                ImportSource::Editors => self.paint_editor_result(layers, x, top, width)?,
                #[cfg(unix)]
                ImportSource::Session(_) => self.paint_session_result(layers, x, top, width)?,
            },
        };
        self.ui.content_scroll.set_extents(
            self.content_viewport_extent(),
            self.settings_content_extent(bottom + scroll),
        );
        if self.ui.content_scroll.offset != scroll {
            if let Some(window) = &self.window {
                window.invalidate();
            }
        }
        Ok(())
    }

    fn paint_import_steps(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<f32> {
        let labels = [
            "settings-import-step-source",
            "settings-import-step-review",
            "settings-import-step-result",
        ]
        .map(crate::i18n::tr);
        let active = match self.ui.import_step {
            ImportStep::Source => 0,
            ImportStep::Review => 1,
            ImportStep::Result => 2,
        };
        let font = Rc::clone(&self.ui_font);
        let cell = RenderMetrics::with_font_metrics(&font.metrics())
            .cell_size
            .height as f32;
        let circle = self.ui_px(48.0);
        let gap = self.ui_px(16.0);
        let connector = self.ui_px(64.0);
        let widths: Vec<_> = labels
            .iter()
            .map(|label| circle + gap + self.measure_text_width(&font, label))
            .collect();
        let total = widths.iter().sum::<f32>() + connector * 2.0;
        if total > width {
            let label = format!("{} / 3 · {}", active + 1, labels[active]);
            self.draw_text(
                layers,
                &font,
                x,
                y,
                &label,
                self.palette().secondary_text,
                width,
            )?;
            return Ok(y + cell);
        }
        let mut left = x + (width - total) / 2.0;
        for (index, label) in labels.iter().enumerate() {
            let color = if index <= active {
                self.chrome_palette.accent
            } else {
                self.palette().separator
            };
            self.draw_rounded_frame(
                layers,
                0,
                left,
                y,
                circle,
                circle,
                if index <= active {
                    color
                } else {
                    self.palette().control_bg
                },
                color,
                circle / 2.0,
            )?;
            if index < active {
                self.draw_svg_icon(
                    layers,
                    SvgIcon::Check,
                    left + circle * 0.25,
                    y + circle * 0.25,
                    circle * 0.5,
                    self.palette().on_accent,
                )?;
            } else {
                let number = (index + 1).to_string();
                self.draw_import_step_text(
                    layers,
                    &font,
                    left,
                    y,
                    circle,
                    circle,
                    &number,
                    if index == active {
                        self.palette().on_accent
                    } else {
                        self.palette().secondary_text
                    },
                )?;
            }
            self.draw_import_step_text(
                layers,
                &font,
                left + circle + gap,
                y,
                widths[index] - circle - gap,
                circle,
                label,
                if index == active {
                    self.palette().title
                } else {
                    self.palette().secondary_text
                },
            )?;
            left += widths[index];
            if index < 2 {
                self.draw_rect(
                    layers,
                    0,
                    left + self.ui_px(16.0),
                    y + (circle - self.ui_px(1.0)) / 2.0,
                    connector - self.ui_px(32.0),
                    self.ui_px(1.0),
                    self.palette().separator,
                )?;
                left += connector;
            }
        }
        Ok(y + circle)
    }

    fn draw_import_step_text(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        text: &str,
        color: LinearRgba,
    ) -> anyhow::Result<()> {
        let Some((left, top, right, bottom)) = self.import_ink_bounds(font, text) else {
            return Ok(());
        };
        let baseline = self.metrics.cell_size.height as f32 + self.metrics.descender.get() as f32;
        self.draw_text(
            layers,
            font,
            x + (width - (right - left)) / 2.0 - left,
            y + (height - (bottom - top)) / 2.0 - top - baseline,
            text,
            color,
            width.max(self.measure_text_width(font, text)),
        )
    }

    fn import_ink_bounds(&self, font: &Rc<LoadedFont>, text: &str) -> Option<(f32, f32, f32, f32)> {
        let shaped = self.shaped_text(font, text)?;
        let mut bounds: Option<(f32, f32, f32, f32)> = None;
        let mut advance = 0.0;
        for glyph in &shaped.glyphs {
            if let Some(texture) = &glyph.texture {
                let left = advance + (glyph.x_offset + glyph.bearing_x).get() as f32;
                let top = -(glyph.y_offset + glyph.bearing_y).get() as f32;
                let right = left + texture.coords.size.width as f32 * glyph.scale as f32;
                let bottom = top + texture.coords.size.height as f32 * glyph.scale as f32;
                bounds = Some(match bounds {
                    Some((l, t, r, b)) => (l.min(left), t.min(top), r.max(right), b.max(bottom)),
                    None => (left, top, right, bottom),
                });
            }
            advance += glyph.x_advance.get() as f32;
        }
        bounds
    }

    pub(super) fn draw_import_line(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        text: &str,
        color: LinearRgba,
    ) -> anyhow::Result<()> {
        let Some((left, top, _, bottom)) = self.import_ink_bounds(font, text) else {
            return Ok(());
        };
        let baseline = self.metrics.cell_size.height as f32 + self.metrics.descender.get() as f32;
        self.draw_text(
            layers,
            font,
            x - left,
            y + (height - (bottom - top)) / 2.0 - top - baseline,
            text,
            color,
            width,
        )
    }

    fn import_heading_height(&self, width: f32, title: &str, description: &str) -> f32 {
        let title_height = RenderMetrics::with_font_metrics(&self.import_title_font.metrics())
            .cell_size
            .height as f32;
        let title_lines = self
            .wrap_settings_text(&self.import_title_font, title, width)
            .len();
        let body_lines: usize = description
            .split('\n')
            .map(|line| {
                self.wrap_settings_text(&self.import_body_font, line, width)
                    .len()
            })
            .sum();
        title_height * title_lines as f32
            + self.ui_px(4.0)
            + self.import_line_step() * body_lines as f32
    }

    fn paint_import_heading(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        title: &str,
        description: &str,
    ) -> anyhow::Result<f32> {
        let font = Rc::clone(&self.import_title_font);
        let step = RenderMetrics::with_font_metrics(&font.metrics())
            .cell_size
            .height as f32;
        let mut top = y;
        for line in self.wrap_settings_text(&font, title, width) {
            self.draw_text(
                layers,
                &font,
                x + ((width - self.measure_text_width(&font, &line)) / 2.0).max(0.0),
                top,
                &line,
                self.palette().title,
                width,
            )?;
            top += step;
        }
        self.paint_import_centered_copy(layers, x, top + self.ui_px(4.0), width, description)
    }

    pub(super) fn paint_import_centered_copy(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        text: &str,
    ) -> anyhow::Result<f32> {
        let font = Rc::clone(&self.import_body_font);
        let mut top = y;
        for paragraph in text.split('\n') {
            for line in self.wrap_settings_text(&font, paragraph, width) {
                self.draw_text(
                    layers,
                    &font,
                    x + ((width - self.measure_text_width(&font, &line)) / 2.0).max(0.0),
                    top,
                    &line,
                    self.palette().secondary_text,
                    width,
                )?;
                top += self.import_line_step();
            }
        }
        Ok(top)
    }

    pub(super) fn paint_import_source_heading(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        title: &str,
        description: &str,
    ) -> anyhow::Result<f32> {
        let mark = self.ui_px(88.0).min(width * 0.2);
        // The editors stand side by side, each on a tile as big as another
        // source's mark.
        let editors = (self.ui.import_source == ImportSource::Editors)
            .then(|| self.ui.editor_import.kinds());
        let mark_width = editors.as_ref().map_or(mark, |kinds| {
            crate::editor_projects::stack_width(kinds.len(), mark)
        });
        let gap = self.ui_px(24.0);
        let left = x + mark_width + gap;
        let inner = (width - mark_width - gap).max(1.0);
        let title_font = Rc::clone(&self.title_font);
        let body_font = Rc::clone(&self.import_body_font);
        let title_lines = self.wrap_settings_text(&title_font, title, inner);
        let body_lines = self.wrap_settings_text(&body_font, description, inner);
        let line_height = |font, lines: &[String], minimum| {
            lines
                .iter()
                .filter_map(|line| self.import_ink_bounds(font, line))
                .map(|(_, top, _, bottom)| bottom - top)
                .fold(self.ui_px(minimum), f32::max)
                + self.ui_px(8.0)
        };
        let title_height = line_height(&title_font, &title_lines, 32.0);
        let body_height = line_height(&body_font, &body_lines, 24.0);
        let text_height = title_height * title_lines.len() as f32
            + self.ui_px(4.0)
            + body_height * body_lines.len() as f32;
        let height = mark.max(text_height);
        let mark_y = y + (height - mark) / 2.0;
        if let Some(kinds) = &editors {
            self.paint_editor_stack(layers, kinds, x, mark_y, mark, self.palette().window_bg)?;
        } else if let Some(icon) = self.ui.import_source.icon() {
            self.draw_brand_icon(layers, icon, x, mark_y, mark)?;
        } else {
            self.draw_svg_icon(
                layers,
                SvgIcon::Terminal,
                x,
                mark_y,
                mark,
                self.palette().text,
            )?;
        }
        let mut top = y + (height - text_height) / 2.0;
        for line in title_lines {
            self.draw_import_line(
                layers,
                &title_font,
                left,
                top,
                inner,
                title_height,
                &line,
                self.palette().title,
            )?;
            top += title_height;
        }
        top += self.ui_px(4.0);
        for line in body_lines {
            self.draw_import_line(
                layers,
                &body_font,
                left,
                top,
                inner,
                body_height,
                &line,
                self.palette().secondary_text,
            )?;
            top += body_height;
        }
        Ok(y + height)
    }

    pub(super) fn paint_import_result(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        message: &str,
    ) -> anyhow::Result<f32> {
        let side = self.ui_px(88.0);
        self.draw_svg_icon(
            layers,
            SvgIcon::CircleCheck,
            x + (width - side) / 2.0,
            y,
            side,
            TileColor::Green.linear(),
        )?;
        self.paint_import_heading(
            layers,
            x,
            y + side + self.ui_px(32.0),
            width,
            &crate::i18n::tr("session-import-status-done"),
            message,
        )
    }

    pub(super) fn paint_import_footer(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        body_bottom: f32,
        width: f32,
        back: Option<SettingsAction>,
        buttons: &[ImportButton],
    ) -> anyhow::Result<f32> {
        let top = body_bottom
            .max(self.content_bottom() - self.ui.content_scroll.offset - self.ui_px(132.0));
        self.draw_rect(
            layers,
            0,
            x,
            top,
            width,
            self.ui_px(1.0),
            self.palette().separator.mul_alpha(0.7),
        )?;
        let button_y = top + self.ui_px(24.0);
        let mut action_x = x;
        let mut action_y = button_y;
        let mut action_width = width;
        if let Some(action) = back {
            let label = crate::i18n::tr(if action == SettingsAction::ImportStartOver {
                "settings-import-again"
            } else {
                "session-import-back"
            });
            let button_width = self.button_width_for_label(&label, 0.0).min(width);
            let right_width: f32 = buttons
                .iter()
                .map(|button| self.import_button_width(button))
                .sum::<f32>()
                + self.ui_px(12.0) * buttons.len().saturating_sub(1) as f32;
            self.paint_import_actions(
                layers,
                x,
                button_y,
                button_width,
                &[ImportButton {
                    label,
                    action,
                    primary: false,
                    enabled: !self.import_busy(),
                }],
            )?;
            if right_width + button_width + self.ui_px(24.0) <= width {
                action_x += button_width + self.ui_px(24.0);
                action_width -= button_width + self.ui_px(24.0);
            } else {
                action_y += self.ui_px(CONTROL_HEIGHT + 12.0);
            }
        }
        Ok(
            self.paint_import_actions(layers, action_x, action_y, action_width, buttons)?
                + self.ui_px(48.0),
        )
    }

    fn import_source_layout(&self, width: f32) -> ImportSourceLayout {
        let sources = ImportSource::all();
        let columns = if width >= self.ui_px(600.0) {
            sources.len().min(3)
        } else {
            1
        };
        let gap = self.ui_px(28.0);
        let card_width =
            ((width - gap * (columns - 1) as f32) / columns as f32).min(self.ui_px(360.0));
        let pad = self.ui_px(36.0);
        let descriptions: Vec<_> = sources
            .iter()
            .map(|source| {
                self.wrap_settings_text(
                    &self.import_body_font,
                    &source.description(),
                    (card_width - pad * 2.0).max(1.0),
                )
            })
            .collect();
        let card_height = pad * 2.0
            + self.ui_px(144.0 + 24.0 + 8.0)
            + self.metrics.cell_size.height as f32
            + self.import_line_step() * descriptions.iter().map(Vec::len).max().unwrap_or(1) as f32;
        let rows = sources.len().div_ceil(columns);
        ImportSourceLayout {
            columns,
            card_width,
            card_height,
            total_height: card_height * rows as f32 + gap * (rows - 1) as f32,
            descriptions,
        }
    }

    fn paint_import_sources(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        layout: &ImportSourceLayout,
    ) -> anyhow::Result<f32> {
        let sources = ImportSource::all();
        let palette = self.palette();
        let heading = Rc::clone(&self.ui_font);
        let body = Rc::clone(&self.import_body_font);
        let cell = self.metrics.cell_size.height as f32;
        let step = self.import_line_step();
        let pad = self.ui_px(36.0);
        let gap = self.ui_px(28.0);
        let icon = self.ui_px(144.0);
        let columns = layout.columns;
        let card_width = layout.card_width;
        let height = layout.card_height;
        let descriptions = &layout.descriptions;
        let text_width = (card_width - pad * 2.0).max(1.0);
        let start = x + (width - card_width * columns as f32 - gap * (columns - 1) as f32) / 2.0;
        for (index, source) in sources.iter().enumerate() {
            let left = start + (index % columns) as f32 * (card_width + gap);
            let top = y + (index / columns) as f32 * (height + gap);
            let action = SettingsAction::SelectImportSource(*source);
            let selected = *source == self.ui.import_source;
            let ground = if self.ui.interaction.pressed == Some(action) {
                palette.control_pressed_bg
            } else if self.ui.interaction.hovered == Some(action) {
                palette.control_hover_bg
            } else if selected {
                palette.card_bg
            } else {
                palette.window_bg
            };
            self.ui_context.push(
                rect(left, top, card_width, height),
                WidgetKind::Button,
                action,
            );
            self.draw_rounded_frame(
                layers,
                0,
                left,
                top,
                card_width,
                height,
                ground,
                if selected {
                    self.chrome_palette.accent
                } else {
                    palette.separator
                },
                self.ui_px(32.0),
            )?;
            if *source == ImportSource::Editors {
                let kinds = crate::editor_projects::stack_kinds(
                    crate::editor_projects::installed().iter().copied(),
                );
                let tile = (icon * 0.6).round();
                let span = crate::editor_projects::stack_width(kinds.len(), tile);
                self.paint_editor_stack(
                    layers,
                    &kinds,
                    left + (card_width - span) / 2.0,
                    top + pad + (icon - tile) / 2.0,
                    tile,
                    ground,
                )?;
            } else if let Some(brand) = source.icon() {
                self.draw_brand_icon(
                    layers,
                    brand,
                    left + (card_width - icon) / 2.0,
                    top + pad,
                    icon,
                )?;
            } else {
                self.draw_svg_icon(
                    layers,
                    SvgIcon::Terminal,
                    left + (card_width - icon) / 2.0,
                    top + pad,
                    icon,
                    palette.muted_text,
                )?;
            }
            let title_y = top + pad + icon + self.ui_px(24.0);
            let label = source.label();
            let title_width = self.measure_text_width(&heading, &label);
            // The title and its badge are centered as one line.
            let badge_gap = self.ui_px(14.0);
            let badge_width = if source.is_beta() {
                badge_gap + self.beta_badge_width()
            } else {
                0.0
            };
            let title_x = left + ((card_width - title_width - badge_width) / 2.0).max(pad);
            self.draw_text(
                layers,
                &heading,
                title_x,
                title_y,
                &label,
                palette.title,
                text_width,
            )?;
            if source.is_beta() {
                self.paint_beta_badge(
                    layers,
                    title_x + title_width.min(text_width) + badge_gap,
                    title_y,
                    left + card_width - pad,
                )?;
            }
            for (row, line) in descriptions[index].iter().enumerate() {
                self.draw_text(
                    layers,
                    &body,
                    left + ((card_width - self.measure_text_width(&body, line)) / 2.0).max(pad),
                    title_y + cell + self.ui_px(8.0) + step * row as f32,
                    line,
                    palette.secondary_text,
                    text_width,
                )?;
            }
            if selected {
                let side = self.ui_px(28.0);
                let mark_x = left + card_width - side - self.ui_px(20.0);
                let mark_y = top + self.ui_px(20.0);
                self.draw_rounded_rect(
                    layers,
                    1,
                    mark_x,
                    mark_y,
                    side,
                    side,
                    self.chrome_palette.accent,
                    side / 2.0,
                )?;
                self.draw_svg_icon(
                    layers,
                    SvgIcon::Check,
                    mark_x + self.ui_px(6.0),
                    mark_y + self.ui_px(6.0),
                    side - self.ui_px(12.0),
                    palette.on_accent,
                )?;
            }
        }
        Ok(y + layout.total_height)
    }

    fn paint_wezterm_import(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let source = self.selected_wezterm_source_path();
        let destination = Self::thinkterm_compatible_config_path();
        let fields = self.compatibility_import.fields.clone();
        let selected_count = fields
            .iter()
            .filter(|field| field.lua_value.is_some() && self.import_field_selected(field.id))
            .count();
        let error = self.compatibility_import.error.clone();
        let left = x;
        let inner = width;
        let top = y;
        let font = Rc::clone(&self.ui_font);
        let cell = self.metrics.cell_size.height as f32;
        let mut bottom = self.paint_import_source_heading(
            layers,
            left,
            top,
            inner,
            &crate::i18n::tr("settings-import-wezterm-title"),
            &crate::i18n::tr("settings-compatibility-description"),
        )?;
        bottom += self.ui_px(28.0);
        let columns = if inner >= self.ui_px(840.0) { 2 } else { 1 };
        let gap = self.ui_px(56.0);
        let column_width = (inner - gap * (columns - 1) as f32) / columns as f32;
        let source_path = source
            .as_ref()
            .map(|path| home_relative(path))
            .unwrap_or_else(|| crate::i18n::tr("settings-wezterm-source-missing"));
        let paths = [
            (
                "WezTerm",
                source_path,
                crate::i18n::tr(if source.is_some() {
                    "common-found"
                } else {
                    "common-missing"
                }),
                SettingsAction::OpenWezTermConfigFile,
                source.is_some(),
            ),
            (
                "ThinkTerm",
                home_relative(&destination),
                crate::i18n::tr(if destination.exists() {
                    "settings-import-config-ready"
                } else {
                    "settings-import-config-will-create"
                }),
                SettingsAction::OpenThinkTermConfigFile,
                true,
            ),
        ];
        let path_lines = paths
            .iter()
            .map(|(_, path, ..)| {
                self.wrap_settings_text(&self.import_body_font, path, column_width)
                    .len()
            })
            .max()
            .unwrap_or(1);
        let mut row_bottom = bottom;
        for (index, (name, path, status, action, enabled)) in paths.iter().enumerate() {
            let column_x = left + (index % columns) as f32 * (column_width + gap);
            let column_y = if columns == 1 && index > 0 {
                row_bottom + self.ui_px(24.0)
            } else {
                bottom
            };
            self.draw_text(
                layers,
                &font,
                column_x,
                column_y,
                name,
                palette.text,
                column_width,
            )?;
            let mut path_bottom = self.paint_import_copy(
                layers,
                column_x,
                column_y + cell + self.ui_px(8.0),
                column_width,
                path,
                palette.secondary_text,
            )?;
            if columns == 2 {
                path_bottom = path_bottom.max(
                    column_y + cell + self.ui_px(8.0) + path_lines as f32 * self.import_line_step(),
                );
            }
            let block_bottom = self.paint_import_link(
                layers,
                column_x,
                path_bottom + self.ui_px(12.0),
                column_width,
                status,
                *action,
                *enabled,
            )?;
            row_bottom = row_bottom.max(block_bottom);
        }
        if columns == 2 {
            let side = self.ui_px(28.0);
            self.draw_svg_icon(
                layers,
                SvgIcon::ArrowRight,
                left + column_width + (gap - side) / 2.0,
                (bottom + row_bottom - side) / 2.0,
                side,
                palette.muted_text,
            )?;
        }
        bottom = row_bottom + self.ui_px(24.0);
        if let Some(error) = &error {
            bottom = self.paint_import_notice(layers, left, bottom, inner, error, true)?
                + self.ui_px(24.0);
        }
        if !fields.is_empty() {
            self.paint_separator(layers, left, bottom, inner)?;
            bottom += self.ui_px(24.0);
            let mut args = FluentArgs::new();
            args.set("selected", selected_count);
            args.set("total", fields.len());
            bottom = self.paint_import_copy(
                layers,
                left,
                bottom,
                inner,
                &crate::i18n::tr_args("settings-fields-selected", &args),
                palette.secondary_text,
            )?;
            let buttons = [
                ImportButton {
                    label: crate::i18n::tr("common-select-all"),
                    action: SettingsAction::SelectAllImportFields,
                    enabled: true,
                    primary: false,
                },
                ImportButton {
                    label: crate::i18n::tr("common-clear"),
                    action: SettingsAction::ClearImportFields,
                    enabled: true,
                    primary: false,
                },
            ];
            bottom = self.paint_import_actions(
                layers,
                left,
                bottom + self.ui_px(12.0),
                inner,
                &buttons,
            )?;
            let mut rows = RowCursor::new(bottom + self.ui_px(36.0), self);
            let mut category = None;
            for (index, field) in fields.iter().enumerate() {
                let group_start = category != Some(field.category.as_str());
                if group_start {
                    if index > 0 {
                        rows.y += self.ui_px(16.0);
                    }
                    self.draw_text(
                        layers,
                        &Rc::clone(&self.import_body_font),
                        left,
                        rows.y,
                        &field.category,
                        palette.secondary_text,
                        inner,
                    )?;
                    rows.y += self.import_line_step() + self.ui_px(24.0);
                    category = Some(field.category.as_str());
                }
                self.paint_import_field_row(layers, left, rows.y, inner, field, !group_start)?;
                rows.add(0.0);
            }
            bottom = rows.bottom + self.ui_px(24.0);
        }
        let mut buttons = vec![ImportButton {
            label: crate::i18n::tr("settings-import-reload"),
            action: SettingsAction::LoadWezTermSource,
            enabled: source.is_some(),
            primary: fields.is_empty(),
        }];
        if !fields.is_empty() {
            buttons.push(ImportButton {
                label: crate::i18n::tr("settings-import-selected"),
                action: SettingsAction::ImportSelectedFields,
                enabled: source.is_some() && selected_count > 0,
                primary: true,
            });
        }
        self.paint_import_footer(
            layers,
            left,
            bottom + self.ui_px(32.0),
            inner,
            Some(SettingsAction::ImportBack),
            &buttons,
        )
    }

    fn paint_import_link(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        status: &str,
        action: SettingsAction,
        enabled: bool,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let font = Rc::clone(&self.import_body_font);
        let cell = self.metrics.cell_size.height as f32;
        let label = crate::i18n::tr("settings-import-open");
        let icon = self.ui_px(18.0);
        let gap = self.ui_px(8.0);
        let link_width = self.measure_text_width(&font, &label) + gap + icon;
        let left = x + width - link_width;
        self.draw_text(
            layers,
            &font,
            x,
            y,
            status,
            palette.muted_text,
            (width - link_width - gap).max(1.0),
        )?;
        if enabled {
            self.ui_context.push(
                rect(
                    left - gap,
                    y - gap,
                    link_width + gap * 2.0,
                    cell + gap * 2.0,
                ),
                WidgetKind::Button,
                action,
            );
            if self.ui.interaction.hovered == Some(action) {
                self.draw_rect(
                    layers,
                    0,
                    left,
                    y + cell,
                    link_width,
                    self.ui_px(1.0),
                    self.chrome_palette.accent,
                )?;
            }
        }
        let color = if enabled {
            self.chrome_palette.accent
        } else {
            palette.muted_text
        };
        self.draw_text(
            layers,
            &font,
            left,
            y,
            &label,
            color,
            link_width - icon - gap,
        )?;
        self.draw_svg_icon(
            layers,
            SvgIcon::ExternalLink,
            x + width - icon,
            y + (cell - icon) / 2.0,
            icon,
            color,
        )?;
        Ok(y + cell)
    }

    pub(super) fn import_line_step(&self) -> f32 {
        RenderMetrics::with_font_metrics(&self.import_body_font.metrics())
            .cell_size
            .height as f32
            + self.ui_px(5.0)
    }

    pub(super) fn paint_import_copy(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        text: &str,
        color: LinearRgba,
    ) -> anyhow::Result<f32> {
        let font = Rc::clone(&self.import_body_font);
        let step = self.import_line_step();
        let mut top = y;
        for paragraph in text.split('\n') {
            for line in self.wrap_settings_text(&font, paragraph, width.max(1.0)) {
                self.draw_text(layers, &font, x, top, &line, color, width.max(1.0))?;
                top += step;
            }
            if paragraph.is_empty() {
                top += self.ui_px(8.0);
            }
        }
        Ok(top)
    }

    pub(super) fn paint_import_notice(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        text: &str,
        warning: bool,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let mut buffer = HeapQuadAllocator::default();
        let pad = self.ui_px(24.0);
        let icon = self.ui_px(20.0);
        let inset = pad * 2.0 + icon;
        let bottom = self.paint_import_copy(
            &mut TripleLayerQuadAllocator::Heap(&mut buffer),
            x + inset,
            y + pad,
            width - inset - pad,
            text,
            palette.secondary_text,
        )? + pad;
        self.draw_rounded_rect(
            layers,
            0,
            x,
            y,
            width,
            bottom - y,
            if warning {
                TileColor::Orange.linear().mul_alpha(0.10)
            } else {
                palette.control_bg
            },
            self.ui_px(24.0),
        )?;
        self.draw_svg_icon(
            layers,
            if warning {
                SvgIcon::CircleAlert
            } else {
                SvgIcon::Info
            },
            x + pad,
            y + pad,
            icon,
            if warning {
                TileColor::Orange.linear()
            } else {
                palette.muted_text
            },
        )?;
        buffer.apply_to(layers)?;
        Ok(bottom)
    }

    pub(super) fn paint_import_actions(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        buttons: &[ImportButton],
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let gap = self.ui_px(12.0);
        let height = self.ui_px(CONTROL_HEIGHT);
        let widths: Vec<_> = buttons
            .iter()
            .map(|button| self.import_button_width(button).min(width))
            .collect();
        let (places, rows) = flow_right(&widths, width, gap);
        for ((button, button_width), (offset, row)) in buttons.iter().zip(widths).zip(places) {
            let left = x + offset;
            let top = y + row as f32 * (height + gap);
            let ImportButton {
                label,
                action,
                primary,
                enabled,
            } = button;
            let state = if !enabled {
                ControlState::Disabled
            } else if self.ui.interaction.pressed == Some(*action) {
                ControlState::Pressed
            } else if self.ui.interaction.hovered == Some(*action) {
                ControlState::Hovered
            } else {
                ControlState::Normal
            };
            let (mut ground, mut border) = state.colors(self.chrome_palette);
            if *primary && *enabled {
                ground = match state {
                    ControlState::Pressed => self.chrome_palette.accent.mul_alpha(0.70),
                    ControlState::Hovered => self.chrome_palette.accent.mul_alpha(0.85),
                    _ => self.chrome_palette.accent,
                };
                border = ground;
            }
            if *enabled {
                self.ui_context.push(
                    rect(left, top, button_width, height),
                    WidgetKind::Button,
                    *action,
                );
            }
            self.draw_rounded_frame(
                layers,
                0,
                left,
                top,
                button_width,
                height,
                ground,
                border,
                height / 2.0,
            )?;
            let font = Rc::clone(&self.ui_font);
            let icon = *action == SettingsAction::ImportContinue;
            let extra = if icon { self.ui_px(32.0) } else { 0.0 };
            let label_width = self.measure_text_width(&font, label);
            let text_x = left + ((button_width - label_width - extra) / 2.0).max(self.ui_px(12.0));
            let text_color = if !enabled {
                palette.muted_text
            } else if *primary {
                palette.on_accent
            } else {
                palette.text
            };
            self.draw_import_line(
                layers,
                &font,
                text_x,
                top,
                (left + button_width - self.ui_px(12.0) - text_x - extra).max(1.0),
                height,
                label,
                text_color,
            )?;
            if icon {
                let side = self.ui_px(20.0);
                self.draw_svg_icon(
                    layers,
                    SvgIcon::ArrowRight,
                    (text_x + label_width + self.ui_px(12.0))
                        .min(left + button_width - side - self.ui_px(12.0)),
                    top + (height - side) / 2.0,
                    side,
                    text_color,
                )?;
            }
        }
        Ok(y + rows as f32 * height + rows.saturating_sub(1) as f32 * gap)
    }

    fn import_button_width(&self, button: &ImportButton) -> f32 {
        self.button_width_for_label(&button.label, 0.0)
            + if button.action == SettingsAction::ImportContinue {
                self.ui_px(32.0)
            } else {
                0.0
            }
    }
}
