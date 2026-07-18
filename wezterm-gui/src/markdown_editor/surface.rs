use super::projection::ProjectedText;
use super::{BlockKind, EditorMode, InlineStyle, MarkdownProjection, ProjectedObject};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;
#[cfg(test)]
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VisualLineKind {
    Text,
    Rule,
    Image,
    TableHeader,
    TableRow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VisualRun {
    pub source: Range<usize>,
    pub text: String,
    pub style: InlineStyle,
    pub atomic: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VisualLine {
    pub source: Range<usize>,
    pub block: BlockKind,
    pub kind: VisualLineKind,
    pub runs: Vec<VisualRun>,
}

impl VisualLine {
    #[cfg(test)]
    pub(crate) fn text(&self) -> String {
        self.runs.iter().map(|run| run.text.as_str()).collect()
    }
}

/// Soft-wrap a projected document without changing the underlying source.
/// Hosts choose `max_columns` from their current viewport; every resulting run
/// retains byte ranges into the original Markdown for hit testing and edits.
#[cfg(test)]
pub(crate) fn wrap_visual_document(
    document: &VisualDocument,
    max_columns: usize,
) -> VisualDocument {
    let max_columns = max_columns.max(1);
    let mut lines = Vec::with_capacity(document.lines.len());
    for line in &document.lines {
        if line.runs.is_empty()
            || matches!(line.block, BlockKind::CodeBlock | BlockKind::Table)
            || matches!(line.kind, VisualLineKind::Rule | VisualLineKind::Image)
        {
            lines.push(line.clone());
            continue;
        }

        let mut current = empty_visual_line(line, line.source.start);
        let mut columns = 0usize;
        for run in &line.runs {
            let source_matches_text =
                !run.atomic && run.source.end.saturating_sub(run.source.start) == run.text.len();
            if !source_matches_text {
                let width = UnicodeWidthStr::width(run.text.as_str()).max(1);
                if columns > 0 && columns.saturating_add(width) > max_columns {
                    lines.push(current);
                    current = empty_visual_line(line, run.source.start);
                    columns = 0;
                }
                append_run(&mut current, run.clone());
                columns = columns.saturating_add(width);
                continue;
            }

            for (relative, grapheme) in run.text.grapheme_indices(true) {
                let width = UnicodeWidthStr::width(grapheme).max(1);
                if columns > 0 && columns.saturating_add(width) > max_columns {
                    lines.push(current);
                    current = empty_visual_line(line, run.source.start + relative);
                    columns = 0;
                }
                append_run(
                    &mut current,
                    VisualRun {
                        source: run.source.start + relative
                            ..run.source.start + relative + grapheme.len(),
                        text: grapheme.to_string(),
                        style: run.style,
                        atomic: false,
                    },
                );
                columns = columns.saturating_add(width);
            }
        }
        current.source.end = current.source.end.max(line.source.end);
        lines.push(current);
    }
    VisualDocument { lines }
}

/// Soft-wrap using the actual rendered width supplied by the host. Keeping
/// measurement outside this module lets each host use its own font stack while
/// every resulting run retains exact Markdown source byte ranges.
pub(crate) fn wrap_visual_document_by_width<E, F>(
    document: &VisualDocument,
    max_width: f32,
    mut measure: F,
) -> Result<VisualDocument, E>
where
    F: FnMut(BlockKind, &VisualRun) -> Result<f32, E>,
{
    let max_width = max_width.max(1.0);
    let mut lines = Vec::with_capacity(document.lines.len());
    for line in &document.lines {
        if line.runs.is_empty()
            || matches!(line.block, BlockKind::CodeBlock | BlockKind::Table)
            || matches!(line.kind, VisualLineKind::Rule | VisualLineKind::Image)
        {
            lines.push(line.clone());
            continue;
        }

        let mut current = empty_visual_line(line, line.source.start);
        let mut width = 0.0f32;
        for run in &line.runs {
            let source_matches_text =
                !run.atomic && run.source.end.saturating_sub(run.source.start) == run.text.len();
            let pieces = if source_matches_text {
                run.text
                    .split_word_bound_indices()
                    .map(|(relative, text)| VisualRun {
                        source: run.source.start + relative
                            ..run.source.start + relative + text.len(),
                        text: text.to_string(),
                        style: run.style,
                        atomic: false,
                    })
                    .collect::<Vec<_>>()
            } else {
                vec![run.clone()]
            };

            for piece in pieces {
                let piece_width = measure(line.block, &piece)?.max(0.0);
                let whitespace = piece.text.chars().all(char::is_whitespace);
                if !piece.atomic && !whitespace && piece_width > max_width {
                    for (relative, grapheme) in piece.text.grapheme_indices(true) {
                        let grapheme_run = VisualRun {
                            source: piece.source.start + relative
                                ..piece.source.start + relative + grapheme.len(),
                            text: grapheme.to_string(),
                            style: piece.style,
                            atomic: false,
                        };
                        let grapheme_width = measure(line.block, &grapheme_run)?.max(0.0);
                        if visual_line_has_non_whitespace(&current)
                            && width + grapheme_width > max_width
                        {
                            lines.push(current);
                            current = empty_visual_line(line, grapheme_run.source.start);
                            width = 0.0;
                        }
                        append_run(&mut current, grapheme_run);
                        width += grapheme_width;
                    }
                    continue;
                }

                if !whitespace
                    && visual_line_has_non_whitespace(&current)
                    && width + piece_width > max_width
                {
                    lines.push(current);
                    current = empty_visual_line(line, piece.source.start);
                    width = 0.0;
                }
                append_run(&mut current, piece);
                width += piece_width;
            }
        }
        current.source.end = current.source.end.max(line.source.end);
        lines.push(current);
    }
    Ok(VisualDocument { lines })
}

fn visual_line_has_non_whitespace(line: &VisualLine) -> bool {
    line.runs
        .iter()
        .any(|run| run.text.chars().any(|ch| !ch.is_whitespace()))
}

/// Fit measured table columns into the viewport. Wide tables shrink columns
/// toward a shared floor; very narrow hosts fall back to equal-width cells so
/// the grid remains bounded and individual cell painters can clip their text.
pub(crate) fn fit_table_columns(
    desired: &[f32],
    available_width: f32,
    minimum_width: f32,
) -> Vec<f32> {
    if desired.is_empty() {
        return vec![];
    }
    let available_width = available_width.max(1.0);
    let equal_width = available_width / desired.len() as f32;
    let floor = minimum_width.max(1.0).min(equal_width);
    let mut widths = desired
        .iter()
        .map(|width| width.max(floor))
        .collect::<Vec<_>>();
    let total = widths.iter().sum::<f32>();
    if total < available_width {
        let extra = (available_width - total) / widths.len() as f32;
        for width in &mut widths {
            *width += extra;
        }
    } else if total > available_width {
        let flexible = widths.iter().map(|width| width - floor).sum::<f32>();
        if flexible <= f32::EPSILON {
            widths.fill(equal_width);
        } else {
            let overflow = total - available_width;
            for width in &mut widths {
                let share = (*width - floor) / flexible;
                *width = (*width - overflow * share).max(floor);
            }
        }
    }
    let correction = available_width - widths.iter().sum::<f32>();
    if let Some(last) = widths.last_mut() {
        *last = (*last + correction).max(1.0);
    }
    widths
}

fn empty_visual_line(template: &VisualLine, source_start: usize) -> VisualLine {
    VisualLine {
        source: source_start..source_start,
        block: template.block,
        kind: template.kind,
        runs: vec![],
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct VisualDocument {
    pub lines: Vec<VisualLine>,
}

pub(crate) fn build_visual_document(
    source: &str,
    projection: &MarkdownProjection,
    mode: EditorMode,
    caret: usize,
) -> VisualDocument {
    if mode == EditorMode::Source {
        return source_document(source);
    }

    let visible = projection.visible_source_ranges(source, caret, false);
    let mut objects: Vec<&ProjectedObject> = projection.objects.iter().collect();
    objects.sort_by_key(|object| {
        let range = object.source();
        (range.start, range.end)
    });
    let mut object_index = 0usize;
    let mut lines = vec![];
    let mut line = VisualLine {
        source: 0..0,
        block: BlockKind::Paragraph,
        kind: VisualLineKind::Text,
        runs: vec![],
    };
    let mut idx = 0usize;

    while idx < source.len() {
        while object_index < objects.len() && objects[object_index].source().end <= idx {
            object_index += 1;
        }
        if let Some(object) = objects.get(object_index) {
            let range = object.source();
            if range.start == idx && range.end > range.start {
                match object {
                    ProjectedObject::TaskCheckbox {
                        source: object_source,
                        checked,
                    } => {
                        append_run(
                            &mut line,
                            VisualRun {
                                source: object_source.clone(),
                                text: if *checked { "☑ " } else { "☐ " }.to_string(),
                                style: InlineStyle::default(),
                                atomic: true,
                            },
                        );
                    }
                    ProjectedObject::ListMarker {
                        source: object_source,
                        text,
                    } => {
                        append_run(
                            &mut line,
                            VisualRun {
                                source: object_source.clone(),
                                text: text.clone(),
                                style: InlineStyle::default(),
                                atomic: true,
                            },
                        );
                    }
                    ProjectedObject::Rule {
                        source: object_source,
                    } => {
                        flush_line(&mut lines, &mut line, idx);
                        lines.push(VisualLine {
                            source: object_source.clone(),
                            block: BlockKind::Other,
                            kind: VisualLineKind::Rule,
                            runs: vec![],
                        });
                    }
                    ProjectedObject::Image {
                        source: object_source,
                        alt,
                        target,
                        ..
                    } => {
                        flush_line(&mut lines, &mut line, idx);
                        let label = if alt.is_empty() { target } else { alt };
                        lines.push(VisualLine {
                            source: object_source.clone(),
                            block: BlockKind::Paragraph,
                            kind: VisualLineKind::Image,
                            runs: vec![VisualRun {
                                source: object_source.clone(),
                                text: format!("▧ {label}"),
                                style: InlineStyle::default(),
                                atomic: true,
                            }],
                        });
                    }
                    ProjectedObject::CodeBlock(code) => {
                        flush_line_if_content(&mut lines, &mut line, idx);
                        let mut content_start = code.content.start;
                        let chunks = if code.text.is_empty() {
                            vec![""]
                        } else {
                            code.text.split_inclusive('\n').collect::<Vec<_>>()
                        };
                        for chunk in chunks {
                            let text = chunk.strip_suffix('\n').unwrap_or(chunk);
                            let text_end = content_start + text.len();
                            lines.push(VisualLine {
                                source: content_start..text_end,
                                block: BlockKind::CodeBlock,
                                kind: VisualLineKind::Text,
                                runs: if text.is_empty() {
                                    vec![]
                                } else {
                                    vec![VisualRun {
                                        source: content_start..text_end,
                                        text: text.to_string(),
                                        style: InlineStyle {
                                            code: true,
                                            ..InlineStyle::default()
                                        },
                                        atomic: false,
                                    }]
                                },
                            });
                            content_start += chunk.len();
                        }
                    }
                    ProjectedObject::Table(table) => {
                        flush_line(&mut lines, &mut line, idx);
                        for (row_index, row) in table.rows.iter().enumerate() {
                            let mut runs = vec![];
                            for cell in row {
                                runs.push(VisualRun {
                                    source: cell.source.clone(),
                                    text: cell.text.clone(),
                                    style: InlineStyle {
                                        strong: row_index == 0,
                                        ..InlineStyle::default()
                                    },
                                    atomic: false,
                                });
                            }
                            lines.push(VisualLine {
                                source: row
                                    .first()
                                    .map(|cell| cell.source.start)
                                    .unwrap_or(table.source.start)
                                    ..row
                                        .last()
                                        .map(|cell| cell.source.end)
                                        .unwrap_or(table.source.end),
                                block: BlockKind::Table,
                                kind: if row_index == 0 {
                                    VisualLineKind::TableHeader
                                } else {
                                    VisualLineKind::TableRow
                                },
                                runs,
                            });
                        }
                    }
                }
                idx = range.end.min(source.len());
                if matches!(object, ProjectedObject::CodeBlock(_))
                    && source[idx..].starts_with('\n')
                {
                    idx += 1;
                }
                object_index += 1;
                continue;
            }
        }

        let ch = source[idx..].chars().next().expect("valid source char");
        let next = idx + ch.len_utf8();
        if ch == '\n' {
            flush_line(&mut lines, &mut line, next);
            idx = next;
            continue;
        }
        if range_contains(&visible, idx) {
            let projected = projected_text_at(projection, idx);
            let style = projected.map(|span| span.style).unwrap_or_default();
            line.block = projected.map(|span| span.block).unwrap_or(line.block);
            append_run(
                &mut line,
                VisualRun {
                    source: idx..next,
                    text: ch.to_string(),
                    style,
                    atomic: false,
                },
            );
        }
        idx = next;
    }
    flush_line(&mut lines, &mut line, source.len());
    if lines.is_empty() {
        lines.push(VisualLine {
            source: 0..0,
            block: BlockKind::Paragraph,
            kind: VisualLineKind::Text,
            runs: vec![],
        });
    }
    VisualDocument { lines }
}

fn projected_text_at(projection: &MarkdownProjection, byte: usize) -> Option<&ProjectedText> {
    let index = projection
        .text
        .partition_point(|span| span.source.end <= byte);
    projection
        .text
        .get(index)
        .filter(|span| span.source.start <= byte && byte < span.source.end)
}

fn source_document(source: &str) -> VisualDocument {
    let mut lines = vec![];
    let mut start = 0usize;
    for chunk in source.split_inclusive('\n') {
        let text = chunk.strip_suffix('\n').unwrap_or(chunk);
        let end = start + text.len();
        lines.push(VisualLine {
            source: start..end,
            block: BlockKind::Paragraph,
            kind: VisualLineKind::Text,
            runs: if text.is_empty() {
                vec![]
            } else {
                vec![VisualRun {
                    source: start..end,
                    text: text.to_string(),
                    style: InlineStyle::default(),
                    atomic: false,
                }]
            },
        });
        start += chunk.len();
    }
    if source.is_empty() || source.ends_with('\n') {
        lines.push(VisualLine {
            source: source.len()..source.len(),
            block: BlockKind::Paragraph,
            kind: VisualLineKind::Text,
            runs: vec![],
        });
    }
    VisualDocument { lines }
}

fn append_run(line: &mut VisualLine, run: VisualRun) {
    if let Some(last) = line.runs.last_mut() {
        if !last.atomic
            && !run.atomic
            && last.style == run.style
            && last.source.end == run.source.start
        {
            last.source.end = run.source.end;
            last.text.push_str(&run.text);
            line.source.end = line.source.end.max(run.source.end);
            return;
        }
    }
    if line.runs.is_empty() {
        line.source.start = run.source.start;
    }
    line.source.end = line.source.end.max(run.source.end);
    line.runs.push(run);
}

fn flush_line(lines: &mut Vec<VisualLine>, line: &mut VisualLine, source_end: usize) {
    if line.runs.is_empty() && line.source.start == line.source.end {
        line.source = source_end..source_end;
    } else {
        line.source.end = line.source.end.max(source_end);
    }
    lines.push(line.clone());
    *line = VisualLine {
        source: source_end..source_end,
        block: BlockKind::Paragraph,
        kind: VisualLineKind::Text,
        runs: vec![],
    };
}

fn flush_line_if_content(lines: &mut Vec<VisualLine>, line: &mut VisualLine, source_end: usize) {
    if line.runs.is_empty() && line.source.start == line.source.end {
        line.source = source_end..source_end;
    } else {
        flush_line(lines, line, source_end);
    }
}

fn range_contains(ranges: &[Range<usize>], byte: usize) -> bool {
    ranges
        .binary_search_by(|range| {
            if byte < range.start {
                std::cmp::Ordering::Greater
            } else if byte >= range.end {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_mode_is_lossless_by_lines() {
        let source = "# 你\n\n**bold**\n";
        let doc = build_visual_document(
            source,
            &MarkdownProjection::parse(source),
            EditorMode::Source,
            0,
        );
        assert_eq!(doc.lines[0].text(), "# 你");
        assert_eq!(doc.lines[1].text(), "");
        assert_eq!(doc.lines[2].text(), "**bold**");
    }

    #[test]
    fn live_preview_hides_inactive_emphasis_markers() {
        let source = "**bold** plain";
        let projection = MarkdownProjection::parse(source);
        let doc = build_visual_document(source, &projection, EditorMode::LivePreview, source.len());
        assert_eq!(doc.lines[0].text(), "bold plain");
        let active = build_visual_document(source, &projection, EditorMode::LivePreview, 3);
        assert_eq!(active.lines[0].text(), "**bold** plain");
    }

    #[test]
    fn live_preview_projects_table_and_task_object() {
        let source = "- [x] done\n\n| A | B |\n| - | - |\n| 1 | 2 |\n";
        let projection = MarkdownProjection::parse(source);
        let doc = build_visual_document(source, &projection, EditorMode::LivePreview, source.len());
        assert!(doc.lines.iter().any(|line| line.text().contains("☑")));
        assert!(doc
            .lines
            .iter()
            .any(|line| line.kind == VisualLineKind::TableHeader));
        let row = doc
            .lines
            .iter()
            .find(|line| line.kind == VisualLineKind::TableRow)
            .expect("table body row");
        assert_eq!(row.runs.len(), 2);
        assert_eq!(row.runs[0].text, "1");
        assert_eq!(row.runs[1].text, "2");
    }

    #[test]
    fn live_preview_keeps_fenced_code_component_stable_while_editing() {
        let source = "```rust\nfn main() {}\n```\n";
        let projection = MarkdownProjection::parse(source);
        let caret = source.find("main").unwrap();
        let doc = build_visual_document(source, &projection, EditorMode::LivePreview, caret);
        let code_lines = doc
            .lines
            .iter()
            .filter(|line| line.block == BlockKind::CodeBlock)
            .collect::<Vec<_>>();
        assert_eq!(code_lines.len(), 1);
        assert_eq!(code_lines[0].text(), "fn main() {}");
        assert!(!code_lines[0].text().contains("```"));

        let source_mode = build_visual_document(source, &projection, EditorMode::Source, caret);
        assert_eq!(source_mode.lines[0].text(), "```rust");
        assert_eq!(source_mode.lines[2].text(), "```");
    }

    #[test]
    fn soft_wrap_preserves_unicode_source_ranges() {
        let source = "ab你好cd";
        let projection = MarkdownProjection::parse(source);
        let doc = build_visual_document(source, &projection, EditorMode::LivePreview, 0);
        let wrapped = wrap_visual_document(&doc, 4);
        assert_eq!(wrapped.lines.len(), 2);
        assert_eq!(wrapped.lines[0].text(), "ab你");
        assert_eq!(wrapped.lines[1].text(), "好cd");
        assert_eq!(wrapped.lines.last().unwrap().source.end, source.len());
    }

    #[test]
    fn pixel_wrap_uses_measured_word_widths_and_preserves_ranges() {
        let source = "wide words fit";
        let projection = MarkdownProjection::parse(source);
        let doc = build_visual_document(source, &projection, EditorMode::LivePreview, 0);
        let wrapped = wrap_visual_document_by_width(&doc, 8.0, |_, run| {
            Ok::<_, ()>(
                run.text
                    .chars()
                    .map(|ch| if ch == 'w' { 4.0 } else { 1.0 })
                    .sum(),
            )
        })
        .unwrap();
        assert_eq!(wrapped.lines.len(), 3);
        assert_eq!(wrapped.lines[0].text(), "wide ");
        assert_eq!(wrapped.lines[1].text(), "words ");
        assert_eq!(wrapped.lines[2].text(), "fit");
        assert_eq!(wrapped.lines.last().unwrap().source.end, source.len());
    }

    #[test]
    fn table_column_fitting_fills_and_bounds_the_grid() {
        assert_eq!(fit_table_columns(&[], 100.0, 20.0), Vec::<f32>::new());
        let roomy = fit_table_columns(&[30.0, 50.0], 100.0, 20.0);
        assert!((roomy.iter().sum::<f32>() - 100.0).abs() < 0.01);
        assert!(roomy[1] > roomy[0]);

        let narrow = fit_table_columns(&[200.0, 100.0, 50.0], 90.0, 48.0);
        assert!((narrow.iter().sum::<f32>() - 90.0).abs() < 0.01);
        assert!(narrow.iter().all(|width| *width >= 1.0));
    }

    #[test]
    fn read_only_projection_reuses_live_preview_and_large_documents_wrap() {
        let source = (0..2_000)
            .map(|index| format!("## Row {index} with **bold** and 你好\n"))
            .collect::<String>();
        let projection = MarkdownProjection::parse(&source);
        let live = build_visual_document(&source, &projection, EditorMode::LivePreview, 0);
        let read_only = build_visual_document(&source, &projection, EditorMode::ReadOnly, 0);
        assert_eq!(live, read_only);
        let narrow = wrap_visual_document(&read_only, 12);
        assert!(narrow.lines.len() > read_only.lines.len());
        assert_eq!(narrow.lines.last().unwrap().source.end, source.len());
    }
}
