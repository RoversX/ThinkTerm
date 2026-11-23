//! Markdown, as agents write it, cut into blocks a phone draws with its own
//! text views: paragraphs, headings, list items, quotes, code, rules, and
//! tables (as code, their columns lined up). Within a block, runs of text
//! that are bold, italic, code, struck through or a link.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Block {
    Para { spans: Vec<Span> },
    Heading { level: u8, spans: Vec<Span> },
    /// A list item: how deep, and its mark ("•", "3.", "☐").
    Item { depth: u8, mark: String, spans: Vec<Span> },
    Quote { spans: Vec<Span> },
    Code { lang: String, text: String },
    Rule,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Span {
    pub text: String,
    #[serde(skip_serializing_if = "is_false")]
    pub bold: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub italic: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub code: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub strike: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Widest a table is drawn, in characters, before its cells are cut.
const TABLE_CELL: usize = 40;

#[derive(Default)]
struct Style {
    bold: u32,
    italic: u32,
    strike: u32,
    link: Vec<String>,
}

enum Open {
    Para,
    Heading(u8),
    Item { depth: u8, mark: String },
    Quote,
}

pub fn blocks(markdown: &str) -> Vec<Block> {
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut out = Vec::new();
    let mut style = Style::default();
    let mut spans: Vec<Span> = Vec::new();
    let mut open: Option<Open> = None;
    // Each list being read: the next number, for an ordered one.
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut quotes = 0u32;
    let mut code: Option<(String, String)> = None;
    let mut table: Option<Vec<Vec<String>>> = None;
    let mut cell = String::new();

    fn flush(out: &mut Vec<Block>, open: &mut Option<Open>, spans: &mut Vec<Span>, quotes: u32) {
        let taken = std::mem::take(spans);
        let kind = open.take();
        if taken.iter().all(|s| s.text.trim().is_empty()) {
            return;
        }
        out.push(match kind {
            Some(Open::Heading(level)) => Block::Heading { level, spans: taken },
            Some(Open::Item { depth, mark }) => Block::Item { depth, mark, spans: taken },
            _ if quotes > 0 => Block::Quote { spans: taken },
            Some(Open::Quote) => Block::Quote { spans: taken },
            _ => Block::Para { spans: taken },
        });
    }

    let push = |spans: &mut Vec<Span>, style: &Style, text: &str, code: bool| {
        if text.is_empty() {
            return;
        }
        let span = Span {
            text: text.to_string(),
            bold: style.bold > 0,
            italic: style.italic > 0,
            code,
            strike: style.strike > 0,
            link: style.link.last().cloned(),
        };
        match spans.last_mut() {
            Some(last)
                if !code
                    && !last.code
                    && last.bold == span.bold
                    && last.italic == span.italic
                    && last.strike == span.strike
                    && last.link == span.link =>
            {
                last.text.push_str(text)
            }
            _ => spans.push(span),
        }
    };

    for event in Parser::new_ext(markdown, options) {
        if let Some((_, text)) = code.as_mut() {
            match event {
                Event::Text(t) => text.push_str(&t),
                Event::End(TagEnd::CodeBlock) => {
                    let (lang, text) = code.take().unwrap();
                    out.push(Block::Code {
                        lang,
                        text: text.trim_end_matches('\n').to_string(),
                    });
                }
                _ => {}
            }
            continue;
        }
        if let Some(rows) = table.as_mut() {
            match event {
                Event::Start(Tag::TableHead) | Event::Start(Tag::TableRow) => rows.push(Vec::new()),
                Event::End(TagEnd::TableCell) => {
                    if let Some(row) = rows.last_mut() {
                        row.push(std::mem::take(&mut cell).trim().to_string());
                    }
                }
                Event::Text(t) | Event::Code(t) => cell.push_str(&t),
                Event::SoftBreak | Event::HardBreak => cell.push(' '),
                Event::End(TagEnd::Table) => {
                    let rows = table.take().unwrap();
                    out.push(Block::Code {
                        lang: String::new(),
                        text: lay_out(&rows),
                    });
                }
                _ => {}
            }
            continue;
        }
        match event {
            Event::Start(Tag::Paragraph) => {
                if !matches!(open, Some(Open::Item { .. })) {
                    flush(&mut out, &mut open, &mut spans, quotes);
                    open = Some(Open::Para);
                } else if !spans.is_empty() {
                    // A loose item's later paragraph: on a line of its own.
                    push(&mut spans, &style, "\n", false);
                }
            }
            Event::End(TagEnd::Paragraph) => {
                if !matches!(open, Some(Open::Item { .. })) {
                    flush(&mut out, &mut open, &mut spans, quotes);
                }
            }
            Event::Start(Tag::Heading { level, .. }) => {
                flush(&mut out, &mut open, &mut spans, quotes);
                open = Some(Open::Heading(heading(level)));
            }
            Event::End(TagEnd::Heading(_)) => flush(&mut out, &mut open, &mut spans, quotes),
            Event::Start(Tag::BlockQuote(_)) => {
                flush(&mut out, &mut open, &mut spans, quotes);
                quotes += 1;
                open = Some(Open::Quote);
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                flush(&mut out, &mut open, &mut spans, quotes);
                quotes = quotes.saturating_sub(1);
            }
            Event::Start(Tag::List(first)) => {
                // The item holding this list goes first.
                flush(&mut out, &mut open, &mut spans, quotes);
                lists.push(first);
            }
            Event::End(TagEnd::List(_)) => {
                flush(&mut out, &mut open, &mut spans, quotes);
                lists.pop();
            }
            Event::Start(Tag::Item) => {
                flush(&mut out, &mut open, &mut spans, quotes);
                let depth = lists.len().saturating_sub(1) as u8;
                let mark = match lists.last_mut() {
                    Some(Some(n)) => {
                        let mark = format!("{n}.");
                        *n += 1;
                        mark
                    }
                    _ => "•".to_string(),
                };
                open = Some(Open::Item { depth, mark });
            }
            Event::End(TagEnd::Item) => flush(&mut out, &mut open, &mut spans, quotes),
            Event::TaskListMarker(done) => {
                if let Some(Open::Item { mark, .. }) = open.as_mut() {
                    *mark = if done { "☑".into() } else { "☐".into() };
                }
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                flush(&mut out, &mut open, &mut spans, quotes);
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => info.split_whitespace().next().unwrap_or("").to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                code = Some((lang, String::new()));
            }
            Event::Start(Tag::Table(_)) => {
                flush(&mut out, &mut open, &mut spans, quotes);
                table = Some(Vec::new());
            }
            Event::Start(Tag::Emphasis) => style.italic += 1,
            Event::End(TagEnd::Emphasis) => style.italic = style.italic.saturating_sub(1),
            Event::Start(Tag::Strong) => style.bold += 1,
            Event::End(TagEnd::Strong) => style.bold = style.bold.saturating_sub(1),
            Event::Start(Tag::Strikethrough) => style.strike += 1,
            Event::End(TagEnd::Strikethrough) => style.strike = style.strike.saturating_sub(1),
            Event::Start(Tag::Link { dest_url, .. }) => style.link.push(dest_url.to_string()),
            Event::End(TagEnd::Link) => {
                style.link.pop();
            }
            Event::Text(t) => {
                if open.is_none() {
                    open = Some(Open::Para);
                }
                push(&mut spans, &style, &t, false);
            }
            Event::Code(t) => {
                if open.is_none() {
                    open = Some(Open::Para);
                }
                push(&mut spans, &style, &t, true);
            }
            Event::InlineMath(t) | Event::DisplayMath(t) => push(&mut spans, &style, &t, true),
            Event::Html(t) | Event::InlineHtml(t) => push(&mut spans, &style, &t, false),
            Event::SoftBreak => push(&mut spans, &style, " ", false),
            Event::HardBreak => push(&mut spans, &style, "\n", false),
            Event::Rule => {
                flush(&mut out, &mut open, &mut spans, quotes);
                out.push(Block::Rule);
            }
            _ => {}
        }
    }
    flush(&mut out, &mut open, &mut spans, quotes);
    out
}

fn heading(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// A table's rows as lines of text, each column as wide as its widest
/// cell (counted in characters, a wide one as two).
fn lay_out(rows: &[Vec<String>]) -> String {
    let cut = |cell: &str| -> String {
        let mut out = String::new();
        let mut width = 0;
        for c in cell.chars() {
            let w = char_width(c);
            if width + w > TABLE_CELL {
                out.push('…');
                break;
            }
            out.push(c);
            width += w;
        }
        out
    };
    let rows: Vec<Vec<String>> = rows.iter().map(|row| row.iter().map(|c| cut(c)).collect()).collect();
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..columns)
        .map(|i| {
            rows.iter()
                .filter_map(|row| row.get(i))
                .map(|c| text_width(c))
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut lines = Vec::new();
    for (n, row) in rows.iter().enumerate() {
        let line: Vec<String> = (0..columns)
            .map(|i| {
                let cell = row.get(i).map(String::as_str).unwrap_or("");
                format!("{cell}{}", " ".repeat(widths[i].saturating_sub(text_width(cell))))
            })
            .collect();
        lines.push(line.join("  ").trim_end().to_string());
        if n == 0 && rows.len() > 1 {
            lines.push(widths.iter().map(|w| "─".repeat(*w)).collect::<Vec<_>>().join("  "));
        }
    }
    lines.join("\n")
}

fn text_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// Two columns for the wide scripts a monospaced phone font draws wide.
fn char_width(c: char) -> usize {
    match c as u32 {
        0x1100..=0x115F | 0x2E80..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF | 0xFE30..=0xFE4F | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6 | 0x1F300..=0x1F64F | 0x1F900..=0x1F9FF | 0x20000..=0x3FFFD => 2,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(spans: &[Span]) -> String {
        spans.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn paragraphs_headings_and_inline_styles() {
        let got = blocks("# Title\n\nSome **bold** and `code` and [a link](https://x.y).\nSame paragraph.");
        assert_eq!(got.len(), 2);
        assert!(matches!(&got[0], Block::Heading { level: 1, spans } if plain(spans) == "Title"));
        let Block::Para { spans } = &got[1] else { panic!("{got:?}") };
        assert_eq!(plain(spans), "Some bold and code and a link. Same paragraph.");
        assert!(spans.iter().any(|s| s.bold && s.text == "bold"));
        assert!(spans.iter().any(|s| s.code && s.text == "code"));
        assert!(spans.iter().any(|s| s.link.as_deref() == Some("https://x.y")));
    }

    #[test]
    fn lists_nest_and_number() {
        let got = blocks("1. first\n2. second\n   - inner\n3. third\n\n- [x] done\n- [ ] todo");
        let items: Vec<(u8, String, String)> = got
            .iter()
            .map(|b| match b {
                Block::Item { depth, mark, spans } => (*depth, mark.clone(), plain(spans)),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            items,
            [
                (0, "1.".into(), "first".into()),
                (0, "2.".into(), "second".into()),
                (1, "•".into(), "inner".into()),
                (0, "3.".into(), "third".into()),
                (0, "☑".into(), "done".into()),
                (0, "☐".into(), "todo".into()),
            ]
        );
    }

    #[test]
    fn code_quotes_rules_and_tables() {
        let got = blocks("```rust\nfn main() {}\n```\n\n> quoted\n\n---\n\n| a | bb |\n|---|---|\n| 汉 | x |\n");
        assert_eq!(
            got[0],
            Block::Code {
                lang: "rust".into(),
                text: "fn main() {}".into()
            }
        );
        assert!(matches!(&got[1], Block::Quote { spans } if plain(spans) == "quoted"));
        assert_eq!(got[2], Block::Rule);
        let Block::Code { text, .. } = &got[3] else { panic!("{got:?}") };
        assert_eq!(text, "a   bb\n──  ──\n汉  x");
    }
}
