//! Settings → Import from code editors: the folders VS Code and the editors
//! built from it opened, ticked, and added as projects to a local Space.
//! What the editors opened is read as the review step opens, on a thread of
//! its own -- each folder is looked up on disk, and one on a network share
//! that has gone away can take seconds -- and let go when the page is left.

use super::*;
use crate::editor_projects::{self, EditorKind, Scan};
use crate::workspace_threads::ResolvedProjects;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EditorImportAction {
    /// Show one editor's folders, or with `None` every editor's.
    Filter(Option<u8>),
    /// Tick or untick a row, by its place in the list.
    Toggle(u16),
    SelectAll,
    Clear,
    SpaceMenu,
    /// Add to a local Space, by its place in the menu.
    Space(u8),
    Add,
}

/// What the read found: what the editors opened, and each local Space's
/// projects resolved the same way, so choosing a Space or adding to it asks
/// the file system nothing more.
#[derive(Debug, Default)]
struct Found {
    scan: Scan,
    known: HashMap<String, ResolvedProjects>,
}

/// The read itself, off the UI thread.
fn read(spaces: Vec<String>) -> Found {
    let scan = editor_projects::scan(&crate::workspace_threads::project_path_key);
    let known = spaces
        .into_iter()
        .map(|id| {
            let projects = crate::workspace_threads::resolved_projects(&id);
            (id, projects)
        })
        .collect();
    Found { scan, known }
}

#[derive(Debug, Clone, Default)]
pub(super) struct EditorImportUi {
    /// Whether the read is still out.
    loading: bool,
    /// Which read is wanted: one that comes back after another was asked
    /// for, or after the page was left, is dropped.
    generation: u64,
    /// Each folder resolved as the sidebar keeps a project's path.
    scan: Scan,
    /// Each local Space's projects, by its id, resolved the same way.
    known: HashMap<String, ResolvedProjects>,
    filter: Option<usize>,
    /// Ticked folders, by index into `scan.folders`.
    selected: HashSet<usize>,
    /// The local Spaces, by id and name.
    spaces: Vec<(String, String)>,
    space: Option<String>,
    /// The folder each painted row stands for.
    rows: Vec<usize>,
    /// How many were added, and to which Space, once they have been.
    added: Option<(usize, String)>,
}

impl EditorImportUi {
    /// Start a read, aimed at `space` when it is local and the first local
    /// Space otherwise. Its number, and the Spaces it is to resolve.
    fn begin(&mut self, space: Option<String>) -> (u64, Vec<String>) {
        let spaces = crate::workspace_threads::local_spaces();
        let space = space
            .filter(|id| spaces.iter().any(|(local, _)| local == id))
            .or_else(|| spaces.first().map(|(id, _)| id.clone()));
        let ids = spaces.iter().map(|(id, _)| id.clone()).collect();
        *self = Self {
            loading: true,
            generation: self.generation + 1,
            spaces,
            space,
            ..Self::default()
        };
        (self.generation, ids)
    }

    /// What read `generation` found, if it is still the one wanted.
    fn finish(&mut self, generation: u64, found: Found) {
        if !self.loading || generation != self.generation {
            return;
        }
        self.loading = false;
        self.scan = found.scan;
        self.known = found.known;
        self.retarget(self.space.clone());
    }

    /// Let go of what was read, and of a read still out: the page was left.
    pub(super) fn release(&mut self) {
        *self = Self {
            generation: self.generation,
            ..Self::default()
        };
    }

    /// Aim at `space`, ticking what was used lately and it has not got.
    fn retarget(&mut self, space: Option<String>) {
        self.space = space;
        let now = SystemTime::now();
        self.selected = (0..self.scan.folders.len())
            .filter(|&index| {
                self.importable(index)
                    && now
                        .duration_since(self.scan.folders[index].last_used)
                        .map_or(true, |age| age <= editor_projects::RECENT)
            })
            .collect();
    }

    /// Whether a folder is not among the chosen Space's projects yet. One
    /// it has archived is: adding it brings it back.
    fn importable(&self, index: usize) -> bool {
        let path = &self.scan.folders[index].path;
        !self
            .space
            .as_ref()
            .and_then(|space| self.known.get(space))
            .and_then(|projects| projects.get(path))
            .is_some_and(|(_, archived)| !archived)
    }

    fn visible(&self) -> Vec<usize> {
        self.scan
            .folders_in(self.filter)
            .map(|(index, _)| index)
            .collect()
    }

    /// The ticked folders on show: what Add adds. One ticked under another
    /// editor's filter is not, so the count on the button is what happens.
    fn chosen(&self) -> Vec<usize> {
        self.visible()
            .into_iter()
            .filter(|index| self.selected.contains(index))
            .collect()
    }

    fn space_name(&self) -> String {
        self.spaces
            .iter()
            .find(|(id, _)| Some(id) == self.space.as_ref())
            .map(|(_, name)| name.clone())
            .unwrap_or_default()
    }

    /// The editors the review step's heading shows: those with folders to
    /// offer, or every one this machine has when none has.
    pub(super) fn kinds(&self) -> Vec<EditorKind> {
        if self.scan.editors.is_empty() {
            editor_projects::stack_kinds(editor_projects::installed().iter().copied())
        } else {
            editor_projects::stack_kinds(self.scan.editors.iter().map(|editor| editor.kind))
        }
    }
}

/// When a folder was last used, in the Archived page's words.
fn last_used_label(when: SystemTime) -> String {
    let seconds = when
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64);
    format_archived_when(seconds)
}

impl SettingsWindow {
    pub(super) fn paint_editor_import(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let mut bottom = self.paint_import_source_heading(
            layers,
            x,
            y,
            width,
            &crate::i18n::tr("settings-import-editors-title"),
            &crate::i18n::tr("settings-import-editors-subtitle"),
        )?;
        bottom += self.ui_px(28.0);
        if self.ui.editor_import.loading || self.ui.editor_import.scan.folders.is_empty() {
            let notice = if self.ui.editor_import.loading {
                "settings-import-editors-reading"
            } else {
                "settings-import-editors-empty"
            };
            bottom = self.paint_import_notice(
                layers,
                x,
                bottom,
                width,
                &crate::i18n::tr(notice),
                false,
            )?;
            return self.paint_import_footer(
                layers,
                x,
                bottom + self.ui_px(32.0),
                width,
                Some(SettingsAction::ImportBack),
                &[],
            );
        }

        if self.ui.editor_import.scan.editors.len() > 1 {
            bottom = self.paint_editor_filters(layers, x, bottom, width)? + self.ui_px(28.0);
        }

        let visible = self.ui.editor_import.visible();
        let importable = visible
            .iter()
            .filter(|&&index| self.ui.editor_import.importable(index))
            .count();
        let selected = self.ui.editor_import.chosen().len();
        let mut args = FluentArgs::new();
        args.set("selected", selected);
        args.set("total", importable);
        bottom = self.paint_import_copy(
            layers,
            x,
            bottom,
            width,
            &crate::i18n::tr_args("settings-fields-selected", &args),
            palette.secondary_text,
        )?;
        bottom = self.paint_import_actions(
            layers,
            x,
            bottom + self.ui_px(12.0),
            width,
            &[
                ImportButton {
                    label: crate::i18n::tr("common-select-all"),
                    action: SettingsAction::EditorImport(EditorImportAction::SelectAll),
                    enabled: importable > 0,
                    primary: false,
                },
                ImportButton {
                    label: crate::i18n::tr("common-clear"),
                    action: SettingsAction::EditorImport(EditorImportAction::Clear),
                    enabled: selected > 0,
                    primary: false,
                },
            ],
        )?;

        // Rows off the page are only counted: a long-lived editor can list
        // hundreds of folders, and every row is the same height.
        let mut rows = RowCursor::new(bottom + self.ui_px(36.0), self);
        let visual = self.settings_row_visual_height();
        let page_bottom = self.content_bottom();
        self.ui.editor_import.rows.clear();
        for (place, index) in visible.iter().copied().enumerate() {
            self.ui.editor_import.rows.push(index);
            if rows.y + visual > 0.0 && rows.y - visual < page_bottom {
                self.paint_editor_folder_row(layers, x, rows.y, width, place, index, rows.rule())?;
            }
            rows.add(0.0);
        }
        bottom = rows.bottom + self.ui_px(36.0);

        bottom = self.paint_editor_destination(layers, x, bottom, width)?;
        let remote = self.ui.editor_import.scan.remote;
        if remote > 0 {
            let mut args = FluentArgs::new();
            args.set("count", remote);
            bottom = self.paint_import_copy(
                layers,
                x,
                bottom + self.ui_px(20.0),
                width,
                &crate::i18n::tr_args("settings-import-editors-remote", &args),
                palette.muted_text,
            )?;
        }

        let chosen = selected;
        let mut args = FluentArgs::new();
        args.set("count", chosen);
        self.paint_import_footer(
            layers,
            x,
            bottom + self.ui_px(32.0),
            width,
            Some(SettingsAction::ImportBack),
            &[ImportButton {
                label: crate::i18n::tr_args("settings-import-editors-add", &args),
                action: SettingsAction::EditorImport(EditorImportAction::Add),
                enabled: chosen > 0 && self.ui.editor_import.space.is_some(),
                primary: true,
            }],
        )
    }

    /// All, then each editor, with how many folders each has: pills, like
    /// the Keymap page's switches.
    fn paint_editor_filters(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let accent = self.chrome_palette.accent;
        let font = Rc::clone(&self.ui_font);
        let height = self.ui_px(CONTROL_HEIGHT);
        let gap = self.ui_px(12.0);
        let pad = self.ui_px(22.0);
        let scan = &self.ui.editor_import.scan;
        let mut pills = vec![(
            format!(
                "{}  {}",
                crate::i18n::tr("settings-import-editors-all"),
                scan.folders.len()
            ),
            None,
        )];
        for (index, editor) in scan.editors.iter().enumerate() {
            let count = scan.folders_in(Some(index)).count();
            pills.push((format!("{}  {count}", editor.name), Some(index as u8)));
        }
        let filter = self.ui.editor_import.filter;
        let (mut pill_x, mut pill_y) = (x, y);
        for (label, editor) in pills {
            let pill_width = self.measure_text_width(&font, &label) + pad * 2.0;
            if pill_x > x && pill_x + pill_width > x + width {
                pill_x = x;
                pill_y += height + gap;
            }
            let action = SettingsAction::EditorImport(EditorImportAction::Filter(editor));
            let on = filter.map(|index| index as u8) == editor;
            let hovered = self.ui.interaction.hovered == Some(action);
            let (fill, border, text) = if on {
                (accent, accent, palette.on_accent)
            } else if hovered {
                (palette.control_hover_bg, palette.control_border, palette.text)
            } else {
                (palette.control_bg, palette.control_border, palette.text)
            };
            self.draw_rounded_frame(
                layers,
                0,
                pill_x,
                pill_y,
                pill_width,
                height,
                fill,
                border,
                height / 2.0,
            )?;
            let text_y = self.control_text_y(pill_y, height);
            self.draw_text(layers, &font, pill_x + pad, text_y, &label, text, pill_width)?;
            self.ui_context.push(
                rect(pill_x, pill_y, pill_width, height),
                WidgetKind::Button,
                action,
            );
            pill_x += pill_width + gap;
        }
        Ok(pill_y + height)
    }

    /// A folder: its box, its name over its path, and which editor last
    /// opened it and when -- or that the Space has it already.
    #[allow(clippy::too_many_arguments)]
    fn paint_editor_folder_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        place: usize,
        index: usize,
        draw_top_rule: bool,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let body_font = Rc::clone(&self.body_font);
        let editor_import = &self.ui.editor_import;
        let folder = editor_import.scan.folders[index].clone();
        let editor = editor_import
            .scan
            .editors
            .get(folder.editors[0])
            .map(|editor| editor.name.clone())
            .unwrap_or_default();
        let importable = editor_import.importable(index);
        let selected = importable && editor_import.selected.contains(&index);
        let action = SettingsAction::EditorImport(EditorImportAction::Toggle(place as u16));
        let hovered = importable && self.ui.interaction.hovered == Some(action);
        let pressed = importable && self.ui.interaction.pressed == Some(action);

        if draw_top_rule {
            self.paint_separator(layers, x, y - self.ui_px(28.0), width)?;
        }
        let row_rect = rect(
            x - self.ui_px(14.0),
            y - self.ui_px(18.0),
            width + self.ui_px(28.0),
            self.settings_row_visual_height() + self.ui_px(18.0),
        );
        if importable {
            self.ui_context.push(row_rect, WidgetKind::Button, action);
        }
        if hovered || pressed {
            self.draw_rounded_rect(
                layers,
                0,
                row_rect.origin.x,
                row_rect.origin.y,
                row_rect.size.width,
                row_rect.size.height,
                if pressed {
                    palette.control_pressed_bg
                } else {
                    palette.control_hover_bg
                },
                self.ui_px(16.0),
            )?;
        }

        let checkbox = self.ui_px(28.0);
        let checkbox_y = y + self.ui_px(6.0);
        let (fill, border) = if selected {
            (self.chrome_palette.accent, self.chrome_palette.accent)
        } else if !importable {
            (
                palette.control_bg.mul_alpha(0.5),
                palette.control_border.mul_alpha(0.5),
            )
        } else if hovered {
            (palette.control_hover_bg, self.chrome_palette.accent)
        } else {
            (palette.control_bg, palette.control_border)
        };
        self.draw_rounded_frame(
            layers,
            0,
            x,
            checkbox_y,
            checkbox,
            checkbox,
            fill,
            border,
            self.ui_px(8.0),
        )?;
        if selected {
            self.draw_svg_icon(
                layers,
                SvgIcon::Check,
                x + self.ui_px(5.0),
                checkbox_y + self.ui_px(5.0),
                checkbox - self.ui_px(10.0),
                palette.on_accent,
            )?;
        }

        let meta = if importable {
            format!("{editor} · {}", last_used_label(folder.last_used))
        } else {
            crate::i18n::tr("settings-import-editors-in-sidebar")
        };
        let meta_width = self.measure_text_width(&body_font, &meta).min(width * 0.4);
        let meta_x = x + width - meta_width;
        self.draw_text(
            layers,
            &body_font,
            meta_x,
            y,
            &meta,
            palette.muted_text,
            meta_width + 1.0,
        )?;

        let label_x = x + checkbox + self.ui_px(18.0);
        let (name_color, path_color) = if importable {
            (palette.text, palette.secondary_text)
        } else {
            (palette.muted_text, palette.muted_text)
        };
        self.draw_text(
            layers,
            &ui_font,
            label_x,
            y,
            &folder.name,
            name_color,
            (meta_x - label_x - self.ui_px(24.0)).max(0.0),
        )?;
        self.draw_text(
            layers,
            &body_font,
            label_x,
            self.settings_row_description_y(y),
            &crate::ui::home_relative(&folder.path),
            path_color,
            (x + width - label_x).max(0.0),
        )?;
        Ok(())
    }

    /// Which local Space the projects go to, as a row with a menu.
    fn paint_editor_destination(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        self.paint_separator(layers, x, y - self.ui_px(28.0), width)?;
        let (control_x, control_y, control_width) = self.dropdown_control_geometry(x, y, width);
        self.note_dropdown_anchor(
            SettingsDropdown::EditorImportSpace,
            (control_x, control_y, control_width),
        );
        let text_width = (control_x - x - self.ui_px(24.0)).max(width * 0.45);
        let action = SettingsAction::EditorImport(EditorImportAction::SpaceMenu);
        let label = self.ui.editor_import.space_name();
        let pill = self.dropdown_pill_rect(control_x, control_y, control_width, &label);
        let open = self.ui.open_dropdown == Some(SettingsDropdown::EditorImportSpace);
        let hovered = self.ui.interaction.hovered == Some(action);
        let pressed = self.ui.interaction.pressed == Some(action);
        let bg = if pressed || hovered {
            palette.control_hover_bg
        } else {
            palette.control_bg
        };
        let border = if open {
            palette.nav_selected_bg
        } else if hovered || pressed {
            palette.separator
        } else {
            palette.control_border
        };
        self.ui_context.push(pill, WidgetKind::Button, action);
        self.draw_text(
            layers,
            &ui_font,
            x,
            y,
            &crate::i18n::tr("settings-import-editors-add-to"),
            palette.text,
            text_width,
        )?;
        let extra = self.draw_row_description(
            layers,
            x,
            y,
            &crate::i18n::tr("settings-import-editors-add-to-description"),
            text_width,
        )?;
        self.paint_dropdown_pill(layers, pill, &label, bg, border)?;
        Ok(y + self.settings_row_visual_height() + extra)
    }

    pub(super) fn paint_editor_space_menu(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<()> {
        let chosen = self.ui.editor_import.space.clone();
        let options: Vec<_> = self
            .ui
            .editor_import
            .spaces
            .iter()
            .enumerate()
            .map(|(index, (id, name))| {
                (
                    name.clone(),
                    SettingsAction::EditorImport(EditorImportAction::Space(index as u8)),
                    Some(id) == chosen.as_ref(),
                )
            })
            .collect();
        self.paint_dropdown_menu(layers, x, y, width, &options)
    }

    pub(super) fn paint_editor_result(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        top: f32,
        width: f32,
    ) -> anyhow::Result<f32> {
        let (count, space) = self.ui.editor_import.added.clone().unwrap_or_default();
        let mut args = FluentArgs::new();
        args.set("count", count);
        args.set("space", space);
        let message = crate::i18n::tr_args("settings-import-editors-done", &args);
        let bottom = self.paint_import_result(layers, x, top + self.ui_px(64.0), width, &message)?;
        self.paint_import_footer(
            layers,
            x,
            bottom + self.ui_px(40.0),
            width,
            Some(SettingsAction::ImportStartOver),
            &[],
        )
    }

    /// Editors' marks on overlapping tiles of side `tile`, from `x`.
    pub(super) fn paint_editor_stack(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        kinds: &[EditorKind],
        x: f32,
        y: f32,
        tile: f32,
        ground: LinearRgba,
    ) -> anyhow::Result<()> {
        let ctx = crate::ui::draw::DrawContext::new(
            self.render_state.as_ref().unwrap(),
            self.dimensions,
            &self.metrics,
        );
        let dark = self.chrome_palette.is_dark();
        editor_projects::draw_stack(&ctx, layers, kinds, x, y, tile, ground, dark)
    }

    /// Read what the editors opened on a thread of its own, aimed at `space`,
    /// and show it when it comes back.
    pub(super) fn start_editor_import(&mut self, space: Option<String>) {
        let (generation, spaces) = self.ui.editor_import.begin(space);
        let instance_id = self.instance_id;
        promise::spawn::spawn(async move {
            let found = promise::spawn::spawn_into_new_thread(move || {
                Ok::<_, anyhow::Error>(read(spaces))
            })
            .await
            .unwrap_or_else(|err| {
                log::warn!("reading the folders code editors opened failed: {err:#}");
                Found::default()
            });
            promise::spawn::spawn_into_main_thread(async move {
                let Some(settings) = settings_window_for_instance(instance_id) else {
                    return;
                };
                let mut settings = settings.borrow_mut();
                settings.ui.editor_import.finish(generation, found);
                if let Some(window) = &settings.window {
                    window.invalidate();
                }
            })
            .detach();
        })
        .detach();
    }

    /// A source's mark, lit like the editors' tiles where it is an app icon.
    pub(super) fn paint_brand_tile(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        icon: BrandIcon,
        x: f32,
        y: f32,
        size: f32,
    ) -> anyhow::Result<()> {
        let ctx = crate::ui::draw::DrawContext::new(
            self.render_state.as_ref().unwrap(),
            self.dimensions,
            &self.metrics,
        );
        let dark = self.chrome_palette.is_dark();
        crate::ui::tile::draw_brand_tile(
            &ctx,
            layers,
            icon,
            x,
            y,
            size,
            dark,
            &crate::settings_window::ROW_TILE,
        )
    }

    pub(super) fn perform_editor_import_action(&mut self, action: EditorImportAction) {
        match action {
            EditorImportAction::Filter(editor) => {
                let editors = self.ui.editor_import.scan.editors.len();
                self.ui.editor_import.filter =
                    editor.map(usize::from).filter(|&index| index < editors);
            }
            EditorImportAction::Toggle(place) => {
                let editor_import = &mut self.ui.editor_import;
                let Some(&index) = editor_import.rows.get(place as usize) else {
                    return;
                };
                if !editor_import.importable(index) {
                    return;
                }
                if !editor_import.selected.remove(&index) {
                    editor_import.selected.insert(index);
                }
            }
            EditorImportAction::SelectAll => {
                let editor_import = &mut self.ui.editor_import;
                for index in editor_import.visible() {
                    if editor_import.importable(index) {
                        editor_import.selected.insert(index);
                    }
                }
            }
            EditorImportAction::Clear => {
                let editor_import = &mut self.ui.editor_import;
                for index in editor_import.visible() {
                    editor_import.selected.remove(&index);
                }
            }
            EditorImportAction::SpaceMenu => {
                self.ui.open_dropdown =
                    if self.ui.open_dropdown == Some(SettingsDropdown::EditorImportSpace) {
                        None
                    } else {
                        Some(SettingsDropdown::EditorImportSpace)
                    };
            }
            EditorImportAction::Space(place) => {
                self.ui.open_dropdown = None;
                let Some((id, _)) = self.ui.editor_import.spaces.get(place as usize).cloned()
                else {
                    return;
                };
                self.ui.editor_import.retarget(Some(id));
            }
            EditorImportAction::Add => {
                let editor_import = &self.ui.editor_import;
                let Some(space) = editor_import.space.clone() else {
                    return;
                };
                let paths: Vec<PathBuf> = editor_import
                    .chosen()
                    .into_iter()
                    .map(|index| editor_import.scan.folders[index].path.clone())
                    .collect();
                let none = ResolvedProjects::default();
                let known = editor_import.known.get(&space).unwrap_or(&none);
                let added = crate::workspace_threads::add_projects(&space, &paths, known);
                let name = editor_import.space_name();
                self.ui.editor_import.added = Some((added, name));
                self.ui.import_step = ImportStep::Result;
                self.ui.content_scroll.reset();
                if let Some(front_end) = crate::frontend::try_front_end() {
                    front_end.invalidate_all_windows();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(path: &str) -> Found {
        Found {
            scan: Scan {
                editors: Vec::new(),
                folders: vec![editor_projects::Folder {
                    path: PathBuf::from(path),
                    name: "app".to_string(),
                    editors: vec![0],
                    last_used: SystemTime::now(),
                }],
                remote: 0,
            },
            known: HashMap::new(),
        }
    }

    fn reading(generation: u64) -> EditorImportUi {
        EditorImportUi {
            loading: true,
            generation,
            space: Some("space-a".to_string()),
            ..EditorImportUi::default()
        }
    }

    #[test]
    fn a_read_that_is_no_longer_wanted_is_dropped() {
        let mut ui = reading(2);
        ui.finish(1, found("/code/old"));
        assert!(ui.loading && ui.scan.folders.is_empty());

        ui.release();
        ui.finish(2, found("/code/app"));
        assert!(!ui.loading && ui.scan.folders.is_empty(), "the page was left");

        let mut ui = reading(3);
        ui.finish(3, found("/code/app"));
        assert!(!ui.loading);
        // Used just now and not in the Space: ticked.
        assert!(ui.selected.contains(&0));
    }

    #[test]
    fn a_folder_the_space_has_archived_can_be_added_again() {
        let mut ui = reading(1);
        let mut read = found("/code/app");
        let project = |archived| {
            HashMap::from([(
                PathBuf::from("/code/app"),
                ("project-a".to_string(), archived),
            )])
        };
        read.known.insert("space-a".to_string(), project(true));
        ui.finish(1, read);
        assert!(ui.importable(0));
        ui.known.insert("space-a".to_string(), project(false));
        assert!(!ui.importable(0));
    }
}
