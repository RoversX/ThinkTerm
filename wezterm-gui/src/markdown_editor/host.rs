use super::{
    build_visual_document, default_document_session, EditorViewState, MarkdownDocumentSession,
    MarkdownProjection, ProjectedObject, SourcePosition, VisualDocument,
};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub(crate) struct NoteRunLayout {
    pub source: Range<usize>,
    pub x: f32,
    pub width: f32,
    pub hit_x: f32,
    pub hit_width: f32,
    /// Visual x offsets paired with the source byte position reached at that
    /// boundary. Contains both the leading and trailing edge.
    pub boundaries: Vec<(f32, usize)>,
    pub atomic: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct NoteLineLayout {
    pub source: Range<usize>,
    pub y: f32,
    pub height: f32,
    pub runs: Vec<NoteRunLayout>,
}

#[derive(Debug, Clone)]
pub(crate) struct NoteCodeBlockLayout {
    pub source_start: usize,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub max_horizontal_scroll: f32,
}

pub(crate) struct NoteHostState {
    pub session: Option<Arc<Mutex<MarkdownDocumentSession>>>,
    pub view: EditorViewState,
    pub projection: MarkdownProjection,
    pub visual: VisualDocument,
    wrapped_visual: Arc<VisualDocument>,
    pub projection_revision: Option<u64>,
    pub visual_key: Option<(u64, super::EditorMode, usize)>,
    wrapped_key: Option<(u64, super::EditorMode, usize, usize)>,
    pub line_layouts: Vec<NoteLineLayout>,
    pub viewport_height: f32,
    pub content_height: f32,
    pub load_error: Option<String>,
    pub save_generation: u64,
    pub reveal_caret: bool,
    pub drag_selection_active: bool,
    pub drag_autoscroll_scheduled: bool,
    pub collapsed_code_blocks: HashSet<usize>,
    pub code_horizontal_offsets: HashMap<usize, f32>,
    pub code_block_layouts: Vec<NoteCodeBlockLayout>,
}

impl Default for NoteHostState {
    fn default() -> Self {
        Self {
            session: None,
            view: EditorViewState::default(),
            projection: MarkdownProjection::default(),
            visual: VisualDocument::default(),
            wrapped_visual: Arc::new(VisualDocument::default()),
            projection_revision: None,
            visual_key: None,
            wrapped_key: None,
            line_layouts: vec![],
            viewport_height: 0.0,
            content_height: 0.0,
            load_error: None,
            save_generation: 0,
            reveal_caret: true,
            drag_selection_active: false,
            drag_autoscroll_scheduled: false,
            collapsed_code_blocks: HashSet::new(),
            code_horizontal_offsets: HashMap::new(),
            code_block_layouts: vec![],
        }
    }
}

impl NoteHostState {
    pub(crate) fn ensure_loaded(&mut self) -> bool {
        if self.session.is_some() {
            return true;
        }
        match default_document_session() {
            Ok(session) => {
                self.view.selection = super::SourceSelection::caret(0);
                self.session = Some(session);
                self.load_error = None;
                self.refresh_projection();
                true
            }
            Err(err) => {
                self.load_error = Some(format!("{err:#}"));
                false
            }
        }
    }

    pub(crate) fn refresh_projection(&mut self) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let session = session.lock();
        let revision = session.revision();
        let source = session.source();
        if self.projection_revision != Some(revision) {
            self.projection = MarkdownProjection::parse(source);
            let code_starts = self
                .projection
                .objects
                .iter()
                .filter_map(|object| match object {
                    ProjectedObject::CodeBlock(code) => Some(code.source.start),
                    _ => None,
                })
                .collect::<HashSet<_>>();
            self.collapsed_code_blocks
                .retain(|source_start| code_starts.contains(source_start));
            self.code_horizontal_offsets
                .retain(|source_start, _| code_starts.contains(source_start));
            self.projection_revision = Some(revision);
            self.visual_key = None;
        }
        let active_start = self
            .projection
            .active_syntax(self.view.selection.focus.byte)
            .map(|node| node.source.start)
            .unwrap_or(usize::MAX);
        let key = (revision, self.view.mode, active_start);
        if self.visual_key != Some(key) {
            self.visual = build_visual_document(
                source,
                &self.projection,
                self.view.mode,
                self.view.selection.focus.byte,
            );
            self.visual_key = Some(key);
            self.wrapped_key = None;
        }
    }

    pub(crate) fn cached_wrapped_visual(&self, wrap_key: usize) -> Option<Arc<VisualDocument>> {
        let Some((revision, mode, active_start)) = self.visual_key else {
            return None;
        };
        (self.wrapped_key == Some((revision, mode, active_start, wrap_key)))
            .then(|| Arc::clone(&self.wrapped_visual))
    }

    pub(crate) fn cache_wrapped_visual(
        &mut self,
        wrap_key: usize,
        visual: VisualDocument,
    ) -> Arc<VisualDocument> {
        self.wrapped_visual = Arc::new(visual);
        if let Some((revision, mode, active_start)) = self.visual_key {
            self.wrapped_key = Some((revision, mode, active_start, wrap_key));
        }
        Arc::clone(&self.wrapped_visual)
    }

    pub(crate) fn source_position_for_point(&self, x: f32, y: f32) -> SourcePosition {
        let Some(line) = self
            .line_layouts
            .iter()
            .min_by(|a, b| distance_to_line(a, y).total_cmp(&distance_to_line(b, y)))
        else {
            return SourcePosition::new(0);
        };
        if line.runs.is_empty() {
            return SourcePosition::new(line.source.start);
        }
        let run = line
            .runs
            .iter()
            .min_by(|a, b| distance_to_run(a, x).total_cmp(&distance_to_run(b, x)))
            .expect("non-empty runs");
        if run.atomic {
            let midpoint = run.x + run.width / 2.0;
            return SourcePosition {
                byte: if x < midpoint {
                    run.source.start
                } else {
                    run.source.end
                },
                affinity: if x < midpoint {
                    super::Affinity::Before
                } else {
                    super::Affinity::After
                },
            };
        }
        run.boundaries
            .iter()
            .min_by(|(ax, _), (bx, _)| (x - *ax).abs().total_cmp(&(x - *bx).abs()))
            .map(|(_, byte)| SourcePosition::new(*byte))
            .unwrap_or_else(|| SourcePosition::new(run.source.start))
    }

    /// Move between painted visual rows (including soft wraps) while retaining
    /// the preferred horizontal pixel. The controller falls back to physical
    /// source-line navigation when the target row is outside the current
    /// viewport and therefore intentionally has no shaped layout.
    pub(crate) fn visual_vertical_target(&self, delta: isize) -> Option<(SourcePosition, usize)> {
        let caret = self.view.selection.focus.byte;
        let current = self.line_layouts.iter().position(|line| {
            line.runs
                .iter()
                .any(|run| run.source.start <= caret && caret <= run.source.end)
                || (line.runs.is_empty() && line.source.start == caret)
        })?;
        let target = if delta.is_negative() {
            current.checked_sub(delta.unsigned_abs())?
        } else {
            current.checked_add(delta as usize)?
        };
        let target_line = self.line_layouts.get(target)?;
        let current_line = &self.line_layouts[current];
        let current_x = current_line
            .runs
            .iter()
            .find(|run| run.source.start <= caret && caret <= run.source.end)
            .and_then(|run| {
                run.boundaries
                    .iter()
                    .min_by_key(|(_, source)| source.abs_diff(caret))
                    .map(|(x, _)| *x)
            })
            .or_else(|| current_line.runs.first().map(|run| run.x))
            .unwrap_or(0.0);
        let preferred = self
            .view
            .preferred_column
            .unwrap_or(current_x.max(0.0) as usize);
        let position = self
            .source_position_for_point(preferred as f32, target_line.y + target_line.height / 2.0);
        Some((position, preferred))
    }

    pub(crate) fn atomic_source_for_point(&self, x: f32, y: f32) -> Option<Range<usize>> {
        self.line_layouts
            .iter()
            .find(|line| y >= line.y && y <= line.y + line.height)?
            .runs
            .iter()
            .find(|run| run.atomic && x >= run.x && x <= run.x + run.width)
            .map(|run| run.source.clone())
    }

    pub(crate) fn table_cell_target(&self, delta: isize) -> Option<usize> {
        let caret = self.view.selection.focus.byte;
        let cells: Vec<_> = self
            .projection
            .objects
            .iter()
            .filter_map(|object| match object {
                ProjectedObject::Table(table) => Some(table),
                _ => None,
            })
            .flat_map(|table| table.rows.iter().flat_map(|row| row.iter()))
            .collect();
        let current = cells
            .iter()
            .position(|cell| cell.source.start <= caret && caret <= cell.source.end)?;
        let target = if delta.is_negative() {
            current.checked_sub(delta.unsigned_abs())?
        } else {
            current.checked_add(delta as usize)?
        };
        cells.get(target).map(|cell| cell.source.start)
    }

    pub(crate) fn clamp_scroll(&mut self) {
        let max = (self.content_height - self.viewport_height).max(0.0);
        self.view.scroll_offset = self.view.scroll_offset.clamp(0.0, max);
    }

    pub(crate) fn scroll_by(&mut self, delta: f32) -> bool {
        let old = self.view.scroll_offset;
        self.view.scroll_offset += delta;
        self.clamp_scroll();
        (old - self.view.scroll_offset).abs() > f32::EPSILON
    }

    pub(crate) fn toggle_code_block(&mut self, source_start: usize) -> bool {
        if !self.collapsed_code_blocks.remove(&source_start) {
            self.collapsed_code_blocks.insert(source_start);
            true
        } else {
            false
        }
    }

    pub(crate) fn scroll_code_block_at(&mut self, x: f32, y: f32, delta: f32) -> bool {
        let Some(layout) = self.code_block_layouts.iter().find(|layout| {
            x >= layout.x
                && x <= layout.x + layout.width
                && y >= layout.y
                && y <= layout.y + layout.height
                && layout.max_horizontal_scroll > 0.0
        }) else {
            return false;
        };
        let offset = self
            .code_horizontal_offsets
            .entry(layout.source_start)
            .or_default();
        let old = *offset;
        *offset = (*offset + delta).clamp(0.0, layout.max_horizontal_scroll);
        (old - *offset).abs() > f32::EPSILON
    }
}

fn distance_to_line(line: &NoteLineLayout, y: f32) -> f32 {
    if y < line.y {
        line.y - y
    } else if y > line.y + line.height {
        y - (line.y + line.height)
    } else {
        0.0
    }
}

fn distance_to_run(run: &NoteRunLayout, x: f32) -> f32 {
    if x < run.hit_x {
        run.hit_x - x
    } else if x > run.hit_x + run.hit_width {
        x - (run.hit_x + run.hit_width)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(source: Range<usize>, glyph_x: f32, cell_x: f32) -> NoteRunLayout {
        NoteRunLayout {
            boundaries: vec![(glyph_x, source.start), (glyph_x + 10.0, source.end)],
            source,
            x: glyph_x,
            width: 10.0,
            hit_x: cell_x,
            hit_width: 100.0,
            atomic: false,
        }
    }

    #[test]
    fn table_cell_whitespace_hits_its_own_source_range() {
        let mut host = NoteHostState::default();
        host.line_layouts.push(NoteLineLayout {
            source: 0..20,
            y: 10.0,
            height: 30.0,
            runs: vec![run(1..2, 10.0, 0.0), run(11..12, 110.0, 100.0)],
        });

        assert_eq!(host.source_position_for_point(90.0, 20.0).byte, 2);
        assert_eq!(host.source_position_for_point(110.0, 20.0).byte, 11);
        assert_eq!(host.source_position_for_point(190.0, 20.0).byte, 12);
    }

    #[test]
    fn point_selection_chooses_the_nearest_visual_row() {
        let mut host = NoteHostState::default();
        host.line_layouts = vec![
            NoteLineLayout {
                source: 0..1,
                y: 10.0,
                height: 20.0,
                runs: vec![run(0..1, 0.0, 0.0)],
            },
            NoteLineLayout {
                source: 2..3,
                y: 40.0,
                height: 20.0,
                runs: vec![run(2..3, 0.0, 0.0)],
            },
        ];

        assert_eq!(host.source_position_for_point(0.0, 5.0).byte, 0);
        assert_eq!(host.source_position_for_point(0.0, 65.0).byte, 2);
    }

    #[test]
    fn code_block_view_state_is_per_block_and_bounded() {
        let mut host = NoteHostState::default();
        assert!(host.toggle_code_block(10));
        assert!(host.collapsed_code_blocks.contains(&10));
        assert!(!host.toggle_code_block(10));
        assert!(!host.collapsed_code_blocks.contains(&10));

        host.code_block_layouts.push(NoteCodeBlockLayout {
            source_start: 20,
            x: 100.0,
            y: 50.0,
            width: 300.0,
            height: 120.0,
            max_horizontal_scroll: 80.0,
        });
        assert!(!host.scroll_code_block_at(20.0, 60.0, 30.0));
        assert!(host.scroll_code_block_at(200.0, 80.0, 30.0));
        assert_eq!(host.code_horizontal_offsets.get(&20), Some(&30.0));
        assert!(host.scroll_code_block_at(200.0, 80.0, 100.0));
        assert_eq!(host.code_horizontal_offsets.get(&20), Some(&80.0));
        assert!(!host.scroll_code_block_at(200.0, 80.0, 100.0));
    }
}
