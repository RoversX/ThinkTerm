//! Shared syntax detection, parsing, and highlighting for ThinkTerm.
//!
//! The GUI consumes semantic UTF-8 byte ranges from this crate and remains in
//! charge of mapping those semantic categories to theme colours.  Keeping the
//! engine independent from window/rendering types lets the same registry back
//! file preview, Markdown code blocks, and a future code editor.

use anyhow::{anyhow, Context, Result};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;
use std::sync::atomic::AtomicUsize;
use std::sync::OnceLock;
use tree_sitter::{InputEdit, Parser, Point, Tree};
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

/// Maximum source size that GUI consumers should submit for highlighting.
///
/// The engine itself accepts larger inputs so future editors can choose their
/// own policy, but the current Preview and Note integrations use this bound.
pub const DEFAULT_HIGHLIGHT_BYTE_LIMIT: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LanguageId {
    Bash,
    C,
    Cpp,
    CSharp,
    Css,
    Go,
    Html,
    Java,
    JavaScript,
    Jsx,
    Json,
    Lua,
    Markdown,
    Python,
    Rust,
    Sql,
    Toml,
    Tsx,
    TypeScript,
    Yaml,
}

impl LanguageId {
    pub const ALL: [Self; 20] = [
        Self::Bash,
        Self::C,
        Self::Cpp,
        Self::CSharp,
        Self::Css,
        Self::Go,
        Self::Html,
        Self::Java,
        Self::JavaScript,
        Self::Jsx,
        Self::Json,
        Self::Lua,
        Self::Markdown,
        Self::Python,
        Self::Rust,
        Self::Sql,
        Self::Toml,
        Self::Tsx,
        Self::TypeScript,
        Self::Yaml,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::CSharp => "c_sharp",
            Self::Css => "css",
            Self::Go => "go",
            Self::Html => "html",
            Self::Java => "java",
            Self::JavaScript => "javascript",
            Self::Jsx => "jsx",
            Self::Json => "json",
            Self::Lua => "lua",
            Self::Markdown => "markdown",
            Self::Python => "python",
            Self::Rust => "rust",
            Self::Sql => "sql",
            Self::Toml => "toml",
            Self::Tsx => "tsx",
            Self::TypeScript => "typescript",
            Self::Yaml => "yaml",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HighlightKind {
    Attribute,
    Comment,
    Constant,
    Constructor,
    Embedded,
    Function,
    Keyword,
    Label,
    Module,
    Number,
    Operator,
    Property,
    Punctuation,
    String,
    Tag,
    Type,
    Variable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighlightSpan {
    pub range: Range<usize>,
    pub kind: HighlightKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighlightResult {
    pub language: LanguageId,
    pub spans: Vec<HighlightSpan>,
}

const HIGHLIGHT_NAMES: [&str; 27] = [
    "attribute",
    "comment",
    "constant",
    "constant.builtin",
    "constructor",
    "embedded",
    "function",
    "function.builtin",
    "function.method",
    "function.method.builtin",
    "keyword",
    "label",
    "module",
    "number",
    "operator",
    "property",
    "punctuation",
    "punctuation.bracket",
    "punctuation.delimiter",
    "string",
    "string.documentation",
    "string.escape",
    "string.special",
    "tag",
    "type",
    "type.builtin",
    "variable",
];

const HIGHLIGHT_KINDS: [HighlightKind; HIGHLIGHT_NAMES.len()] = [
    HighlightKind::Attribute,
    HighlightKind::Comment,
    HighlightKind::Constant,
    HighlightKind::Constant,
    HighlightKind::Constructor,
    HighlightKind::Embedded,
    HighlightKind::Function,
    HighlightKind::Function,
    HighlightKind::Function,
    HighlightKind::Function,
    HighlightKind::Keyword,
    HighlightKind::Label,
    HighlightKind::Module,
    HighlightKind::Number,
    HighlightKind::Operator,
    HighlightKind::Property,
    HighlightKind::Punctuation,
    HighlightKind::Punctuation,
    HighlightKind::Punctuation,
    HighlightKind::String,
    HighlightKind::String,
    HighlightKind::String,
    HighlightKind::String,
    HighlightKind::Tag,
    HighlightKind::Type,
    HighlightKind::Type,
    HighlightKind::Variable,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ConfigId {
    Public(LanguageId),
    MarkdownInline,
}

struct Registry {
    configs: HashMap<ConfigId, HighlightConfiguration>,
    aliases: HashMap<&'static str, ConfigId>,
}

impl Registry {
    fn config(&self, id: LanguageId) -> Option<&HighlightConfiguration> {
        self.configs.get(&ConfigId::Public(id))
    }

    fn injection(&self, name: &str) -> Option<&HighlightConfiguration> {
        let normalized = normalize_token(name);
        self.aliases
            .get(normalized.as_str())
            .and_then(|id| self.configs.get(id))
    }
}

static REGISTRY: OnceLock<std::result::Result<Registry, String>> = OnceLock::new();

thread_local! {
    static HIGHLIGHTER: RefCell<Highlighter> = RefCell::new(Highlighter::new());
}

fn registry() -> Result<&'static Registry> {
    match REGISTRY.get_or_init(|| build_registry().map_err(|err| format!("{err:#}"))) {
        Ok(registry) => Ok(registry),
        Err(err) => Err(anyhow!(err.clone())),
    }
}

fn configured(
    language: tree_sitter::Language,
    name: &str,
    highlights: &str,
    injections: &str,
    locals: &str,
) -> Result<HighlightConfiguration> {
    let mut config = HighlightConfiguration::new(language, name, highlights, injections, locals)
        .with_context(|| format!("build Tree-sitter highlight query for {name}"))?;
    config.configure(&HIGHLIGHT_NAMES);
    Ok(config)
}

fn build_registry() -> Result<Registry> {
    let mut configs = HashMap::new();

    let javascript_highlights = format!(
        "{}\n{}",
        tree_sitter_javascript::HIGHLIGHT_QUERY,
        tree_sitter_javascript::JSX_HIGHLIGHT_QUERY
    );

    macro_rules! insert {
        ($id:expr, $language:expr, $name:expr, $highlights:expr) => {
            configs.insert(
                ConfigId::Public($id),
                configured(($language).into(), $name, $highlights, "", "")?,
            );
        };
        ($id:expr, $language:expr, $name:expr, $highlights:expr, $injections:expr, $locals:expr) => {
            configs.insert(
                ConfigId::Public($id),
                configured(($language).into(), $name, $highlights, $injections, $locals)?,
            );
        };
    }

    insert!(
        LanguageId::Bash,
        tree_sitter_bash::LANGUAGE,
        "bash",
        tree_sitter_bash::HIGHLIGHT_QUERY
    );
    insert!(
        LanguageId::C,
        tree_sitter_c::LANGUAGE,
        "c",
        tree_sitter_c::HIGHLIGHT_QUERY
    );
    insert!(
        LanguageId::Cpp,
        tree_sitter_cpp::LANGUAGE,
        "cpp",
        tree_sitter_cpp::HIGHLIGHT_QUERY
    );
    insert!(
        LanguageId::CSharp,
        tree_sitter_c_sharp::LANGUAGE,
        "c_sharp",
        tree_sitter_c_sharp::HIGHLIGHTS_QUERY
    );
    insert!(
        LanguageId::Css,
        tree_sitter_css::LANGUAGE,
        "css",
        tree_sitter_css::HIGHLIGHTS_QUERY
    );
    insert!(
        LanguageId::Go,
        tree_sitter_go::LANGUAGE,
        "go",
        tree_sitter_go::HIGHLIGHTS_QUERY
    );
    insert!(
        LanguageId::Html,
        tree_sitter_html::LANGUAGE,
        "html",
        tree_sitter_html::HIGHLIGHTS_QUERY,
        tree_sitter_html::INJECTIONS_QUERY,
        ""
    );
    insert!(
        LanguageId::Java,
        tree_sitter_java::LANGUAGE,
        "java",
        tree_sitter_java::HIGHLIGHTS_QUERY
    );
    insert!(
        LanguageId::JavaScript,
        tree_sitter_javascript::LANGUAGE,
        "javascript",
        javascript_highlights.as_str(),
        tree_sitter_javascript::INJECTIONS_QUERY,
        tree_sitter_javascript::LOCALS_QUERY
    );
    insert!(
        LanguageId::Jsx,
        tree_sitter_javascript::LANGUAGE,
        "jsx",
        javascript_highlights.as_str(),
        tree_sitter_javascript::INJECTIONS_QUERY,
        tree_sitter_javascript::LOCALS_QUERY
    );
    insert!(
        LanguageId::Json,
        tree_sitter_json::LANGUAGE,
        "json",
        tree_sitter_json::HIGHLIGHTS_QUERY
    );
    insert!(
        LanguageId::Lua,
        tree_sitter_lua::LANGUAGE,
        "lua",
        tree_sitter_lua::HIGHLIGHTS_QUERY,
        tree_sitter_lua::INJECTIONS_QUERY,
        tree_sitter_lua::LOCALS_QUERY
    );
    insert!(
        LanguageId::Markdown,
        tree_sitter_md::LANGUAGE,
        "markdown",
        tree_sitter_md::HIGHLIGHT_QUERY_BLOCK,
        tree_sitter_md::INJECTION_QUERY_BLOCK,
        ""
    );
    configs.insert(
        ConfigId::MarkdownInline,
        configured(
            tree_sitter_md::INLINE_LANGUAGE.into(),
            "markdown_inline",
            tree_sitter_md::HIGHLIGHT_QUERY_INLINE,
            tree_sitter_md::INJECTION_QUERY_INLINE,
            "",
        )?,
    );
    insert!(
        LanguageId::Python,
        tree_sitter_python::LANGUAGE,
        "python",
        tree_sitter_python::HIGHLIGHTS_QUERY
    );
    insert!(
        LanguageId::Rust,
        tree_sitter_rust::LANGUAGE,
        "rust",
        tree_sitter_rust::HIGHLIGHTS_QUERY,
        tree_sitter_rust::INJECTIONS_QUERY,
        ""
    );
    insert!(
        LanguageId::Sql,
        tree_sitter_sequel::LANGUAGE,
        "sql",
        tree_sitter_sequel::HIGHLIGHTS_QUERY
    );
    insert!(
        LanguageId::Toml,
        tree_sitter_toml_ng::LANGUAGE,
        "toml",
        tree_sitter_toml_ng::HIGHLIGHTS_QUERY
    );
    insert!(
        LanguageId::Tsx,
        tree_sitter_typescript::LANGUAGE_TSX,
        "tsx",
        tree_sitter_typescript::HIGHLIGHTS_QUERY,
        "",
        tree_sitter_typescript::LOCALS_QUERY
    );
    insert!(
        LanguageId::TypeScript,
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
        "typescript",
        tree_sitter_typescript::HIGHLIGHTS_QUERY,
        "",
        tree_sitter_typescript::LOCALS_QUERY
    );
    insert!(
        LanguageId::Yaml,
        tree_sitter_yaml::LANGUAGE,
        "yaml",
        tree_sitter_yaml::HIGHLIGHTS_QUERY
    );

    let mut aliases = HashMap::new();
    let mut add_aliases = |id, values: &'static [&'static str]| {
        for value in values {
            aliases.insert(*value, id);
        }
    };
    add_aliases(
        ConfigId::Public(LanguageId::Bash),
        &["bash", "sh", "shell", "shellscript", "zsh"],
    );
    add_aliases(ConfigId::Public(LanguageId::C), &["c"]);
    add_aliases(ConfigId::Public(LanguageId::Cpp), &["cpp", "c++", "cxx"]);
    add_aliases(
        ConfigId::Public(LanguageId::CSharp),
        &["c_sharp", "csharp", "c#", "cs"],
    );
    add_aliases(ConfigId::Public(LanguageId::Css), &["css"]);
    add_aliases(ConfigId::Public(LanguageId::Go), &["go", "golang"]);
    add_aliases(ConfigId::Public(LanguageId::Html), &["html", "htm"]);
    add_aliases(ConfigId::Public(LanguageId::Java), &["java"]);
    add_aliases(
        ConfigId::Public(LanguageId::JavaScript),
        &["javascript", "js", "node"],
    );
    add_aliases(ConfigId::Public(LanguageId::Jsx), &["jsx"]);
    add_aliases(ConfigId::Public(LanguageId::Json), &["json", "jsonc"]);
    add_aliases(ConfigId::Public(LanguageId::Lua), &["lua"]);
    add_aliases(ConfigId::Public(LanguageId::Markdown), &["markdown", "md"]);
    add_aliases(ConfigId::MarkdownInline, &["markdown_inline"]);
    add_aliases(ConfigId::Public(LanguageId::Python), &["python", "py"]);
    add_aliases(ConfigId::Public(LanguageId::Rust), &["rust", "rs"]);
    add_aliases(ConfigId::Public(LanguageId::Sql), &["sql"]);
    add_aliases(ConfigId::Public(LanguageId::Toml), &["toml"]);
    add_aliases(ConfigId::Public(LanguageId::Tsx), &["tsx"]);
    add_aliases(
        ConfigId::Public(LanguageId::TypeScript),
        &["typescript", "ts"],
    );
    add_aliases(ConfigId::Public(LanguageId::Yaml), &["yaml", "yml"]);

    Ok(Registry { configs, aliases })
}

fn normalize_token(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace('-', "_")
}

pub fn detect_fence(language: &str) -> Option<LanguageId> {
    let token = language
        .split_whitespace()
        .next()?
        .trim_matches(['{', '}'])
        .trim_start_matches('.');
    let token = normalize_token(token);
    let token = token.strip_prefix("language_").unwrap_or(&token);
    let registry = registry().ok()?;
    match registry.aliases.get(token)? {
        ConfigId::Public(id) => Some(*id),
        ConfigId::MarkdownInline => Some(LanguageId::Markdown),
    }
}

pub fn detect_path(path: &Path, source: &str) -> Option<LanguageId> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    match file_name.to_ascii_lowercase().as_str() {
        ".bashrc" | ".bash_profile" | ".profile" => return Some(LanguageId::Bash),
        _ => {}
    }

    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    if let Some(language) = extension.as_deref().and_then(detect_extension) {
        return Some(language);
    }

    detect_shebang(source.lines().next().unwrap_or_default())
}

fn detect_extension(extension: &str) -> Option<LanguageId> {
    Some(match extension {
        "sh" | "bash" => LanguageId::Bash,
        "c" => LanguageId::C,
        "h" | "cc" | "cp" | "cpp" | "cxx" | "h++" | "hh" | "hpp" | "hxx" => LanguageId::Cpp,
        "cs" => LanguageId::CSharp,
        "css" => LanguageId::Css,
        "go" => LanguageId::Go,
        "htm" | "html" => LanguageId::Html,
        "java" => LanguageId::Java,
        "cjs" | "js" | "mjs" => LanguageId::JavaScript,
        "jsx" => LanguageId::Jsx,
        "json" | "jsonc" => LanguageId::Json,
        "lua" => LanguageId::Lua,
        "markdown" | "md" | "mdown" | "mkd" => LanguageId::Markdown,
        "py" | "pyi" | "pyw" => LanguageId::Python,
        "rs" => LanguageId::Rust,
        "sql" => LanguageId::Sql,
        "toml" => LanguageId::Toml,
        "tsx" => LanguageId::Tsx,
        "cts" | "mts" | "ts" => LanguageId::TypeScript,
        "yaml" | "yml" => LanguageId::Yaml,
        _ => return None,
    })
}

fn detect_shebang(line: &str) -> Option<LanguageId> {
    if !line.starts_with("#!") {
        return None;
    }
    let lower = line.to_ascii_lowercase();
    if lower.contains("python") {
        Some(LanguageId::Python)
    } else if lower.contains("node") || lower.contains("deno") {
        Some(LanguageId::JavaScript)
    } else if lower.contains("bash") || lower.ends_with("/sh") || lower.contains(" sh ") {
        Some(LanguageId::Bash)
    } else if lower.contains("lua") {
        Some(LanguageId::Lua)
    } else {
        None
    }
}

pub fn highlight_path(
    path: &Path,
    source: &str,
    cancellation: Option<&AtomicUsize>,
) -> Result<Option<HighlightResult>> {
    let Some(language) = detect_path(path, source) else {
        return Ok(None);
    };
    highlight(language, source, cancellation).map(Some)
}

pub fn highlight(
    language: LanguageId,
    source: &str,
    cancellation: Option<&AtomicUsize>,
) -> Result<HighlightResult> {
    let registry = registry()?;
    let config = registry
        .config(language)
        .ok_or_else(|| anyhow!("missing Tree-sitter config for {}", language.as_str()))?;

    HIGHLIGHTER.with(|cell| {
        let mut highlighter = cell.borrow_mut();
        let events = highlighter
            .highlight(config, source.as_bytes(), cancellation, |name| {
                registry.injection(name)
            })
            .with_context(|| format!("highlight {} source", language.as_str()))?;

        let mut active = Vec::new();
        let mut spans: Vec<HighlightSpan> = Vec::new();
        for event in events {
            match event.with_context(|| format!("iterate {} highlights", language.as_str()))? {
                HighlightEvent::HighlightStart(highlight) => {
                    active.push(HIGHLIGHT_KINDS[highlight.0]);
                }
                HighlightEvent::HighlightEnd => {
                    active.pop();
                }
                HighlightEvent::Source { start, end } => {
                    let Some(kind) = active.last().copied() else {
                        continue;
                    };
                    if start >= end || end > source.len() {
                        continue;
                    }
                    if let Some(previous) = spans.last_mut() {
                        if previous.kind == kind && previous.range.end == start {
                            previous.range.end = end;
                            continue;
                        }
                    }
                    spans.push(HighlightSpan {
                        range: start..end,
                        kind,
                    });
                }
            }
        }
        Ok(HighlightResult { language, spans })
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyntaxPoint {
    pub row: usize,
    pub column: usize,
}

impl From<SyntaxPoint> for Point {
    fn from(value: SyntaxPoint) -> Self {
        Point::new(value.row, value.column)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyntaxEdit {
    pub start_byte: usize,
    pub old_end_byte: usize,
    pub new_end_byte: usize,
    pub start_position: SyntaxPoint,
    pub old_end_position: SyntaxPoint,
    pub new_end_position: SyntaxPoint,
}

impl From<SyntaxEdit> for InputEdit {
    fn from(value: SyntaxEdit) -> Self {
        Self {
            start_byte: value.start_byte,
            old_end_byte: value.old_end_byte,
            new_end_byte: value.new_end_byte,
            start_position: value.start_position.into(),
            old_end_position: value.old_end_position.into(),
            new_end_position: value.new_end_position.into(),
        }
    }
}

pub struct SyntaxSession {
    language: LanguageId,
    parser: Parser,
    tree: Tree,
    revision: u64,
}

impl SyntaxSession {
    pub fn new(language: LanguageId, source: &str, revision: u64) -> Result<Self> {
        let registry = registry()?;
        let config = registry
            .config(language)
            .ok_or_else(|| anyhow!("missing Tree-sitter config for {}", language.as_str()))?;
        let mut parser = Parser::new();
        parser
            .set_language(&config.language)
            .with_context(|| format!("set {} parser language", language.as_str()))?;
        let tree = parser
            .parse(source, None)
            .ok_or_else(|| anyhow!("{} parse cancelled", language.as_str()))?;
        Ok(Self {
            language,
            parser,
            tree,
            revision,
        })
    }

    pub fn language(&self) -> LanguageId {
        self.language
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    pub fn apply_edit(
        &mut self,
        edit: SyntaxEdit,
        new_source: &str,
        revision: u64,
    ) -> Result<Vec<Range<usize>>> {
        self.tree.edit(&edit.into());
        let edited_old_tree = self.tree.clone();
        let new_tree = self
            .parser
            .parse(new_source, Some(&self.tree))
            .ok_or_else(|| anyhow!("{} incremental parse cancelled", self.language.as_str()))?;
        let changed = edited_old_tree
            .changed_ranges(&new_tree)
            .map(|range| range.start_byte..range.end_byte)
            .collect();
        self.tree = new_tree;
        self.revision = revision;
        Ok(changed)
    }

    /// Produce semantic spans for the session's current source.
    ///
    /// The syntax tree itself is updated incrementally.  The official
    /// highlighter currently reparses for highlighting, but this API boundary
    /// allows a future editor to switch to changed-range queries without
    /// changing GUI consumers.
    pub fn highlight(
        &self,
        source: &str,
        cancellation: Option<&AtomicUsize>,
    ) -> Result<HighlightResult> {
        highlight(self.language, source, cancellation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn detects_extensions_fences_and_shebangs() {
        assert_eq!(
            detect_path(Path::new("main.rs"), ""),
            Some(LanguageId::Rust)
        );
        assert_eq!(
            detect_path(Path::new("view.tsx"), ""),
            Some(LanguageId::Tsx)
        );
        assert_eq!(
            detect_path(Path::new("unknown"), "#!/usr/bin/env python3\n"),
            Some(LanguageId::Python)
        );
        assert_eq!(detect_fence("C++"), Some(LanguageId::Cpp));
        assert_eq!(detect_fence("typescript"), Some(LanguageId::TypeScript));
        assert_eq!(detect_fence("rust title=main.rs"), Some(LanguageId::Rust));
        assert_eq!(detect_fence("{.python}"), Some(LanguageId::Python));
        assert_eq!(detect_path(Path::new("unknown.xyz"), ""), None);
    }

    #[test]
    fn every_public_language_builds_and_highlights() {
        let fixtures = [
            (LanguageId::Bash, "if true; then echo hi; fi\n"),
            (LanguageId::C, "int main(void) { return 0; }\n"),
            (LanguageId::Cpp, "auto main() -> int { return 0; }\n"),
            (LanguageId::CSharp, "class C { string S = \"x\"; }\n"),
            (LanguageId::Css, "body { color: red; }\n"),
            (LanguageId::Go, "package main\nfunc main() {}\n"),
            (LanguageId::Html, "<script>const x = 1;</script>\n"),
            (LanguageId::Java, "class Main { int n = 1; }\n"),
            (LanguageId::JavaScript, "const x = () => 1;\n"),
            (LanguageId::Jsx, "const x = <div>ok</div>;\n"),
            (LanguageId::Json, "{\"ok\": true}\n"),
            (LanguageId::Lua, "local value = \"ok\"\n"),
            (
                LanguageId::Markdown,
                "# Hello\n\n```rust\nfn main() {}\n```\n",
            ),
            (LanguageId::Python, "def main():\n    return 1\n"),
            (LanguageId::Rust, "fn main() { let x = 1; }\n"),
            (LanguageId::Sql, "select id from users;\n"),
            (LanguageId::Toml, "name = \"thinkterm\"\n"),
            (LanguageId::Tsx, "const x: JSX.Element = <div />;\n"),
            (LanguageId::TypeScript, "const x: number = 1;\n"),
            (LanguageId::Yaml, "name: thinkterm\n"),
        ];
        for (language, source) in fixtures {
            let result = highlight(language, source, None)
                .unwrap_or_else(|err| panic!("{} failed: {err:#}", language.as_str()));
            assert_eq!(result.language, language);
            assert!(
                !result.spans.is_empty(),
                "{} returned no spans",
                language.as_str()
            );
        }
    }

    #[test]
    fn highlight_ranges_are_valid_utf8_and_include_injections() {
        let source = "<script>const 你好 = \"ok\";</script>";
        let result = highlight(LanguageId::Html, source, None).unwrap();
        assert!(result
            .spans
            .iter()
            .any(|span| span.kind == HighlightKind::Keyword));
        assert!(result
            .spans
            .iter()
            .any(|span| span.kind == HighlightKind::String));
        for span in result.spans {
            assert!(source.is_char_boundary(span.range.start));
            assert!(source.is_char_boundary(span.range.end));
            assert!(span.range.end <= source.len());
        }
    }

    #[test]
    fn preserves_cross_line_state_and_nested_markdown_languages() {
        let javascript = "/* first line\nsecond line */\nconst value = 1;\n";
        let result = highlight(LanguageId::JavaScript, javascript, None).unwrap();
        assert!(result.spans.iter().any(|span| {
            span.kind == HighlightKind::Comment
                && javascript[span.range.clone()].contains("second line")
        }));

        let markdown = "```rust\nfn main() { let value = 1; }\n```\n";
        let result = highlight(LanguageId::Markdown, markdown, None).unwrap();
        assert!(result
            .spans
            .iter()
            .any(|span| span.kind == HighlightKind::Keyword));
    }

    #[test]
    fn highlighting_honors_cancellation() {
        let cancellation = AtomicUsize::new(1);
        let source = "let value = 1;\n".repeat(10_000);
        assert!(highlight(LanguageId::JavaScript, &source, Some(&cancellation)).is_err());
    }

    #[test]
    fn highlights_a_preview_sized_source() {
        let line = "const value = { nested: [1, 2, 3] };\n";
        let source = line.repeat(DEFAULT_HIGHLIGHT_BYTE_LIMIT / line.len());
        let result = highlight(LanguageId::JavaScript, &source, None).unwrap();
        assert!(!result.spans.is_empty());
        assert!(source.len() <= DEFAULT_HIGHLIGHT_BYTE_LIMIT);
    }

    #[test]
    fn incremental_session_matches_a_full_reparse() {
        let old = "fn main() { let value = 1; }\n";
        let inserted = "mut ";
        let start = old.find("value").unwrap();
        let mut new = old.to_string();
        new.insert_str(start, inserted);
        let point = SyntaxPoint {
            row: 0,
            column: start,
        };
        let edit = SyntaxEdit {
            start_byte: start,
            old_end_byte: start,
            new_end_byte: start + inserted.len(),
            start_position: point,
            old_end_position: point,
            new_end_position: SyntaxPoint {
                row: 0,
                column: start + inserted.len(),
            },
        };
        let mut session = SyntaxSession::new(LanguageId::Rust, old, 1).unwrap();
        let changed = session.apply_edit(edit, &new, 2).unwrap();
        assert!(!changed.is_empty());
        let full = SyntaxSession::new(LanguageId::Rust, &new, 2).unwrap();
        assert_eq!(
            session.tree().root_node().to_sexp(),
            full.tree().root_node().to_sexp()
        );
        assert_eq!(
            session.highlight(&new, None).unwrap(),
            highlight(LanguageId::Rust, &new, None).unwrap()
        );
    }
}
