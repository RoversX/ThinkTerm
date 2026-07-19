use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum BlockKind {
    Paragraph,
    Heading(u8),
    Quote,
    List,
    ListItem,
    CodeBlock,
    Table,
    Properties,
    Callout,
    Embed,
    Other,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
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
    WikiLink,
    Frontmatter,
    Callout,
    BlockId,
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
pub(crate) struct ProjectedProperty {
    pub source: Range<usize>,
    pub key_source: Range<usize>,
    pub value_source: Range<usize>,
    pub key: String,
    pub value: String,
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
    Frontmatter {
        source: Range<usize>,
        fields: Vec<ProjectedProperty>,
    },
    WikiLink {
        source: Range<usize>,
        target: String,
        label: String,
        embed: bool,
        resolved_path: Option<String>,
        ambiguous_paths: Vec<String>,
        rendered_lines: Vec<String>,
    },
    Callout {
        source: Range<usize>,
        kind: String,
        title: String,
        body: Vec<(Range<usize>, String)>,
    },
    BlockId {
        source: Range<usize>,
        id: String,
    },
}

impl ProjectedObject {
    pub(crate) fn source(&self) -> Range<usize> {
        match self {
            Self::Rule { source }
            | Self::TaskCheckbox { source, .. }
            | Self::ListMarker { source, .. }
            | Self::Image { source, .. }
            | Self::Frontmatter { source, .. }
            | Self::WikiLink { source, .. }
            | Self::Callout { source, .. }
            | Self::BlockId { source, .. } => source.clone(),
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
        let options = Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_TABLES
            | Options::ENABLE_TASKLISTS
            | Options::ENABLE_FOOTNOTES;
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
        projection.apply_obsidian_extensions(source);
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

    fn apply_obsidian_extensions(&mut self, source: &str) {
        if let Some((range, fields)) = parse_frontmatter(source) {
            self.text
                .retain(|span| !ranges_overlap(&span.source, &range));
            self.objects
                .retain(|object| !ranges_overlap(&object.source(), &range));
            self.blocks
                .retain(|block| !ranges_overlap(&block.source, &range));
            self.syntax
                .retain(|node| !ranges_overlap(&node.source, &range));
            self.syntax.push(MarkdownSyntaxNode {
                kind: MarkdownSyntaxKind::Frontmatter,
                source: range.clone(),
            });
            self.objects.push(ProjectedObject::Frontmatter {
                source: range,
                fields,
            });
        }

        for (range, target, label, embed) in parse_wiki_links(source, &self.syntax) {
            self.syntax.push(MarkdownSyntaxNode {
                kind: MarkdownSyntaxKind::WikiLink,
                source: range.clone(),
            });
            self.objects.push(ProjectedObject::WikiLink {
                source: range,
                target,
                label,
                embed,
                resolved_path: None,
                ambiguous_paths: vec![],
                rendered_lines: vec![],
            });
        }
        for (range, kind, title, body) in parse_callouts(source, &self.syntax) {
            self.syntax.push(MarkdownSyntaxNode {
                kind: MarkdownSyntaxKind::Callout,
                source: range.clone(),
            });
            self.objects.push(ProjectedObject::Callout {
                source: range,
                kind,
                title,
                body,
            });
        }
        for (range, id) in parse_block_ids(source, &self.syntax) {
            self.syntax.push(MarkdownSyntaxNode {
                kind: MarkdownSyntaxKind::BlockId,
                source: range.clone(),
            });
            self.objects
                .push(ProjectedObject::BlockId { source: range, id });
        }
        self.text = project_tags(project_plain_urls(std::mem::take(&mut self.text)));
    }

    pub(crate) fn resolve_vault_links(&mut self, vault_root: &Path, current_note: &str) {
        if !self
            .objects
            .iter()
            .any(|object| matches!(object, ProjectedObject::WikiLink { .. }))
        {
            return;
        }
        let paths = match crate::markdown_editor::vault_file_paths(vault_root) {
            Ok(paths) => paths,
            Err(err) => {
                log::debug!("unable to index Vault links: {err:#}");
                return;
            }
        };
        let index = VaultLinkIndex::new(paths);
        let current_directory = Path::new(current_note)
            .parent()
            .unwrap_or_else(|| Path::new(""));
        for object in &mut self.objects {
            let ProjectedObject::WikiLink {
                target,
                embed,
                resolved_path,
                ambiguous_paths,
                rendered_lines,
                ..
            } = object
            else {
                continue;
            };
            let resolution = index.resolve(current_directory, target);
            *resolved_path = resolution.path;
            *ambiguous_paths = resolution.ambiguous_paths;
            rendered_lines.clear();
            if *embed {
                if let Some(path) = resolved_path.as_deref() {
                    let mut visiting = HashSet::new();
                    *rendered_lines =
                        render_embedded_note(vault_root, path, &index, 0, &mut visiting);
                }
            }
        }
    }

    pub(crate) fn active_syntax(&self, byte: usize) -> Option<&MarkdownSyntaxNode> {
        self.syntax
            .iter()
            .filter(|node| node.source.start <= byte && byte <= node.source.end)
            .min_by_key(|node| node.source.end.saturating_sub(node.source.start))
    }

    pub(crate) fn active_block_range(&self, byte: usize) -> Option<Range<usize>> {
        let end = self
            .blocks
            .partition_point(|block| block.source.start <= byte);
        self.blocks[..end]
            .iter()
            .rev()
            .take(64)
            .filter(|block| byte <= block.source.end)
            .min_by_key(|block| block.source.end.saturating_sub(block.source.start))
            .map(|block| block.source.clone())
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

fn ranges_overlap(left: &Range<usize>, right: &Range<usize>) -> bool {
    left.start < right.end && right.start < left.end
}

fn range_is_literal_code_or_frontmatter(
    range: &Range<usize>,
    syntax: &[MarkdownSyntaxNode],
) -> bool {
    syntax.iter().any(|node| {
        matches!(
            node.kind,
            MarkdownSyntaxKind::InlineCode
                | MarkdownSyntaxKind::CodeBlock
                | MarkdownSyntaxKind::Frontmatter
        ) && ranges_overlap(&node.source, range)
    })
}

fn parse_frontmatter(source: &str) -> Option<(Range<usize>, Vec<ProjectedProperty>)> {
    let opening_end = source
        .strip_prefix("---\n")
        .map(|_| 4)
        .or_else(|| source.strip_prefix("---\r\n").map(|_| 5))?;
    let mut cursor = opening_end;
    let mut closing_end = None;
    while cursor <= source.len() {
        let line_end = source[cursor..]
            .find('\n')
            .map(|offset| cursor + offset)
            .unwrap_or(source.len());
        if source[cursor..line_end].trim_end_matches('\r').trim() == "---" {
            closing_end = Some(if line_end < source.len() {
                line_end + 1
            } else {
                line_end
            });
            break;
        }
        if line_end == source.len() {
            break;
        }
        cursor = line_end + 1;
    }
    let closing_end = closing_end?;
    let closing_start = source[..closing_end].rfind("---").unwrap_or(closing_end);
    let body = &source[opening_end..closing_start];
    let mut fields: Vec<ProjectedProperty> = Vec::new();
    let mut source_offset = opening_end;
    for chunk in body.split_inclusive('\n') {
        let raw_line = chunk.trim_end_matches(['\r', '\n']);
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            source_offset += chunk.len();
            continue;
        }
        if raw_line.len() > line.len() {
            if let Some(item) = line.strip_prefix("- ") {
                if let Some(field) = fields.last_mut() {
                    if !field.value.is_empty() {
                        field.value.push_str(", ");
                    }
                    field.value.push_str(item.trim().trim_matches(['\'', '"']));
                    field.source.end = source_offset + raw_line.len();
                    field.value_source.end = field.source.end;
                }
                source_offset += chunk.len();
                continue;
            }
        }
        let Some((key, value)) = line.split_once(':') else {
            source_offset += chunk.len();
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            source_offset += chunk.len();
            continue;
        }
        let leading = raw_line.len().saturating_sub(raw_line.trim_start().len());
        let key_start = source_offset + leading;
        let colon = line.find(':').unwrap_or(key.len());
        let key_end = key_start + colon;
        let raw_value_start = key_end + 1;
        let value_leading = source[raw_value_start..source_offset + raw_line.len()]
            .len()
            .saturating_sub(
                source[raw_value_start..source_offset + raw_line.len()]
                    .trim_start()
                    .len(),
            );
        let value_start = raw_value_start + value_leading;
        fields.push(ProjectedProperty {
            source: key_start..source_offset + raw_line.len(),
            key_source: key_start..key_end,
            value_source: value_start..source_offset + raw_line.len(),
            key: key.to_string(),
            value: value.trim().trim_matches(['\'', '"']).to_string(),
        });
        source_offset += chunk.len();
    }
    Some((0..closing_end, fields))
}

fn parse_wiki_links(
    source: &str,
    syntax: &[MarkdownSyntaxNode],
) -> Vec<(Range<usize>, String, String, bool)> {
    let mut result = Vec::new();
    let mut cursor = 0usize;
    while let Some(relative_start) = source[cursor..].find("[[") {
        let brackets_start = cursor + relative_start;
        let start = brackets_start.saturating_sub(
            (brackets_start > 0 && source.as_bytes()[brackets_start - 1] == b'!') as usize,
        );
        let Some(relative_end) = source[brackets_start + 2..].find("]]") else {
            break;
        };
        let end = brackets_start + 2 + relative_end + 2;
        let range = start..end;
        cursor = end;
        if range_is_literal_code_or_frontmatter(&range, syntax) {
            continue;
        }
        let inner = source[brackets_start + 2..end - 2].trim();
        let (target, alias) = inner
            .split_once('|')
            .map(|(target, alias)| (target.trim(), Some(alias.trim())))
            .unwrap_or((inner, None));
        if target.is_empty() {
            continue;
        }
        let label = alias
            .filter(|alias| !alias.is_empty())
            .unwrap_or(target)
            .to_string();
        result.push((range, target.to_string(), label, start < brackets_start));
    }
    result
}

fn parse_callouts(
    source: &str,
    syntax: &[MarkdownSyntaxNode],
) -> Vec<(Range<usize>, String, String, Vec<(Range<usize>, String)>)> {
    let mut result = Vec::new();
    let mut lines = Vec::new();
    let mut line_start = 0;
    for line in source.split_inclusive('\n') {
        lines.push((line_start, line));
        line_start += line.len();
    }
    let mut index = 0usize;
    while index < lines.len() {
        let (line_start, line) = lines[index];
        let line_without_newline = line.trim_end_matches(['\r', '\n']);
        let Some(quote_offset) = line_without_newline.find('>') else {
            index += 1;
            continue;
        };
        if !line_without_newline[..quote_offset].trim().is_empty() {
            index += 1;
            continue;
        }
        let after_quote = &line_without_newline[quote_offset + 1..];
        let whitespace = after_quote.len() - after_quote.trim_start().len();
        let marker_start = line_start + quote_offset + 1 + whitespace;
        let marker = &line_without_newline[quote_offset + 1 + whitespace..];
        if !marker.starts_with("[!") {
            index += 1;
            continue;
        }
        let Some(close) = marker.find(']') else {
            index += 1;
            continue;
        };
        let kind = marker[2..close].trim();
        if kind.is_empty() {
            index += 1;
            continue;
        }
        let title = marker[close + 1..].trim_start_matches(['+', '-']).trim();
        let mut body = Vec::new();
        let mut next = index + 1;
        while let Some((body_start, body_line)) = lines.get(next).copied() {
            let body_without_newline = body_line.trim_end_matches(['\r', '\n']);
            let leading = body_without_newline.len() - body_without_newline.trim_start().len();
            let Some(after_quote) = body_without_newline[leading..].strip_prefix('>') else {
                break;
            };
            let whitespace = after_quote.len() - after_quote.trim_start().len();
            let text_start = body_start + leading + 1 + whitespace;
            let text = after_quote.trim_start();
            // A second callout header starts a new card even when the author
            // does not put a blank line between the two quote blocks.
            if text.starts_with("[!") {
                break;
            }
            body.push((
                text_start..body_start + body_without_newline.len(),
                clean_embedded_markdown_line(text),
            ));
            next += 1;
        }
        let block_end = lines
            .get(next.saturating_sub(1))
            .map(|(start, line)| start + line.len())
            .unwrap_or_else(|| line_start + line.len());
        let range = marker_start..block_end;
        if !range_is_literal_code_or_frontmatter(&range, syntax) {
            result.push((
                range,
                kind.to_ascii_uppercase(),
                if title.is_empty() {
                    kind.to_string()
                } else {
                    title.to_string()
                },
                body,
            ));
        }
        index = next.max(index + 1);
    }
    result
}

fn parse_block_ids(source: &str, syntax: &[MarkdownSyntaxNode]) -> Vec<(Range<usize>, String)> {
    let mut result = Vec::new();
    let mut line_start = 0usize;
    for line in source.split_inclusive('\n') {
        let content = line.trim_end_matches(['\r', '\n']);
        let trimmed = content.trim_end();
        let token_start = trimmed
            .rfind(char::is_whitespace)
            .map(|index| index + 1)
            .unwrap_or(0);
        let token = &trimmed[token_start..];
        if let Some(id) = token.strip_prefix('^').filter(|id| {
            !id.is_empty()
                && id
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        }) {
            let range = line_start + token_start..line_start + trimmed.len();
            if !range_is_literal_code_or_frontmatter(&range, syntax) {
                result.push((range, id.to_string()));
            }
        }
        line_start += line.len();
    }
    result
}

fn project_tags(spans: Vec<ProjectedText>) -> Vec<ProjectedText> {
    let mut projected = Vec::with_capacity(spans.len());
    for span in spans {
        if span.style.code || span.source.end.saturating_sub(span.source.start) != span.text.len() {
            projected.push(span);
            continue;
        }
        let bytes = span.text.as_bytes();
        let mut cursor = 0usize;
        let mut found = false;
        while cursor < bytes.len() {
            let Some(relative) = span.text[cursor..].find('#') else {
                break;
            };
            let start = cursor + relative;
            let boundary = start == 0
                || span.text[..start]
                    .chars()
                    .next_back()
                    .is_some_and(|ch| ch.is_whitespace() || matches!(ch, '(' | '[' | '{'));
            let mut end = start + 1;
            while end < bytes.len() {
                let ch = span.text[end..].chars().next().expect("tag char boundary");
                if ch.is_alphanumeric() || matches!(ch, '_' | '-' | '/') {
                    end += ch.len_utf8();
                } else {
                    break;
                }
            }
            if !boundary || end == start + 1 {
                cursor = start + 1;
                continue;
            }
            found = true;
            if cursor < start {
                projected.push(ProjectedText {
                    source: span.source.start + cursor..span.source.start + start,
                    text: span.text[cursor..start].to_string(),
                    ..span.clone()
                });
            }
            let mut style = span.style;
            style.link = true;
            projected.push(ProjectedText {
                source: span.source.start + start..span.source.start + end,
                text: span.text[start..end].to_string(),
                style,
                block: span.block,
                link_target: Some(span.text[start..end].to_string()),
            });
            cursor = end;
        }
        if found {
            if cursor < span.text.len() {
                projected.push(ProjectedText {
                    source: span.source.start + cursor..span.source.end,
                    text: span.text[cursor..].to_string(),
                    ..span
                });
            }
        } else {
            projected.push(span);
        }
    }
    projected
}

fn project_plain_urls(spans: Vec<ProjectedText>) -> Vec<ProjectedText> {
    let mut projected = Vec::with_capacity(spans.len());
    for span in spans {
        if span.style.code
            || span.link_target.is_some()
            || span.source.end.saturating_sub(span.source.start) != span.text.len()
        {
            projected.push(span);
            continue;
        }

        let mut cursor = 0usize;
        let mut emitted_url = false;
        while cursor < span.text.len() {
            let remaining = &span.text[cursor..];
            let Some(relative_start) = remaining.find("http") else {
                break;
            };
            let start = cursor + relative_start;
            if !span.text[start..].starts_with("http://")
                && !span.text[start..].starts_with("https://")
            {
                cursor = start + "http".len();
                continue;
            }
            let boundary_ok = start == 0
                || span.text[..start]
                    .chars()
                    .next_back()
                    .is_some_and(|ch| ch.is_whitespace() || matches!(ch, '(' | '[' | '{' | '<'));
            if !boundary_ok {
                cursor = start + 1;
                continue;
            }

            let mut end = span.text.len();
            for (relative, ch) in span.text[start..].char_indices() {
                if relative > 0
                    && (ch.is_whitespace() || matches!(ch, '<' | '>' | '"' | '\'' | '`'))
                {
                    end = start + relative;
                    break;
                }
            }
            while end > start {
                let Some(ch) = span.text[start..end].chars().next_back() else {
                    break;
                };
                if matches!(ch, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}') {
                    end -= ch.len_utf8();
                } else {
                    break;
                }
            }
            if end <= start
                || url::Url::parse(&span.text[start..end]).is_err()
                || !span.text[start..end]
                    .split_once(':')
                    .is_some_and(|(scheme, _)| matches!(scheme, "http" | "https"))
            {
                cursor = start + 1;
                continue;
            }

            emitted_url = true;
            if cursor < start {
                projected.push(ProjectedText {
                    source: span.source.start + cursor..span.source.start + start,
                    text: span.text[cursor..start].to_string(),
                    ..span.clone()
                });
            }
            let mut style = span.style;
            style.link = true;
            projected.push(ProjectedText {
                source: span.source.start + start..span.source.start + end,
                text: span.text[start..end].to_string(),
                style,
                block: span.block,
                link_target: Some(span.text[start..end].to_string()),
            });
            cursor = end;
        }

        if emitted_url {
            if cursor < span.text.len() {
                projected.push(ProjectedText {
                    source: span.source.start + cursor..span.source.end,
                    text: span.text[cursor..].to_string(),
                    ..span
                });
            }
        } else {
            projected.push(span);
        }
    }
    projected
}

struct VaultLinkIndex {
    exact: HashMap<String, String>,
    names: HashMap<String, Vec<String>>,
}

#[derive(Default)]
struct VaultLinkResolution {
    path: Option<String>,
    ambiguous_paths: Vec<String>,
}

impl VaultLinkIndex {
    fn new(paths: Vec<String>) -> Self {
        let mut exact = HashMap::new();
        let mut names: HashMap<String, Vec<String>> = HashMap::new();
        for path in &paths {
            exact.insert(path.to_ascii_lowercase(), path.clone());
            let is_markdown = Path::new(path)
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("md"));
            if is_markdown {
                // Only notes receive Obsidian's extension-less aliases. An
                // attachment must use its real filename so that `Plan.md` and
                // `Plan.png` cannot shadow each other.
                let without_extension = Path::new(path).with_extension("");
                exact.insert(
                    without_extension
                        .to_string_lossy()
                        .replace('\\', "/")
                        .to_ascii_lowercase(),
                    path.clone(),
                );
                let Some(stem) = Path::new(path).file_stem().and_then(|stem| stem.to_str()) else {
                    continue;
                };
                names
                    .entry(stem.to_ascii_lowercase())
                    .or_default()
                    .push(path.clone());
            }
        }
        Self { exact, names }
    }

    fn resolve(&self, current_directory: &Path, target: &str) -> VaultLinkResolution {
        let target = target
            .split(['#', '^'])
            .next()
            .unwrap_or(target)
            .trim()
            .replace('\\', "/");
        if target.is_empty() {
            return VaultLinkResolution::default();
        }
        let target_path = Path::new(&target);
        let local = current_directory
            .join(target_path)
            .to_string_lossy()
            .replace('\\', "/");
        for candidate in [local.as_str(), target.as_str()] {
            let key = candidate.to_ascii_lowercase();
            if let Some(path) = self.exact.get(&key) {
                return VaultLinkResolution {
                    path: Some(path.clone()),
                    ambiguous_paths: vec![],
                };
            }
            if !key.ends_with(".md") {
                if let Some(path) = self.exact.get(&format!("{key}.md")) {
                    return VaultLinkResolution {
                        path: Some(path.clone()),
                        ambiguous_paths: vec![],
                    };
                }
            }
        }
        let name = target_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or(&target)
            .to_ascii_lowercase();
        let Some(candidates) = self.names.get(&name) else {
            return VaultLinkResolution::default();
        };
        if candidates.len() == 1 {
            VaultLinkResolution {
                path: Some(candidates[0].clone()),
                ambiguous_paths: vec![],
            }
        } else {
            let mut ambiguous_paths = candidates.clone();
            ambiguous_paths.sort();
            VaultLinkResolution {
                path: None,
                ambiguous_paths,
            }
        }
    }
}

fn render_embedded_note(
    vault_root: &Path,
    relative_path: &str,
    index: &VaultLinkIndex,
    depth: usize,
    visiting: &mut HashSet<String>,
) -> Vec<String> {
    if Path::new(relative_path)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| !extension.eq_ignore_ascii_case("md"))
    {
        return vec![format!("Attachment: {relative_path}")];
    }
    if depth >= 3 {
        return vec!["…".to_string()];
    }
    if !visiting.insert(relative_path.to_ascii_lowercase()) {
        return vec![format!("↻ {relative_path} (cycle)")];
    }
    let result = std::fs::read_to_string(vault_root.join(relative_path))
        .map(|source| {
            let frontmatter = parse_frontmatter(&source).map(|(range, _)| range);
            let current_directory = Path::new(relative_path)
                .parent()
                .unwrap_or_else(|| Path::new(""));
            let mut lines = Vec::new();
            let mut source_offset = 0usize;
            let mut in_fence = false;
            for line in source.split_inclusive('\n') {
                if lines.len() >= 200 {
                    lines.push("…".to_string());
                    break;
                }
                let line_end = source_offset + line.len();
                if frontmatter
                    .as_ref()
                    .is_some_and(|range| source_offset < range.end && range.start < line_end)
                {
                    source_offset = line_end;
                    continue;
                }
                let trimmed = line.trim_end_matches(['\r', '\n']).trim();
                if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
                    in_fence = !in_fence;
                    source_offset = line_end;
                    continue;
                }
                if let Some(target) = trimmed
                    .strip_prefix("![[")
                    .and_then(|value| value.strip_suffix("]]"))
                {
                    if let Some(path) = index.resolve(current_directory, target).path {
                        lines.extend(render_embedded_note(
                            vault_root,
                            &path,
                            index,
                            depth + 1,
                            visiting,
                        ));
                    } else {
                        lines.push(format!("▧ {target}"));
                    }
                } else if in_fence {
                    lines.push(line.trim_end_matches(['\r', '\n']).to_string());
                } else {
                    lines.push(clean_embedded_markdown_line(trimmed));
                }
                source_offset = line_end;
            }
            while lines.last().is_some_and(|line| line.is_empty()) {
                lines.pop();
            }
            lines
        })
        .unwrap_or_else(|_| vec![format!("Missing embed: {relative_path}")]);
    visiting.remove(&relative_path.to_ascii_lowercase());
    result
}

fn clean_embedded_markdown_line(line: &str) -> String {
    let mut text = line.trim_start();
    if text.starts_with('#') {
        text = text.trim_start_matches('#').trim_start();
    }
    if text.starts_with('>') {
        text = text.trim_start_matches('>').trim_start();
    }
    if let Some(marker) = text.strip_prefix("[!") {
        if let Some(end) = marker.find(']') {
            let kind = &marker[..end];
            let title = marker[end + 1..].trim_start_matches(['+', '-']).trim();
            return if title.is_empty() {
                kind.to_ascii_uppercase()
            } else {
                format!("{}  {title}", kind.to_ascii_uppercase())
            };
        }
    }

    let mut output = String::with_capacity(text.len());
    let mut cursor = 0usize;
    while let Some(start_relative) = text[cursor..].find("[[") {
        let start = cursor + start_relative;
        output.push_str(&text[cursor..start]);
        let Some(end_relative) = text[start + 2..].find("]]") else {
            output.push_str(&text[start..]);
            cursor = text.len();
            break;
        };
        let end = start + 2 + end_relative;
        let inner = &text[start + 2..end];
        let label = inner
            .split_once('|')
            .map(|(_, alias)| alias)
            .unwrap_or(inner);
        output.push_str(label);
        cursor = end + 2;
    }
    if cursor < text.len() {
        output.push_str(&text[cursor..]);
    }
    for marker in ["**", "__", "~~", "`"] {
        output = output.replace(marker, "");
    }
    output
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
    fn projects_plain_http_urls_as_clickable_links_without_trailing_punctuation() {
        let source = "Open https://example.test/path/file_name.zip, then continue.";
        let projection = MarkdownProjection::parse(source);
        let link = projection
            .text
            .iter()
            .find(|span| span.link_target.is_some())
            .expect("plain URL should be projected as a link");

        assert_eq!(link.text, "https://example.test/path/file_name.zip");
        assert_eq!(link.link_target.as_deref(), Some(link.text.as_str()));
        assert!(link.style.link);
        assert_eq!(&source[link.source.clone()], link.text);
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

    #[test]
    fn captures_obsidian_properties_links_callouts_tags_and_block_ids() {
        let source = "---\ntags: [rust, notes]\nstatus: draft\n---\n\n[[Plan|Release plan]] and #shipping\n\n> [!warning] Check this\n> Body\n\nParagraph ^stable-id\n\n[^1]: footnote\nRef [^1]\n";
        let projection = MarkdownProjection::parse(source);
        assert!(projection.objects.iter().any(|object| matches!(
            object,
            ProjectedObject::Frontmatter { fields, .. }
                if fields.iter().any(|field| field.key == "status" && field.value == "draft")
        )));
        assert!(projection.objects.iter().any(|object| matches!(
            object,
            ProjectedObject::WikiLink { target, label, embed: false, .. }
                if target == "Plan" && label == "Release plan"
        )));
        assert!(projection.objects.iter().any(|object| matches!(
            object,
            ProjectedObject::Callout { kind, title, body, .. }
                if kind == "WARNING" && title == "Check this"
                    && body.iter().any(|(_, line)| line == "Body")
        )));
        assert!(projection.objects.iter().any(|object| matches!(
            object,
            ProjectedObject::BlockId { id, .. } if id == "stable-id"
        )));
        assert!(projection.text.iter().any(|span| {
            span.text == "#shipping"
                && span.style.link
                && span.link_target.as_deref() == Some("#shipping")
        }));
        assert!(projection
            .text
            .iter()
            .any(|span| span.text.contains("footnote") || span.text == "1"));
    }

    #[test]
    fn frontmatter_projects_indented_sequence_values() {
        let source = "---\ntitle: Example\ntags:\n  - clippings\n  - android\n---\n\nBody\n";
        let projection = MarkdownProjection::parse(source);
        assert!(projection.objects.iter().any(|object| matches!(
            object,
            ProjectedObject::Frontmatter { fields, .. }
                if fields.iter().any(|field| field.key == "tags" && field.value == "clippings, android")
        )));
    }

    #[test]
    fn resolves_and_renders_embeds_with_cycle_detection() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("A.md"), "# A\n\n![[B]]\n").unwrap();
        std::fs::write(temp.path().join("B.md"), "## B\n\n![[A]]\n").unwrap();
        let mut projection = MarkdownProjection::parse("![[A]]");
        projection.resolve_vault_links(temp.path(), "Home.md");
        let embed = projection.objects.iter().find_map(|object| match object {
            ProjectedObject::WikiLink {
                resolved_path,
                rendered_lines,
                ..
            } => Some((resolved_path, rendered_lines)),
            _ => None,
        });
        let (resolved, lines) = embed.expect("embed");
        assert_eq!(resolved.as_deref(), Some("A.md"));
        assert!(lines.iter().any(|line| line == "A"));
        assert!(lines.iter().any(|line| line.contains("cycle")));
    }

    #[test]
    fn resolves_attachment_embeds_by_explicit_filename() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("assets")).unwrap();
        std::fs::write(temp.path().join("assets/cover.png"), b"not decoded here").unwrap();
        std::fs::write(temp.path().join("cover.md"), "note").unwrap();

        let mut projection = MarkdownProjection::parse("![[assets/cover.png]]");
        projection.resolve_vault_links(temp.path(), "Home.md");
        let attachment = projection.objects.iter().find_map(|object| match object {
            ProjectedObject::WikiLink {
                resolved_path,
                rendered_lines,
                ..
            } => Some((resolved_path, rendered_lines)),
            _ => None,
        });
        let (resolved, lines) = attachment.expect("attachment embed");
        assert_eq!(resolved.as_deref(), Some("assets/cover.png"));
        assert_eq!(lines, &["Attachment: assets/cover.png".to_string()]);
    }

    #[test]
    fn adjacent_callouts_remain_separate_cards() {
        let projection = MarkdownProjection::parse(
            "> [!note] First\n> first body\n> [!warning] Second\n> second body\n",
        );
        let callouts = projection
            .objects
            .iter()
            .filter_map(|object| match object {
                ProjectedObject::Callout {
                    kind, title, body, ..
                } => Some((kind, title, body)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(callouts.len(), 2);
        assert_eq!(callouts[0].0, "NOTE");
        assert_eq!(callouts[0].2[0].1, "first body");
        assert_eq!(callouts[1].0, "WARNING");
        assert_eq!(callouts[1].2[0].1, "second body");
    }

    #[test]
    fn reports_ambiguous_wiki_links_instead_of_picking_or_creating_one() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("A")).unwrap();
        std::fs::create_dir_all(temp.path().join("B")).unwrap();
        std::fs::write(temp.path().join("A/Plan.md"), "A").unwrap();
        std::fs::write(temp.path().join("B/Plan.md"), "B").unwrap();
        let mut projection = MarkdownProjection::parse("[[Plan]]");
        projection.resolve_vault_links(temp.path(), "Home.md");
        let resolution = projection.objects.iter().find_map(|object| match object {
            ProjectedObject::WikiLink {
                resolved_path,
                ambiguous_paths,
                ..
            } => Some((resolved_path, ambiguous_paths)),
            _ => None,
        });
        let (resolved, ambiguous) = resolution.expect("wiki link");
        assert!(resolved.is_none());
        assert_eq!(
            ambiguous,
            &vec!["A/Plan.md".to_string(), "B/Plan.md".to_string()]
        );
    }
}
