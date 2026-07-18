use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockKind {
    Paragraph,
    Heading(u8),
    Quote,
    List,
    ListItem,
    CodeBlock,
    Table,
    Other,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct InlineStyle {
    pub strong: bool,
    pub emphasis: bool,
    pub strikethrough: bool,
    pub code: bool,
    pub link: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MarkdownSyntaxKind {
    Heading,
    Emphasis,
    Strong,
    Strikethrough,
    Link,
    Image,
    InlineCode,
    List,
    ListItem,
    Quote,
    CodeBlock,
    Table,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MarkdownSyntaxNode {
    pub kind: MarkdownSyntaxKind,
    pub source: Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectedText {
    pub source: Range<usize>,
    pub text: String,
    pub style: InlineStyle,
    pub block: BlockKind,
    pub link_target: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectedTableCell {
    pub source: Range<usize>,
    pub text: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum TableAlignment {
    #[default]
    None,
    Left,
    Center,
    Right,
}

impl From<Alignment> for TableAlignment {
    fn from(value: Alignment) -> Self {
        match value {
            Alignment::None => Self::None,
            Alignment::Left => Self::Left,
            Alignment::Center => Self::Center,
            Alignment::Right => Self::Right,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectedTable {
    pub source: Range<usize>,
    pub rows: Vec<Vec<ProjectedTableCell>>,
    pub alignments: Vec<TableAlignment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectedCodeBlock {
    /// Full Markdown range, including the opening/closing fences when fenced.
    pub source: Range<usize>,
    /// Exact editable code content, excluding fences and language metadata.
    pub content: Range<usize>,
    pub text: String,
    pub language: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProjectedObject {
    Rule {
        source: Range<usize>,
    },
    TaskCheckbox {
        source: Range<usize>,
        checked: bool,
    },
    ListMarker {
        source: Range<usize>,
        text: String,
    },
    Image {
        source: Range<usize>,
        target: String,
        title: String,
        alt: String,
    },
    CodeBlock(ProjectedCodeBlock),
    Table(ProjectedTable),
}

impl ProjectedObject {
    pub(crate) fn source(&self) -> Range<usize> {
        match self {
            Self::Rule { source }
            | Self::TaskCheckbox { source, .. }
            | Self::ListMarker { source, .. }
            | Self::Image { source, .. } => source.clone(),
            Self::CodeBlock(code) => code.source.clone(),
            Self::Table(table) => table.source.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectionBlock {
    pub kind: BlockKind,
    pub source: Range<usize>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct MarkdownProjection {
    pub text: Vec<ProjectedText>,
    pub objects: Vec<ProjectedObject>,
    pub blocks: Vec<ProjectionBlock>,
    pub syntax: Vec<MarkdownSyntaxNode>,
}

#[derive(Debug, Clone, Default)]
struct StyleState {
    strong: usize,
    emphasis: usize,
    strikethrough: usize,
    code: usize,
    links: Vec<String>,
}

impl StyleState {
    fn style(&self) -> InlineStyle {
        InlineStyle {
            strong: self.strong > 0,
            emphasis: self.emphasis > 0,
            strikethrough: self.strikethrough > 0,
            code: self.code > 0,
            link: !self.links.is_empty(),
        }
    }
}

#[derive(Debug)]
struct ImageBuilder {
    source: Range<usize>,
    target: String,
    title: String,
    alt: String,
}

#[derive(Debug)]
struct CodeBlockBuilder {
    source: Range<usize>,
    content: Option<Range<usize>>,
    text: String,
    language: Option<String>,
}

#[derive(Debug)]
struct TableBuilder {
    source: Range<usize>,
    rows: Vec<Vec<ProjectedTableCell>>,
    alignments: Vec<TableAlignment>,
    current_row: Vec<ProjectedTableCell>,
    current_cell_range: Option<Range<usize>>,
    current_cell_text: String,
}

impl MarkdownProjection {
    pub(crate) fn parse(source: &str) -> Self {
        let options =
            Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
        let mut projection = Self::default();
        let mut styles = StyleState::default();
        let mut block_stack = vec![BlockKind::Paragraph];
        let mut image: Option<ImageBuilder> = None;
        let mut code_block: Option<CodeBlockBuilder> = None;
        let mut table: Option<TableBuilder> = None;

        for (event, range) in Parser::new_ext(source, options).into_offset_iter() {
            match event {
                Event::Start(tag) => {
                    if let Some(kind) = syntax_kind_for_tag(&tag) {
                        projection.syntax.push(MarkdownSyntaxNode {
                            kind,
                            source: range.clone(),
                        });
                    }
                    if let Some(kind) = block_kind_for_tag(&tag) {
                        projection.blocks.push(ProjectionBlock {
                            kind,
                            source: range.clone(),
                        });
                        block_stack.push(kind);
                    }
                    match tag {
                        Tag::Strong => styles.strong += 1,
                        Tag::Emphasis => styles.emphasis += 1,
                        Tag::Strikethrough => styles.strikethrough += 1,
                        Tag::Link { dest_url, .. } => styles.links.push(dest_url.to_string()),
                        Tag::Image {
                            dest_url, title, ..
                        } => {
                            image = Some(ImageBuilder {
                                source: range.clone(),
                                target: dest_url.to_string(),
                                title: title.to_string(),
                                alt: String::new(),
                            });
                        }
                        Tag::CodeBlock(kind) => {
                            styles.code += 1;
                            let language = match kind {
                                CodeBlockKind::Fenced(info) => info
                                    .split_whitespace()
                                    .next()
                                    .filter(|language| !language.is_empty())
                                    .map(ToOwned::to_owned),
                                CodeBlockKind::Indented => None,
                            };
                            code_block = Some(CodeBlockBuilder {
                                source: range.clone(),
                                content: None,
                                text: String::new(),
                                language,
                            });
                        }
                        Tag::Table(alignments) => {
                            table = Some(TableBuilder {
                                source: range.clone(),
                                rows: vec![],
                                alignments: alignments
                                    .iter()
                                    .copied()
                                    .map(TableAlignment::from)
                                    .collect(),
                                current_row: vec![],
                                current_cell_range: None,
                                current_cell_text: String::new(),
                            });
                        }
                        Tag::TableRow => {
                            if let Some(table) = table.as_mut() {
                                table.current_row.clear();
                            }
                        }
                        Tag::TableHead => {
                            if let Some(table) = table.as_mut() {
                                table.current_row.clear();
                            }
                        }
                        Tag::TableCell => {
                            if let Some(table) = table.as_mut() {
                                table.current_cell_range = Some(range.clone());
                                table.current_cell_text.clear();
                            }
                        }
                        _ => {}
                    }
                }
                Event::End(tag) => {
                    match tag {
                        TagEnd::Strong => styles.strong = styles.strong.saturating_sub(1),
                        TagEnd::Emphasis => styles.emphasis = styles.emphasis.saturating_sub(1),
                        TagEnd::Strikethrough => {
                            styles.strikethrough = styles.strikethrough.saturating_sub(1)
                        }
                        TagEnd::Link => {
                            styles.links.pop();
                        }
                        TagEnd::Image => {
                            if let Some(image) = image.take() {
                                projection.objects.push(ProjectedObject::Image {
                                    source: image.source,
                                    target: image.target,
                                    title: image.title,
                                    alt: image.alt,
                                });
                            }
                        }
                        TagEnd::CodeBlock => {
                            styles.code = styles.code.saturating_sub(1);
                            if let Some(mut code) = code_block.take() {
                                code.source = range.clone();
                                let content = code.content.take().unwrap_or_else(|| {
                                    empty_code_block_content_range(source, &code.source)
                                });
                                projection.objects.push(ProjectedObject::CodeBlock(
                                    ProjectedCodeBlock {
                                        source: code.source,
                                        content,
                                        text: code.text,
                                        language: code.language,
                                    },
                                ));
                            }
                        }
                        TagEnd::TableCell => {
                            if let Some(table) = table.as_mut() {
                                let raw_source = table
                                    .current_cell_range
                                    .take()
                                    .unwrap_or_else(|| range.clone());
                                let source = table_cell_content_range(
                                    source,
                                    raw_source,
                                    &table.current_cell_text,
                                );
                                table.current_row.push(ProjectedTableCell {
                                    source,
                                    text: std::mem::take(&mut table.current_cell_text),
                                });
                            }
                        }
                        TagEnd::TableRow => {
                            if let Some(table) = table.as_mut() {
                                if !table.current_row.is_empty() {
                                    table.rows.push(std::mem::take(&mut table.current_row));
                                }
                            }
                        }
                        TagEnd::TableHead => {
                            if let Some(table) = table.as_mut() {
                                if !table.current_row.is_empty() {
                                    table.rows.push(std::mem::take(&mut table.current_row));
                                }
                            }
                        }
                        TagEnd::Table => {
                            if let Some(mut table) = table.take() {
                                table.source = range.clone();
                                projection
                                    .objects
                                    .push(ProjectedObject::Table(ProjectedTable {
                                        source: table.source,
                                        rows: table.rows,
                                        alignments: table.alignments,
                                    }));
                            }
                        }
                        _ => {}
                    }
                    if block_kind_for_end(tag).is_some() && block_stack.len() > 1 {
                        block_stack.pop();
                    }
                }
                Event::Text(text) => {
                    if let Some(image) = image.as_mut() {
                        image.alt.push_str(&text);
                        continue;
                    }
                    if let Some(table) = table.as_mut() {
                        if table.current_cell_range.is_some() {
                            table.current_cell_text.push_str(&text);
                        }
                    }
                    let source_range = content_range(source, range, &text);
                    if let Some(code) = code_block.as_mut() {
                        code.text.push_str(&text);
                        code.content = Some(match code.content.take() {
                            Some(content) => content.start..source_range.end.max(content.end),
                            None => source_range.clone(),
                        });
                    }
                    projection.text.push(ProjectedText {
                        source: source_range,
                        text: text.to_string(),
                        style: styles.style(),
                        block: *block_stack.last().unwrap_or(&BlockKind::Other),
                        link_target: styles.links.last().cloned(),
                    });
                }
                Event::Code(text) => {
                    projection.syntax.push(MarkdownSyntaxNode {
                        kind: MarkdownSyntaxKind::InlineCode,
                        source: range.clone(),
                    });
                    let mut style = styles.style();
                    style.code = true;
                    let source_range = content_range(source, range, &text);
                    projection.text.push(ProjectedText {
                        source: source_range,
                        text: text.to_string(),
                        style,
                        block: *block_stack.last().unwrap_or(&BlockKind::Other),
                        link_target: styles.links.last().cloned(),
                    });
                }
                Event::Html(text) | Event::InlineHtml(text) => {
                    projection.text.push(ProjectedText {
                        source: range,
                        text: text.to_string(),
                        style: styles.style(),
                        block: *block_stack.last().unwrap_or(&BlockKind::Other),
                        link_target: None,
                    });
                }
                Event::SoftBreak | Event::HardBreak => {
                    projection.text.push(ProjectedText {
                        source: range,
                        text: "\n".to_string(),
                        style: styles.style(),
                        block: *block_stack.last().unwrap_or(&BlockKind::Other),
                        link_target: None,
                    });
                }
                Event::Rule => projection
                    .objects
                    .push(ProjectedObject::Rule { source: range }),
                Event::TaskListMarker(checked) => {
                    projection.objects.push(ProjectedObject::TaskCheckbox {
                        source: range,
                        checked,
                    });
                }
                Event::InlineMath(text) | Event::DisplayMath(text) => {
                    projection.text.push(ProjectedText {
                        source: range,
                        text: text.to_string(),
                        style: styles.style(),
                        block: *block_stack.last().unwrap_or(&BlockKind::Other),
                        link_target: None,
                    });
                }
                Event::FootnoteReference(text) => {
                    projection.text.push(ProjectedText {
                        source: range,
                        text: text.to_string(),
                        style: styles.style(),
                        block: *block_stack.last().unwrap_or(&BlockKind::Other),
                        link_target: None,
                    });
                }
            }
        }
        projection
            .blocks
            .sort_by_key(|block| (block.source.start, block.source.end));
        projection
            .text
            .sort_by_key(|span| (span.source.start, span.source.end));
        projection.objects.extend(
            projection
                .blocks
                .iter()
                .filter(|block| block.kind == BlockKind::ListItem)
                .filter_map(|block| list_marker(source, &block.source)),
        );
        projection
    }

    pub(crate) fn active_syntax(&self, byte: usize) -> Option<&MarkdownSyntaxNode> {
        self.syntax
            .iter()
            .filter(|node| node.source.start <= byte && byte <= node.source.end)
            .min_by_key(|node| node.source.end.saturating_sub(node.source.start))
    }

    pub(crate) fn visible_source_ranges(
        &self,
        source: &str,
        caret: usize,
        source_mode: bool,
    ) -> Vec<Range<usize>> {
        if source_mode {
            return vec![0..source.len()];
        }
        let mut ranges: Vec<Range<usize>> =
            self.text.iter().map(|span| span.source.clone()).collect();
        ranges.extend(self.objects.iter().map(ProjectedObject::source));
        if let Some(active) = self.active_syntax(caret) {
            ranges.push(active.source.clone());
        }
        for (idx, ch) in source.char_indices() {
            if ch == '\n' {
                ranges.push(idx..idx + 1);
            }
        }
        merge_ranges(ranges, source.len())
    }
}

fn list_marker(source: &str, item: &Range<usize>) -> Option<ProjectedObject> {
    let start = item.start.min(source.len());
    let end = item.end.min(source.len()).max(start);
    let line_end = source[start..end]
        .find('\n')
        .map(|offset| start + offset)
        .unwrap_or(end);
    let line = &source[start..line_end];
    let indent = line.len().saturating_sub(line.trim_start().len());
    let marker_start = start + indent;
    let rest = &line[indent..];
    if rest.starts_with("- [") || rest.starts_with("* [") || rest.starts_with("+ [") {
        return None;
    }
    if rest.starts_with("- ") || rest.starts_with("* ") || rest.starts_with("+ ") {
        return Some(ProjectedObject::ListMarker {
            source: marker_start..marker_start + 2,
            text: "• ".to_string(),
        });
    }
    let digits = rest.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if digits > 0 {
        let suffix = rest.as_bytes().get(digits..digits + 2)?;
        if suffix == b". " || suffix == b") " {
            return Some(ProjectedObject::ListMarker {
                source: marker_start..marker_start + digits + 2,
                text: rest[..digits + 2].to_string(),
            });
        }
    }
    None
}

fn content_range(source: &str, range: Range<usize>, rendered: &str) -> Range<usize> {
    let start = range.start.min(source.len());
    let end = range.end.min(source.len()).max(start);
    let slice = &source[start..end];
    if rendered.is_empty() {
        return start..start;
    }
    if let Some(relative) = slice.find(rendered) {
        return start + relative..start + relative + rendered.len();
    }
    // Escapes and normalized parser text may not be a literal source slice.
    // Keeping the whole event range visible is lossless and safer than hiding
    // characters that the user then cannot reach.
    start..end
}

fn table_cell_content_range(source: &str, range: Range<usize>, rendered: &str) -> Range<usize> {
    if !rendered.is_empty() {
        return content_range(source, range, rendered);
    }
    let start = range.start.min(source.len());
    let end = range.end.min(source.len()).max(start);
    let insertion = source[start..end]
        .char_indices()
        .find(|(_, ch)| !ch.is_whitespace())
        .map(|(relative, _)| start + relative)
        .unwrap_or(end);
    insertion..insertion
}

fn empty_code_block_content_range(source: &str, range: &Range<usize>) -> Range<usize> {
    let start = range.start.min(source.len());
    let end = range.end.min(source.len()).max(start);
    let slice = &source[start..end];
    let content_start = slice
        .find('\n')
        .map(|relative| start + relative + 1)
        .unwrap_or(start);
    content_start..content_start
}

fn merge_ranges(mut ranges: Vec<Range<usize>>, source_len: usize) -> Vec<Range<usize>> {
    ranges.retain(|range| range.start < range.end && range.start < source_len);
    for range in &mut ranges {
        range.end = range.end.min(source_len);
    }
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut merged: Vec<Range<usize>> = vec![];
    for range in ranges {
        if let Some(last) = merged.last_mut() {
            if range.start <= last.end {
                last.end = last.end.max(range.end);
                continue;
            }
        }
        merged.push(range);
    }
    merged
}

fn syntax_kind_for_tag(tag: &Tag<'_>) -> Option<MarkdownSyntaxKind> {
    Some(match tag {
        Tag::Heading { .. } => MarkdownSyntaxKind::Heading,
        Tag::Emphasis => MarkdownSyntaxKind::Emphasis,
        Tag::Strong => MarkdownSyntaxKind::Strong,
        Tag::Strikethrough => MarkdownSyntaxKind::Strikethrough,
        Tag::Link { .. } => MarkdownSyntaxKind::Link,
        Tag::Image { .. } => MarkdownSyntaxKind::Image,
        Tag::List(_) => MarkdownSyntaxKind::List,
        Tag::Item => MarkdownSyntaxKind::ListItem,
        Tag::BlockQuote(_) => MarkdownSyntaxKind::Quote,
        Tag::CodeBlock(_) => MarkdownSyntaxKind::CodeBlock,
        Tag::Table(_) => MarkdownSyntaxKind::Table,
        _ => return None,
    })
}

fn block_kind_for_tag(tag: &Tag<'_>) -> Option<BlockKind> {
    Some(match tag {
        Tag::Paragraph => BlockKind::Paragraph,
        Tag::Heading { level, .. } => BlockKind::Heading(*level as u8),
        Tag::BlockQuote(_) => BlockKind::Quote,
        Tag::List(_) => BlockKind::List,
        Tag::Item => BlockKind::ListItem,
        Tag::CodeBlock(_) => BlockKind::CodeBlock,
        Tag::Table(_) => BlockKind::Table,
        _ => return None,
    })
}

fn block_kind_for_end(tag: TagEnd) -> Option<BlockKind> {
    Some(match tag {
        TagEnd::Paragraph => BlockKind::Paragraph,
        TagEnd::Heading(level) => BlockKind::Heading(level as u8),
        TagEnd::BlockQuote(_) => BlockKind::Quote,
        TagEnd::List(_) => BlockKind::List,
        TagEnd::Item => BlockKind::ListItem,
        TagEnd::CodeBlock => BlockKind::CodeBlock,
        TagEnd::Table => BlockKind::Table,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_core_gfm_structures_and_source_ranges() {
        let source = "# Title\n\nA **bold** [link](https://example.com).\n\n- [x] done\n";
        let projection = MarkdownProjection::parse(source);
        assert!(projection
            .blocks
            .iter()
            .any(|block| block.kind == BlockKind::Heading(1)));
        assert!(projection
            .text
            .iter()
            .any(|span| span.text == "bold" && span.style.strong));
        assert!(projection
            .text
            .iter()
            .any(|span| span.text == "link" && span.style.link));
        assert!(projection
            .objects
            .iter()
            .any(|object| matches!(object, ProjectedObject::TaskCheckbox { checked: true, .. })));
        assert!(projection.objects.iter().any(|object| matches!(
            object,
            ProjectedObject::TaskCheckbox { source: marker, .. }
                if &source[marker.clone()] == "[x]"
        )));
        assert!(!projection
            .objects
            .iter()
            .any(|object| matches!(object, ProjectedObject::ListMarker { .. })));
        for span in &projection.text {
            assert!(span.source.end <= source.len());
            assert!(source.is_char_boundary(span.source.start));
            assert!(source.is_char_boundary(span.source.end));
        }
    }

    #[test]
    fn projects_unordered_and_ordered_list_markers() {
        let source = "- one\n2. two\n";
        let projection = MarkdownProjection::parse(source);
        assert!(projection.objects.iter().any(|object| matches!(
            object,
            ProjectedObject::ListMarker { text, .. } if text == "• "
        )));
        assert!(projection.objects.iter().any(|object| matches!(
            object,
            ProjectedObject::ListMarker { text, .. } if text == "2. "
        )));
    }

    #[test]
    fn captures_table_cells_and_image() {
        let source =
            "| A | B | C |\n| :-- | :-: | --: |\n| 1 | 2 | 3 |\n\n![Alt](attachments/a.png)\n";
        let projection = MarkdownProjection::parse(source);
        let table = projection.objects.iter().find_map(|object| match object {
            ProjectedObject::Table(table) => Some(table),
            _ => None,
        });
        let table = table.expect("table projection");
        assert_eq!(table.rows.len(), 2);
        assert_eq!(table.rows[0][0].text, "A");
        assert_eq!(&source[table.rows[0][0].source.clone()], "A");
        assert_eq!(&source[table.rows[1][2].source.clone()], "3");
        assert_eq!(
            table.alignments,
            vec![
                TableAlignment::Left,
                TableAlignment::Center,
                TableAlignment::Right
            ]
        );
        assert!(projection.objects.iter().any(|object| matches!(
            object,
            ProjectedObject::Image { target, alt, .. }
                if target == "attachments/a.png" && alt == "Alt"
        )));
    }

    #[test]
    fn captures_fenced_code_language_and_exact_content() {
        let source =
            "before\n\n```python linenos\ndef greet(name):\n    return name\n```\n\nafter\n";
        let projection = MarkdownProjection::parse(source);
        let code = projection.objects.iter().find_map(|object| match object {
            ProjectedObject::CodeBlock(code) => Some(code),
            _ => None,
        });
        let code = code.expect("fenced code projection");
        assert_eq!(code.language.as_deref(), Some("python"));
        assert_eq!(code.text, "def greet(name):\n    return name\n");
        assert_eq!(&source[code.content.clone()], code.text);
        assert!(source[code.source.clone()].starts_with("```python"));
        assert!(source[code.source.clone()].trim_end().ends_with("```"));
    }

    #[test]
    fn live_preview_reveals_smallest_active_syntax() {
        let source = "A **bold** word";
        let projection = MarkdownProjection::parse(source);
        let caret = source.find("bold").unwrap();
        let active = projection.active_syntax(caret).unwrap();
        assert_eq!(active.kind, MarkdownSyntaxKind::Strong);
        let visible = projection.visible_source_ranges(source, caret, false);
        assert!(visible
            .iter()
            .any(|range| range.start <= source.find("**").unwrap() && range.end >= caret));
    }
}
