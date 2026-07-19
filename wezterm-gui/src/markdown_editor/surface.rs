use super::{BlockKind, EditorMode, InlineStyle, MarkdownProjection, ProjectedObject};
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use unicode_linebreak::linebreaks;
use unicode_segmentation::UnicodeSegmentation;
#[cfg(test)]
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    pub text: Arc<String>,
    pub style: InlineStyle,
    pub atomic: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VisualLine {
    pub source: Range<usize>,
    pub block: BlockKind,
    pub kind: VisualLineKind,
    /// Pixel indent applied by the host to wrapped continuation rows. Stored
    /// as bits so visual documents retain exact Eq/cache semantics.
    pub continuation_indent_bits: u32,
    pub runs: Vec<VisualRun>,
}

impl VisualLine {
    pub(crate) fn continuation_indent(&self) -> f32 {
        f32::from_bits(self.continuation_indent_bits)
    }

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
                        text: Arc::new(grapheme.to_string()),
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
#[cfg(test)]
pub(crate) fn wrap_visual_document_by_width<E, F>(
    document: &VisualDocument,
    max_width: f32,
    mut measure: F,
) -> Result<VisualDocument, E>
where
    F: FnMut(BlockKind, InlineStyle, &str) -> Result<f32, E>,
{
    let max_width = max_width.max(1.0);
    let mut lines = Vec::with_capacity(document.lines.len());
    for line in &document.lines {
        wrap_visual_line_by_width(line, max_width, &mut measure, &mut lines)?;
    }
    Ok(VisualDocument { lines })
}

/// Reuse the wrapped output of unchanged visual lines across document
/// revisions. Compact, line-relative hashes avoid retaining another copy of a
/// large note while still allowing edits above a line to shift its byte ranges
/// without forcing its text to be measured and wrapped again.
pub(crate) fn wrap_visual_document_by_width_cached<E, F>(
    document: &VisualDocument,
    max_width: f32,
    wrap_key: usize,
    cache: &mut VisualWrapCache,
    mut measure: F,
) -> Result<Arc<VisualDocument>, E>
where
    F: FnMut(BlockKind, InlineStyle, &str) -> Result<f32, E>,
{
    if cache.wrap_key != Some(wrap_key) {
        *cache = VisualWrapCache {
            wrap_key: Some(wrap_key),
            ..VisualWrapCache::default()
        };
    }

    let max_width = max_width.max(1.0);
    let mut lines = Vec::with_capacity(document.lines.len());
    let mut wrapped_line_ranges = Vec::with_capacity(document.lines.len());
    let mut source_line_hashes = Vec::with_capacity(document.lines.len());
    for (line_index, line) in document.lines.iter().enumerate() {
        let wrapped_start = lines.len();
        let hash = normalized_visual_line_hash(line);
        source_line_hashes.push(hash);
        // Ordinary typing does not add/remove physical rows. Try the same
        // index first and avoid hashing the entire document on every key.
        let reused_index = cache
            .source_line_hashes
            .get(line_index)
            .filter(|old_hash| **old_hash == hash)
            .map(|_| line_index)
            .or_else(|| {
                cache
                    .source_line_lookup
                    .get(&hash)
                    .and_then(|indices| indices.first().copied())
            });
        let reused = reused_index.and_then(|index| {
            let old_source_start = *cache.source_line_starts.get(index)?;
            let wrapped = cache.wrapped_line_ranges.get(index)?.clone();
            Some((old_source_start, wrapped))
        });

        if let Some((old_source_start, wrapped_range)) = reused {
            for wrapped in &cache.wrapped.lines[wrapped_range] {
                lines.push(shift_visual_line(
                    wrapped,
                    old_source_start,
                    line.source.start,
                ));
            }
        } else {
            wrap_visual_line_by_width(line, max_width, &mut measure, &mut lines)?;
        }
        wrapped_line_ranges.push(wrapped_start..lines.len());
    }

    let wrapped = Arc::new(VisualDocument { lines });
    let source_line_starts = document
        .lines
        .iter()
        .map(|line| line.source.start)
        .collect::<Vec<_>>();
    let estimated_bytes = source_line_hashes
        .capacity()
        .saturating_mul(std::mem::size_of::<u64>())
        .saturating_add(
            source_line_starts
                .capacity()
                .saturating_mul(std::mem::size_of::<usize>()),
        )
        .saturating_add(
            wrapped_line_ranges
                .capacity()
                .saturating_mul(std::mem::size_of::<Range<usize>>()),
        )
        .saturating_add(
            source_line_hashes
                .len()
                .saturating_mul(std::mem::size_of::<(u64, usize)>()),
        );
    const WRAP_CACHE_MAX_BYTES: usize = 32 * 1024 * 1024;
    if estimated_bytes <= WRAP_CACHE_MAX_BYTES {
        cache.source_line_hashes = source_line_hashes;
        cache.source_line_starts = source_line_starts;
        cache.wrapped = Arc::clone(&wrapped);
        cache.wrapped_line_ranges = wrapped_line_ranges;
        cache.source_line_lookup.clear();
        for (index, hash) in cache.source_line_hashes.iter().copied().enumerate() {
            cache
                .source_line_lookup
                .entry(hash)
                .or_default()
                .push(index);
        }
    } else {
        // Keep the current wrapped document in the host, but bound the
        // cross-revision metadata retained solely for reuse.
        cache.source_line_hashes.clear();
        cache.source_line_starts.clear();
        cache.wrapped = Arc::new(VisualDocument::default());
        cache.wrapped_line_ranges.clear();
        cache.source_line_lookup.clear();
    }
    Ok(wrapped)
}

fn wrap_visual_line_by_width<E, F>(
    line: &VisualLine,
    max_width: f32,
    measure: &mut F,
    lines: &mut Vec<VisualLine>,
) -> Result<(), E>
where
    F: FnMut(BlockKind, InlineStyle, &str) -> Result<f32, E>,
{
    if line.runs.is_empty()
        || matches!(line.block, BlockKind::CodeBlock | BlockKind::Table)
        || matches!(line.kind, VisualLineKind::Rule | VisualLineKind::Image)
    {
        lines.push(line.clone());
        return Ok(());
    }

    let hanging_indent = if let Some(prefix) = continuation_prefix(line) {
        Some(measure(line.block, InlineStyle::default(), &prefix)?.max(0.0))
    } else {
        None
    };
    let mut current = empty_visual_line(line, line.source.start);
    let mut width = 0.0f32;
    for run in &line.runs {
        let break_anywhere = run.style.link && looks_like_external_url(&run.text);
        let source_matches_text =
            !run.atomic && run.source.end.saturating_sub(run.source.start) == run.text.len();
        if source_matches_text {
            let mut relative = 0usize;
            for (piece_end, _) in linebreaks(&run.text) {
                let piece = &run.text[relative..piece_end];
                let piece_source =
                    run.source.start + relative..run.source.start + relative + piece.len();
                let piece_width = measure(line.block, run.style, piece)?.max(0.0);
                let whitespace = piece.chars().all(char::is_whitespace);
                if !whitespace
                    && (piece_width > max_width
                        || (break_anywhere
                            && visual_line_has_non_whitespace(&current)
                            && width + piece_width > max_width))
                {
                    for (grapheme_relative, grapheme) in piece.grapheme_indices(true) {
                        let grapheme_source = piece_source.start + grapheme_relative
                            ..piece_source.start + grapheme_relative + grapheme.len();
                        let grapheme_width = measure(line.block, run.style, grapheme)?.max(0.0);
                        let grapheme_is_whitespace = grapheme.chars().all(char::is_whitespace);
                        if !grapheme_is_whitespace
                            && visual_line_has_non_whitespace(&current)
                            && width + grapheme_width > max_width
                        {
                            lines.push(current);
                            (current, width) = continuation_visual_line(
                                line,
                                grapheme_source.start,
                                hanging_indent.as_ref(),
                            );
                        }
                        append_text_run(&mut current, grapheme_source, grapheme, run.style, false);
                        width += grapheme_width;
                    }
                    relative = piece_end;
                    continue;
                }

                if !whitespace
                    && visual_line_has_non_whitespace(&current)
                    && width + piece_width > max_width
                {
                    lines.push(current);
                    (current, width) =
                        continuation_visual_line(line, piece_source.start, hanging_indent.as_ref());
                }
                append_text_run(&mut current, piece_source, piece, run.style, false);
                width += piece_width;
                relative = piece_end;
            }
            continue;
        }

        let piece_width = measure(line.block, run.style, &run.text)?.max(0.0);
        let whitespace = run.text.chars().all(char::is_whitespace);
        if !whitespace
            && (piece_width > max_width
                || (break_anywhere
                    && visual_line_has_non_whitespace(&current)
                    && width + piece_width > max_width))
        {
            for (relative, grapheme) in run.text.grapheme_indices(true) {
                let grapheme_source = if run.atomic {
                    run.source.clone()
                } else {
                    run.source.start + relative..run.source.start + relative + grapheme.len()
                };
                let grapheme_width = measure(line.block, run.style, grapheme)?.max(0.0);
                let grapheme_is_whitespace = grapheme.chars().all(char::is_whitespace);
                if !grapheme_is_whitespace
                    && visual_line_has_non_whitespace(&current)
                    && width + grapheme_width > max_width
                {
                    lines.push(current);
                    (current, width) = continuation_visual_line(
                        line,
                        grapheme_source.start,
                        hanging_indent.as_ref(),
                    );
                }
                append_text_run(
                    &mut current,
                    grapheme_source,
                    grapheme,
                    run.style,
                    run.atomic,
                );
                width += grapheme_width;
            }
            continue;
        }

        if !whitespace
            && visual_line_has_non_whitespace(&current)
            && width + piece_width > max_width
        {
            lines.push(current);
            (current, width) =
                continuation_visual_line(line, run.source.start, hanging_indent.as_ref());
        }
        append_text_run(
            &mut current,
            run.source.clone(),
            &run.text,
            run.style,
            run.atomic,
        );
        width += piece_width;
    }
    current.source.end = current.source.end.max(line.source.end);
    lines.push(current);
    Ok(())
}

fn looks_like_external_url(text: &str) -> bool {
    let text = text.trim_start();
    text.starts_with("https://") || text.starts_with("http://")
}

fn continuation_prefix(line: &VisualLine) -> Option<String> {
    if line.block == BlockKind::Properties {
        return line
            .runs
            .first()
            .filter(|run| run.atomic && run.style.strong)
            .map(|run| run.text.as_str().to_string());
    }

    let mut prefix = String::new();
    for run in &line.runs {
        let text = run.text.as_str();
        let trimmed = text.trim_end();
        let is_task = matches!(trimmed, "☐" | "☑");
        let is_bullet = trimmed == "•";
        let is_ordered = trimmed
            .strip_suffix('.')
            .or_else(|| trimmed.strip_suffix(')'))
            .is_some_and(|digits| {
                !digits.is_empty() && digits.chars().all(|ch| ch.is_ascii_digit())
            });
        if !run.atomic || !(is_task || is_bullet || is_ordered) || !text.ends_with(' ') {
            break;
        }
        prefix.push_str(text);
    }
    (!prefix.is_empty()).then_some(prefix)
}

fn continuation_visual_line(
    template: &VisualLine,
    source_start: usize,
    hanging_indent: Option<&f32>,
) -> (VisualLine, f32) {
    let mut line = empty_visual_line(template, source_start);
    let Some(width) = hanging_indent else {
        return (line, 0.0);
    };
    line.continuation_indent_bits = width.to_bits();
    (line, *width)
}

fn normalized_visual_line_hash(line: &VisualLine) -> u64 {
    let mut hasher = DefaultHasher::new();
    line.block.hash(&mut hasher);
    line.kind.hash(&mut hasher);
    line.continuation_indent_bits.hash(&mut hasher);
    line.source
        .end
        .saturating_sub(line.source.start)
        .hash(&mut hasher);
    for run in &line.runs {
        run.source
            .start
            .saturating_sub(line.source.start)
            .hash(&mut hasher);
        run.source
            .end
            .saturating_sub(line.source.start)
            .hash(&mut hasher);
        run.text.hash(&mut hasher);
        run.style.hash(&mut hasher);
        run.atomic.hash(&mut hasher);
    }
    hasher.finish()
}

fn shift_visual_line(line: &VisualLine, old_start: usize, new_start: usize) -> VisualLine {
    let shift = |range: &Range<usize>| {
        if new_start >= old_start {
            let delta = new_start - old_start;
            range.start.saturating_add(delta)..range.end.saturating_add(delta)
        } else {
            let delta = old_start - new_start;
            range.start.saturating_sub(delta)..range.end.saturating_sub(delta)
        }
    };
    VisualLine {
        source: shift(&line.source),
        block: line.block,
        kind: line.kind,
        continuation_indent_bits: line.continuation_indent_bits,
        runs: line
            .runs
            .iter()
            .map(|run| VisualRun {
                source: shift(&run.source),
                text: run.text.clone(),
                style: run.style,
                atomic: run.atomic,
            })
            .collect(),
    }
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
        continuation_indent_bits: 0.0f32.to_bits(),
        runs: vec![],
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct VisualDocument {
    pub lines: Vec<VisualLine>,
}

impl VisualDocument {
    /// Approximate the amount of text that needs soft wrapping. Code, tables,
    /// rules, and images are laid out by their component painters and do not
    /// contribute to the expensive proportional-font wrap pass.
    pub(crate) fn estimated_wrap_work_bytes(&self) -> usize {
        self.lines
            .iter()
            .filter(|line| {
                !matches!(line.block, BlockKind::CodeBlock | BlockKind::Table)
                    && !matches!(line.kind, VisualLineKind::Rule | VisualLineKind::Image)
            })
            .flat_map(|line| &line.runs)
            .fold(0usize, |total, run| total.saturating_add(run.text.len()))
    }
}

#[derive(Debug, Default)]
pub(crate) struct VisualWrapCache {
    wrap_key: Option<usize>,
    source_line_hashes: Vec<u64>,
    source_line_starts: Vec<usize>,
    wrapped: Arc<VisualDocument>,
    wrapped_line_ranges: Vec<Range<usize>>,
    source_line_lookup: HashMap<u64, Vec<usize>>,
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
    let mut visible_index = 0usize;
    let mut projected_index = 0usize;
    let mut lines = vec![];
    let mut line = VisualLine {
        source: 0..0,
        block: BlockKind::Paragraph,
        kind: VisualLineKind::Text,
        continuation_indent_bits: 0.0f32.to_bits(),
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
                if range.start <= caret
                    && caret <= range.end
                    && matches!(
                        object,
                        ProjectedObject::WikiLink { .. }
                            | ProjectedObject::Callout { .. }
                            | ProjectedObject::BlockId { .. }
                    )
                {
                    append_revealed_source_range(
                        &mut lines,
                        &mut line,
                        source,
                        &range,
                        BlockKind::Paragraph,
                    );
                    idx = range.end.min(source.len());
                    object_index += 1;
                    continue;
                }
                match object {
                    ProjectedObject::TaskCheckbox {
                        source: object_source,
                        checked,
                    } => {
                        append_run(
                            &mut line,
                            VisualRun {
                                source: object_source.clone(),
                                text: Arc::new(if *checked { "☑ " } else { "☐ " }.to_string()),
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
                                text: Arc::new(text.clone()),
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
                            continuation_indent_bits: 0.0f32.to_bits(),
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
                            continuation_indent_bits: 0.0f32.to_bits(),
                            runs: vec![VisualRun {
                                source: object_source.clone(),
                                text: Arc::new(format!("▧ {label}")),
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
                                continuation_indent_bits: 0.0f32.to_bits(),
                                runs: if text.is_empty() {
                                    vec![]
                                } else {
                                    vec![VisualRun {
                                        source: content_start..text_end,
                                        text: Arc::new(text.to_string()),
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
                                    text: Arc::new(cell.text.clone()),
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
                                continuation_indent_bits: 0.0f32.to_bits(),
                                runs,
                            });
                        }
                    }
                    ProjectedObject::Frontmatter {
                        source: object_source,
                        fields,
                    } => {
                        flush_line_if_content(&mut lines, &mut line, idx);
                        if fields.is_empty() {
                            lines.push(VisualLine {
                                source: object_source.clone(),
                                block: BlockKind::Properties,
                                kind: VisualLineKind::Text,
                                continuation_indent_bits: 0.0f32.to_bits(),
                                runs: vec![VisualRun {
                                    source: object_source.clone(),
                                    text: Arc::new("Properties".to_string()),
                                    style: InlineStyle {
                                        strong: true,
                                        ..InlineStyle::default()
                                    },
                                    atomic: true,
                                }],
                            });
                        } else {
                            for field in fields {
                                lines.push(VisualLine {
                                    source: field.source.clone(),
                                    block: BlockKind::Properties,
                                    kind: VisualLineKind::Text,
                                    continuation_indent_bits: 0.0f32.to_bits(),
                                    runs: vec![
                                        VisualRun {
                                            source: field.key_source.clone(),
                                            text: Arc::new(format!("{}: ", field.key)),
                                            style: InlineStyle {
                                                strong: true,
                                                ..InlineStyle::default()
                                            },
                                            atomic: true,
                                        },
                                        VisualRun {
                                            source: field.value_source.clone(),
                                            text: Arc::new(field.value.clone()),
                                            style: InlineStyle::default(),
                                            atomic: true,
                                        },
                                    ],
                                });
                            }
                        }
                    }
                    ProjectedObject::WikiLink {
                        source: object_source,
                        label,
                        embed,
                        resolved_path,
                        rendered_lines,
                        ..
                    } => {
                        if *embed {
                            flush_line_if_content(&mut lines, &mut line, idx);
                            let is_image = resolved_path.as_deref().is_some_and(|path| {
                                Path::new(path)
                                    .extension()
                                    .and_then(|extension| extension.to_str())
                                    .is_some_and(|extension| {
                                        matches!(
                                            extension.to_ascii_lowercase().as_str(),
                                            "png"
                                                | "jpg"
                                                | "jpeg"
                                                | "gif"
                                                | "webp"
                                                | "bmp"
                                                | "ico"
                                                | "tif"
                                                | "tiff"
                                        )
                                    })
                            });
                            let embedded = if rendered_lines.is_empty() {
                                vec![format!("▧ {label}")]
                            } else {
                                rendered_lines.clone()
                            };
                            for text in embedded {
                                lines.push(VisualLine {
                                    source: object_source.clone(),
                                    block: BlockKind::Embed,
                                    kind: if is_image {
                                        VisualLineKind::Image
                                    } else {
                                        VisualLineKind::Text
                                    },
                                    continuation_indent_bits: 0.0f32.to_bits(),
                                    runs: vec![VisualRun {
                                        source: object_source.clone(),
                                        text: Arc::new(text),
                                        style: InlineStyle::default(),
                                        atomic: true,
                                    }],
                                });
                            }
                        } else {
                            append_run(
                                &mut line,
                                VisualRun {
                                    source: object_source.clone(),
                                    text: Arc::new(label.clone()),
                                    style: InlineStyle {
                                        link: true,
                                        ..InlineStyle::default()
                                    },
                                    atomic: true,
                                },
                            );
                        }
                    }
                    ProjectedObject::Callout {
                        source: object_source,
                        kind,
                        title,
                        body,
                    } => {
                        flush_line_if_content(&mut lines, &mut line, idx);
                        lines.push(VisualLine {
                            source: object_source.clone(),
                            block: BlockKind::Callout,
                            kind: VisualLineKind::Text,
                            continuation_indent_bits: 0.0f32.to_bits(),
                            runs: vec![VisualRun {
                                source: object_source.clone(),
                                text: Arc::new(format!("◆ {kind}  {title}")),
                                style: InlineStyle {
                                    strong: true,
                                    ..InlineStyle::default()
                                },
                                atomic: true,
                            }],
                        });
                        for (body_source, text) in body {
                            lines.push(VisualLine {
                                source: body_source.clone(),
                                block: BlockKind::Callout,
                                kind: VisualLineKind::Text,
                                continuation_indent_bits: 0.0f32.to_bits(),
                                runs: vec![VisualRun {
                                    source: body_source.clone(),
                                    text: Arc::new(text.clone()),
                                    style: InlineStyle::default(),
                                    atomic: true,
                                }],
                            });
                        }
                    }
                    ProjectedObject::BlockId { .. } => {
                        // Obsidian block IDs are anchors, not prose. Live
                        // Preview hides them until the caret enters the token.
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

        if source.as_bytes()[idx] == b'\n' {
            let next = idx + 1;
            flush_line(&mut lines, &mut line, next);
            idx = next;
            continue;
        }

        while visible_index < visible.len() && visible[visible_index].end <= idx {
            visible_index += 1;
        }
        let Some(visible_range) = visible.get(visible_index) else {
            // There can still be line breaks after the final rendered range;
            // preserve them so source positions and empty lines stay exact.
            idx = next_visual_boundary(source, idx, source.len(), objects.get(object_index));
            continue;
        };
        if idx < visible_range.start {
            idx = next_visual_boundary(source, idx, visible_range.start, objects.get(object_index));
            continue;
        }

        while projected_index < projection.text.len()
            && projection.text[projected_index].source.end <= idx
        {
            projected_index += 1;
        }
        let projected = projection
            .text
            .get(projected_index)
            .filter(|span| span.source.start <= idx && idx < span.source.end);
        let style = projected.map(|span| span.style).unwrap_or_default();
        line.block = projected.map(|span| span.block).unwrap_or(line.block);

        // Append a whole contiguous source slice with the same visibility and
        // style. The previous implementation allocated one String and ran two
        // binary searches for every Unicode scalar, which made large Notes
        // visibly rebuild on each edit.
        let mut end = visible_range.end.min(source.len());
        if let Some(span) = projected {
            end = end.min(span.source.end);
        } else if let Some(span) = projection.text.get(projected_index) {
            if span.source.start > idx {
                end = end.min(span.source.start);
            }
        }
        if let Some(object) = objects.get(object_index) {
            let object_start = object.source().start;
            if object_start > idx {
                end = end.min(object_start);
            }
        }
        if let Some(relative) = source[idx..end].find('\n') {
            end = idx + relative;
        }
        if end == idx {
            // Zero-length parser events must never stall the visual scanner.
            let ch = source[idx..].chars().next().expect("valid source char");
            end = idx + ch.len_utf8();
        }
        append_run(
            &mut line,
            VisualRun {
                source: idx..end,
                text: Arc::new(source[idx..end].to_string()),
                style,
                atomic: false,
            },
        );
        idx = end;
    }
    flush_line(&mut lines, &mut line, source.len());
    if lines.is_empty() {
        lines.push(VisualLine {
            source: 0..0,
            block: BlockKind::Paragraph,
            kind: VisualLineKind::Text,
            continuation_indent_bits: 0.0f32.to_bits(),
            runs: vec![],
        });
    }
    VisualDocument { lines }
}

fn next_visual_boundary(
    source: &str,
    idx: usize,
    limit: usize,
    object: Option<&&ProjectedObject>,
) -> usize {
    let mut end = limit.min(source.len());
    if let Some(object) = object {
        let object_start = object.source().start;
        if object_start > idx {
            end = end.min(object_start);
        }
    }
    if let Some(relative) = source[idx..end].find('\n') {
        return idx + relative;
    }
    end.max(
        idx + source[idx..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(0),
    )
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
            continuation_indent_bits: 0.0f32.to_bits(),
            runs: if text.is_empty() {
                vec![]
            } else {
                vec![VisualRun {
                    source: start..end,
                    text: Arc::new(text.to_string()),
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
            continuation_indent_bits: 0.0f32.to_bits(),
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
            Arc::make_mut(&mut last.text).push_str(&run.text);
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

fn append_text_run(
    line: &mut VisualLine,
    source: Range<usize>,
    text: &str,
    style: InlineStyle,
    atomic: bool,
) {
    if let Some(last) = line.runs.last_mut() {
        if last.atomic && atomic && last.style == style && last.source == source {
            Arc::make_mut(&mut last.text).push_str(text);
            line.source.end = line.source.end.max(source.end);
            return;
        }
        if !last.atomic && !atomic && last.style == style && last.source.end == source.start {
            last.source.end = source.end;
            Arc::make_mut(&mut last.text).push_str(text);
            line.source.end = line.source.end.max(source.end);
            return;
        }
    }
    if line.runs.is_empty() {
        line.source.start = source.start;
    }
    line.source.end = line.source.end.max(source.end);
    line.runs.push(VisualRun {
        source,
        text: Arc::new(text.to_string()),
        style,
        atomic,
    });
}

fn append_revealed_source_range(
    lines: &mut Vec<VisualLine>,
    line: &mut VisualLine,
    source: &str,
    range: &Range<usize>,
    block: BlockKind,
) {
    let mut cursor = range.start;
    for chunk in source[range.clone()].split_inclusive('\n') {
        let text = chunk.strip_suffix('\n').unwrap_or(chunk);
        let end = cursor + text.len();
        line.block = block;
        if !text.is_empty() {
            append_text_run(line, cursor..end, text, InlineStyle::default(), false);
        }
        cursor += chunk.len();
        if chunk.ends_with('\n') {
            flush_line(lines, line, cursor);
        }
    }
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
        continuation_indent_bits: 0.0f32.to_bits(),
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
    fn live_preview_keeps_frontmatter_projected_when_caret_is_inside_it() {
        let source = "---\ntitle: Example\ntags:\n  - notes\n---\n\n# Body\n";
        let projection = MarkdownProjection::parse(source);
        let doc = build_visual_document(source, &projection, EditorMode::LivePreview, 0);
        let text = doc
            .lines
            .iter()
            .map(VisualLine::text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(!text.contains("---"));
        assert!(!text.contains("  - notes"));
        assert!(text.contains("title: Example"));
        assert!(text.contains("tags:"));
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
        assert_eq!(row.runs[0].text.as_str(), "1");
        assert_eq!(row.runs[1].text.as_str(), "2");
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
    fn obsidian_image_embed_projects_as_an_image_component() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("assets")).unwrap();
        std::fs::write(
            temp.path().join("assets/diagram.bmp"),
            b"decoded by the painter",
        )
        .unwrap();
        let source = "![[assets/diagram.bmp]]\n\nOutside";
        let mut projection = MarkdownProjection::parse(source);
        projection.resolve_vault_links(temp.path(), "Home.md");

        let doc = build_visual_document(
            source,
            &projection,
            EditorMode::LivePreview,
            source.find("Outside").unwrap(),
        );
        assert!(doc
            .lines
            .iter()
            .any(|line| line.kind == VisualLineKind::Image));
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
        let wrapped = wrap_visual_document_by_width(&doc, 8.0, |_, _, text| {
            Ok::<_, ()>(
                text.chars()
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
    fn pixel_wrap_uses_hanging_indent_for_list_continuations() {
        let source = "- Alpha beta gamma";
        let projection = MarkdownProjection::parse(source);
        let doc = build_visual_document(source, &projection, EditorMode::LivePreview, source.len());
        let wrapped = wrap_visual_document_by_width(&doc, 9.0, |_, _, text| {
            Ok::<_, ()>(text.graphemes(true).count() as f32)
        })
        .unwrap();
        let lines = wrapped
            .lines
            .iter()
            .map(VisualLine::text)
            .collect::<Vec<_>>();

        assert_eq!(lines, ["• Alpha ", "beta ", "gamma"]);
        assert_eq!(wrapped.lines[0].continuation_indent(), 0.0);
        assert_eq!(wrapped.lines[1].continuation_indent(), 2.0);
        assert_eq!(wrapped.lines[2].continuation_indent(), 2.0);
    }

    #[test]
    fn pixel_wrap_uses_remaining_row_width_for_long_urls() {
        let source = "- https://example.test/a/long_resource_name.zip";
        let projection = MarkdownProjection::parse(source);
        let doc = build_visual_document(source, &projection, EditorMode::LivePreview, source.len());
        let wrapped = wrap_visual_document_by_width(&doc, 18.0, |_, _, text| {
            Ok::<_, ()>(text.graphemes(true).count() as f32)
        })
        .unwrap();

        let rows = wrapped
            .lines
            .iter()
            .map(VisualLine::text)
            .collect::<Vec<_>>();
        assert!(rows.len() > 1);
        assert_eq!(rows[0].graphemes(true).count(), 18);
        assert!(rows.iter().skip(1).all(|row| !row.starts_with('•')));
    }

    #[test]
    fn pixel_wrap_obeys_unicode_cjk_line_break_opportunities() {
        let source = "你好，世界。测试（括号）继续";
        let projection = MarkdownProjection::parse(source);
        let doc = build_visual_document(source, &projection, EditorMode::LivePreview, source.len());
        let wrapped = wrap_visual_document_by_width(&doc, 4.0, |_, _, text| {
            Ok::<_, ()>(text.graphemes(true).count() as f32)
        })
        .unwrap();
        let lines = wrapped
            .lines
            .iter()
            .map(VisualLine::text)
            .collect::<Vec<_>>();

        assert_eq!(lines.concat(), source);
        assert!(lines.iter().skip(1).all(|line| !line.starts_with('，')
            && !line.starts_with('。')
            && !line.starts_with('）')));
        assert!(lines
            .iter()
            .take(lines.len().saturating_sub(1))
            .all(|line| !line.ends_with('（')));
    }

    #[test]
    fn pixel_wrap_breaks_long_atomic_property_values() {
        let source = "---\nsource: https://example.com/a/very/long/path\n---\n";
        let projection = MarkdownProjection::parse(source);
        let doc = build_visual_document(source, &projection, EditorMode::LivePreview, 0);
        let wrapped = wrap_visual_document_by_width(&doc, 12.0, |_, _, text| {
            Ok::<_, ()>(text.graphemes(true).count() as f32)
        })
        .unwrap();

        assert!(wrapped.lines.len() > doc.lines.len());
        assert!(wrapped
            .lines
            .iter()
            .all(|line| line.text().graphemes(true).count() <= 12));
        assert!(wrapped
            .lines
            .iter()
            .filter(|line| line.block == BlockKind::Properties)
            .skip(1)
            .all(|line| line.continuation_indent() > 0.0));
    }

    #[test]
    fn cached_pixel_wrap_reuses_shifted_unchanged_lines() {
        let source = "alpha words need wrapping\nbeta content stays stable\n";
        let projection = MarkdownProjection::parse(source);
        let visual = build_visual_document(source, &projection, EditorMode::LivePreview, 0);
        let mut cache = VisualWrapCache::default();
        let mut initial_measurements = 0usize;
        wrap_visual_document_by_width_cached(&visual, 9.0, 1, &mut cache, |_, _, text| {
            initial_measurements += 1;
            Ok::<_, ()>(text.chars().count() as f32)
        })
        .unwrap();
        assert!(initial_measurements > 0);

        let shifted_source = format!("new prefix line\n{source}");
        let shifted_projection = MarkdownProjection::parse(&shifted_source);
        let shifted_visual = build_visual_document(
            &shifted_source,
            &shifted_projection,
            EditorMode::LivePreview,
            0,
        );
        let mut cached_measurements = 0usize;
        let cached = wrap_visual_document_by_width_cached(
            &shifted_visual,
            9.0,
            1,
            &mut cache,
            |_, _, text| {
                cached_measurements += 1;
                Ok::<_, ()>(text.chars().count() as f32)
            },
        )
        .unwrap();
        let mut full_measurements = 0usize;
        let full = wrap_visual_document_by_width(&shifted_visual, 9.0, |_, _, text| {
            full_measurements += 1;
            Ok::<_, ()>(text.chars().count() as f32)
        })
        .unwrap();

        assert_eq!(*cached, full);
        assert!(cached_measurements < full_measurements);
    }

    #[test]
    fn wrap_work_estimate_ignores_component_layout_text() {
        let prose = "visible prose ".repeat(700);
        let source = format!("{prose}\n\n```text\n{}\n```\n", "code ".repeat(2_000));
        let projection = MarkdownProjection::parse(&source);
        let visual = build_visual_document(&source, &projection, EditorMode::LivePreview, 0);

        let estimated = visual.estimated_wrap_work_bytes();
        assert!(estimated >= prose.trim_end().len());
        assert!(estimated < source.len() / 2);
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
