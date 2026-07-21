use super::{BlockKind, MarkdownProjection, ProjectedObject};
use std::collections::HashSet;
use std::ops::Range;

const MAX_CHECK_CHUNK_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct SpellCheckSegment {
    pub check: Range<usize>,
    pub source: Range<usize>,
}

#[derive(Debug, Clone)]
pub(crate) struct SpellCheckChunk {
    pub text: String,
    pub segments: Vec<SpellCheckSegment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NoteSpellingIssue {
    pub source: Range<usize>,
    pub word: String,
    pub suggestions: Vec<String>,
}

impl SpellCheckChunk {
    pub(crate) fn source_range_for_issue(&self, issue: Range<usize>) -> Option<Range<usize>> {
        let segment = self
            .segments
            .iter()
            .find(|segment| issue.start >= segment.check.start && issue.end <= segment.check.end)?;
        let start = segment.source.start + issue.start.saturating_sub(segment.check.start);
        let end = segment.source.start + issue.end.saturating_sub(segment.check.start);
        Some(start..end)
    }
}

#[cfg(test)]
pub(crate) fn build_spell_check_chunks(
    source: &str,
    projection: &MarkdownProjection,
) -> Vec<SpellCheckChunk> {
    build_spell_check_chunks_in_range(source, projection, 0..source.len())
}

pub(crate) fn build_spell_check_chunks_in_range(
    source: &str,
    projection: &MarkdownProjection,
    check_range: Range<usize>,
) -> Vec<SpellCheckChunk> {
    let front_matter = front_matter_range(source);
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();

    let text_start = projection
        .text
        .partition_point(|text| text.source.end <= check_range.start);
    for text in projection.text[text_start..]
        .iter()
        .take_while(|text| text.source.start < check_range.end)
    {
        if text.style.code || matches!(text.block, BlockKind::CodeBlock) {
            continue;
        }
        let mut source_range = text.source.clone();
        let mut projected = text.text.as_str();
        if source_range.start < check_range.start || source_range.end > check_range.end {
            if source_range.end.saturating_sub(source_range.start) != projected.len() {
                continue;
            }
            let clipped_start = source_range.start.max(check_range.start);
            let clipped_end = source_range.end.min(check_range.end);
            let relative_start = clipped_start - source_range.start;
            let relative_end = clipped_end - source_range.start;
            let Some(clipped) = projected.get(relative_start..relative_end) else {
                continue;
            };
            source_range = clipped_start..clipped_end;
            projected = clipped;
        }
        push_candidate(
            source,
            source_range,
            projected,
            front_matter.as_ref(),
            &mut seen,
            &mut candidates,
        );
    }

    for object in &projection.objects {
        if !ranges_intersect(&object.source(), &check_range) {
            continue;
        }
        if let ProjectedObject::Table(table) = object {
            for cell in table.rows.iter().flatten() {
                push_candidate(
                    source,
                    cell.source.clone(),
                    &cell.text,
                    front_matter.as_ref(),
                    &mut seen,
                    &mut candidates,
                );
            }
        }
    }

    candidates.sort_by_key(|(range, _)| range.start);
    let mut chunks = Vec::new();
    let mut current = SpellCheckChunk {
        text: String::new(),
        segments: Vec::new(),
    };
    for (source_range, text) in candidates {
        let separator = usize::from(!current.text.is_empty());
        if !current.text.is_empty()
            && current.text.len() + separator + text.len() > MAX_CHECK_CHUNK_BYTES
        {
            chunks.push(current);
            current = SpellCheckChunk {
                text: String::new(),
                segments: Vec::new(),
            };
        }
        if !current.text.is_empty() {
            current.text.push('\n');
        }
        let start = current.text.len();
        current.text.push_str(&text);
        current.segments.push(SpellCheckSegment {
            check: start..start + text.len(),
            source: source_range,
        });
    }
    if !current.text.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn push_candidate(
    source: &str,
    range: Range<usize>,
    projected: &str,
    front_matter: Option<&Range<usize>>,
    seen: &mut HashSet<(usize, usize)>,
    candidates: &mut Vec<(Range<usize>, String)>,
) {
    if range.is_empty()
        || range.end > source.len()
        || front_matter.is_some_and(|front| ranges_intersect(front, &range))
    {
        return;
    }
    let original = &source[range.clone()];
    if original != projected {
        return;
    }
    let excluded = excluded_token_ranges(projected);
    if !excluded.is_empty() {
        let mut kept_start = 0usize;
        for excluded in excluded {
            push_plain_candidate(
                range.start + kept_start..range.start + excluded.start,
                &projected[kept_start..excluded.start],
                seen,
                candidates,
            );
            kept_start = excluded.end;
        }
        push_plain_candidate(
            range.start + kept_start..range.end,
            &projected[kept_start..],
            seen,
            candidates,
        );
        return;
    }
    push_plain_candidate(range, projected, seen, candidates);
}

fn push_plain_candidate(
    range: Range<usize>,
    projected: &str,
    seen: &mut HashSet<(usize, usize)>,
    candidates: &mut Vec<(Range<usize>, String)>,
) {
    let trimmed = projected.trim();
    if trimmed.is_empty() || trimmed.starts_with('<') || !seen.insert((range.start, range.end)) {
        return;
    }
    candidates.push((range, projected.to_string()));
}

fn excluded_token_ranges(text: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut search_from = 0usize;
    for token in text.split_whitespace() {
        let Some(relative) = text[search_from..].find(token) else {
            continue;
        };
        let start = search_from + relative;
        let end = start + token.len();
        let normalized = token.trim_matches(|ch: char| {
            matches!(ch, '(' | ')' | '[' | ']' | '<' | '>' | ',' | '.' | ';')
        });
        if normalized.starts_with("http://")
            || normalized.starts_with("https://")
            || normalized.starts_with("www.")
            || normalized.contains('@')
        {
            ranges.push(start..end);
        }
        search_from = end;
    }
    ranges
}

fn ranges_intersect(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start < b.end && b.start < a.end
}

fn front_matter_range(source: &str) -> Option<Range<usize>> {
    super::projection::frontmatter_range(source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excludes_code_urls_and_front_matter() {
        let source = "---\ntitle: Helo\n---\n\nProse mistke https://example.com `codde`\n\n```rs\nlet mistke = 1;\n```";
        let projection = MarkdownProjection::parse(source);
        let text = build_spell_check_chunks(source, &projection)
            .into_iter()
            .map(|chunk| chunk.text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Prose mistke"));
        assert!(!text.contains("Helo"));
        assert!(!text.contains("https://example.com"));
        assert!(!text.contains("codde"));
        assert!(!text.contains("let mistke"));
    }

    #[test]
    fn unclosed_frontmatter_does_not_swallow_prose_before_a_dash_line() {
        // The opening `---` is never closed by an exact `---` line, so this is
        // not frontmatter; every prose line must still be spellchecked.
        let source = "---\ntitlee: draft\n--- not a closing fence\nProse eror here.\n";
        let projection = MarkdownProjection::parse(source);
        let text = build_spell_check_chunks(source, &projection)
            .into_iter()
            .map(|chunk| chunk.text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("titlee"));
        assert!(text.contains("Prose eror"));
    }

    #[test]
    fn crlf_frontmatter_is_excluded_from_spellcheck() {
        let source = "---\r\ntitle: Helo\r\n---\r\n\r\nProse mistke.\r\n";
        let projection = MarkdownProjection::parse(source);
        let text = build_spell_check_chunks(source, &projection)
            .into_iter()
            .map(|chunk| chunk.text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!text.contains("Helo"));
        assert!(text.contains("Prose mistke"));
    }

    #[test]
    fn maps_check_ranges_back_to_markdown_source() {
        let source = "A **mistke** here";
        let projection = MarkdownProjection::parse(source);
        let chunks = build_spell_check_chunks(source, &projection);
        let chunk = chunks
            .iter()
            .find(|chunk| chunk.text.contains("mistke"))
            .unwrap();
        let start = chunk.text.find("mistke").unwrap();
        assert_eq!(chunk.source_range_for_issue(start..start + 6), Some(4..10));
    }

    #[test]
    fn scoped_check_only_builds_chunks_for_the_active_region() {
        let source = "First mistke.\n\nSecond eror.\n";
        let projection = MarkdownProjection::parse(source);
        let second = source.find("Second").unwrap();
        let text = build_spell_check_chunks_in_range(source, &projection, second..source.len())
            .into_iter()
            .map(|chunk| chunk.text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!text.contains("First"));
        assert!(text.contains("Second eror"));
    }
}
