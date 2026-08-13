//! Syntax-highlighting contracts shared by Komet's desktop surfaces.
//!
//! This crate intentionally has no UI, RPC, or engine dependencies. Public
//! ranges are byte offsets relative to one UTF-8 source line.

use std::{ops::Range, path::Path, sync::atomic::AtomicUsize};

use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

pub const DEFAULT_MAX_SOURCE_BYTES: usize = 1024 * 1024;
pub const DEFAULT_MAX_SPANS: usize = 200_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HighlightLimits {
    pub max_source_bytes: usize,
    pub max_spans: usize,
}

impl Default for HighlightLimits {
    fn default() -> Self {
        Self {
            max_source_bytes: DEFAULT_MAX_SOURCE_BYTES,
            max_spans: DEFAULT_MAX_SPANS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LanguageId {
    Rust,
    JavaScript,
    Jsx,
    TypeScript,
    Tsx,
    Python,
    Go,
    Json,
    Jsonc,
    Bash,
    Toml,
    Markdown,
    Html,
    Css,
    Yaml,
    C,
    Cpp,
    CSharp,
    Java,
    Kotlin,
    Swift,
    Ruby,
    Php,
    Sql,
    Lua,
    Dockerfile,
    Nix,
    Make,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HighlightKind {
    Comment,
    Keyword,
    String,
    StringSpecial,
    Escape,
    Number,
    Boolean,
    Type,
    TypeBuiltin,
    Constructor,
    Function,
    FunctionBuiltin,
    Macro,
    Property,
    Constant,
    Variable,
    VariableSpecial,
    Parameter,
    Operator,
    Punctuation,
    Tag,
    Attribute,
    Label,
    Embedded,
    Invalid,
}

impl HighlightKind {
    /// Stable precedence used to resolve overlapping parser captures.
    pub const fn precedence(self) -> u8 {
        match self {
            Self::Invalid => 100,
            Self::Escape => 95,
            Self::Macro => 90,
            Self::Property | Self::Attribute => 85,
            Self::FunctionBuiltin | Self::TypeBuiltin | Self::VariableSpecial => 80,
            Self::StringSpecial | Self::Constructor | Self::Parameter => 75,
            Self::Function | Self::Type | Self::Constant | Self::Tag | Self::Label => 70,
            Self::Comment | Self::Keyword | Self::String | Self::Number | Self::Boolean => 60,
            Self::Variable | Self::Operator => 50,
            Self::Punctuation | Self::Embedded => 40,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighlightSpan {
    pub range: Range<usize>,
    pub kind: HighlightKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighlightedDocument {
    pub language: LanguageId,
    pub lines: Vec<Vec<HighlightSpan>>,
}

#[derive(Debug, Clone, Copy)]
pub struct HighlightRequest<'a> {
    pub source: &'a str,
    pub path: Option<&'a str>,
    pub fence_tag: Option<&'a str>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HighlightError {
    #[error("the source language is not registered")]
    UnknownLanguage,
    #[error("highlight range {start}..{end} is invalid for a {len}-byte source")]
    InvalidRange {
        start: usize,
        end: usize,
        len: usize,
    },
    #[error("highlight range {start}..{end} is not on UTF-8 boundaries")]
    InvalidUtf8Boundary { start: usize, end: usize },
    #[error("source exceeds the configured highlighting limit")]
    SourceTooLarge,
    #[error("highlight output exceeds the configured span limit")]
    TooManySpans,
    #[error("parser failed: {0}")]
    Parser(String),
    #[error("the {0:?} grammar is not bundled")]
    GrammarUnavailable(LanguageId),
}

impl HighlightedDocument {
    /// Validate, split, and normalize absolute source spans into line-relative spans.
    pub fn from_absolute_spans(
        language: LanguageId,
        source: &str,
        spans: impl IntoIterator<Item = HighlightSpan>,
    ) -> Result<Self, HighlightError> {
        let starts = line_starts(source);
        let mut lines = vec![Vec::new(); starts.len()];
        for span in spans {
            validate_span(source, &span.range)?;
            if span.range.is_empty() {
                continue;
            }
            for (line_ix, &start) in starts.iter().enumerate() {
                let raw_end = starts.get(line_ix + 1).copied().unwrap_or(source.len());
                let end = source[..raw_end].trim_end_matches(['\n', '\r']).len();
                let segment_start = span.range.start.max(start);
                let segment_end = span.range.end.min(end);
                if segment_start < segment_end {
                    lines[line_ix].push(HighlightSpan {
                        range: segment_start - start..segment_end - start,
                        kind: span.kind,
                    });
                }
                if raw_end >= span.range.end {
                    break;
                }
            }
        }
        for line in &mut lines {
            *line = normalize_line(std::mem::take(line));
        }
        Ok(Self { language, lines })
    }
}

fn validate_span(source: &str, range: &Range<usize>) -> Result<(), HighlightError> {
    if range.start > range.end || range.end > source.len() {
        return Err(HighlightError::InvalidRange {
            start: range.start,
            end: range.end,
            len: source.len(),
        });
    }
    if !source.is_char_boundary(range.start) || !source.is_char_boundary(range.end) {
        return Err(HighlightError::InvalidUtf8Boundary {
            start: range.start,
            end: range.end,
        });
    }
    Ok(())
}

fn normalize_line(spans: Vec<HighlightSpan>) -> Vec<HighlightSpan> {
    let mut boundaries = spans
        .iter()
        .flat_map(|span| [span.range.start, span.range.end])
        .collect::<Vec<_>>();
    boundaries.sort_unstable();
    boundaries.dedup();
    let mut normalized: Vec<HighlightSpan> = Vec::new();
    for pair in boundaries.windows(2) {
        let range = pair[0]..pair[1];
        if range.is_empty() {
            continue;
        }
        let Some(kind) = spans
            .iter()
            .filter(|span| span.range.start <= range.start && span.range.end >= range.end)
            .map(|span| span.kind)
            .max_by_key(|kind| kind.precedence())
        else {
            continue;
        };
        if let Some(previous) = normalized.last_mut()
            && previous.kind == kind
            && previous.range.end == range.start
        {
            previous.range.end = range.end;
        } else {
            normalized.push(HighlightSpan { range, kind });
        }
    }
    normalized
}

fn line_starts(source: &str) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(
        source
            .match_indices('\n')
            .map(|(index, _)| index + 1)
            .filter(|start| *start < source.len()),
    );
    starts
}

/// Whether this build contains a parser and compatible highlight queries.
pub const fn supports_language(language: LanguageId) -> bool {
    matches!(language, LanguageId::Rust)
}

/// Highlight a complete document with the default resource limits.
pub fn highlight(request: HighlightRequest<'_>) -> Result<HighlightedDocument, HighlightError> {
    highlight_with_limits(request, HighlightLimits::default(), None)
}

/// Highlight a complete document with explicit limits and cooperative cancellation.
pub fn highlight_with_limits(
    request: HighlightRequest<'_>,
    limits: HighlightLimits,
    cancellation_flag: Option<&AtomicUsize>,
) -> Result<HighlightedDocument, HighlightError> {
    if request.source.len() > limits.max_source_bytes {
        return Err(HighlightError::SourceTooLarge);
    }
    let language = detect_language(
        request.path,
        request.fence_tag,
        request.source.lines().next(),
    )
    .ok_or(HighlightError::UnknownLanguage)?;
    if !supports_language(language) {
        return Err(HighlightError::GrammarUnavailable(language));
    }

    let mut configuration = rust_configuration()?;
    configuration.configure(CAPTURE_NAMES);
    let mut highlighter = Highlighter::new();
    let events = highlighter
        .highlight(
            &configuration,
            request.source.as_bytes(),
            cancellation_flag,
            |_| None,
        )
        .map_err(|error| HighlightError::Parser(error.to_string()))?;

    let mut active = Vec::new();
    let mut spans = Vec::new();
    for event in events {
        match event.map_err(|error| HighlightError::Parser(error.to_string()))? {
            HighlightEvent::HighlightStart(highlight) => active.push(CAPTURE_KINDS[highlight.0]),
            HighlightEvent::HighlightEnd => {
                active.pop();
            }
            HighlightEvent::Source { start, end } => {
                if let Some(kind) = active.iter().copied().max_by_key(|kind| kind.precedence()) {
                    spans.push(HighlightSpan {
                        range: start..end,
                        kind,
                    });
                    if spans.len() > limits.max_spans {
                        return Err(HighlightError::TooManySpans);
                    }
                }
            }
        }
    }
    HighlightedDocument::from_absolute_spans(language, request.source, spans)
}

fn rust_configuration() -> Result<HighlightConfiguration, HighlightError> {
    // The upstream Rust query groups numbers and booleans as
    // `constant.builtin`. Komet preserves those structural roles separately.
    let highlights = tree_sitter_rust::HIGHLIGHTS_QUERY
        .replace(
            "(boolean_literal) @constant.builtin",
            "(boolean_literal) @boolean",
        )
        .replace(
            "(integer_literal) @constant.builtin",
            "(integer_literal) @number",
        )
        .replace(
            "(float_literal) @constant.builtin",
            "(float_literal) @number",
        );
    HighlightConfiguration::new(
        tree_sitter_rust::LANGUAGE.into(),
        "rust",
        &highlights,
        tree_sitter_rust::INJECTIONS_QUERY,
        "",
    )
    .map_err(|error| HighlightError::Parser(error.to_string()))
}

// Ordered from generic to specific. `HighlightConfiguration::configure`
// resolves dotted captures to the best recognized name in this table.
const CAPTURE_NAMES: &[&str] = &[
    "comment",
    "keyword",
    "string",
    "string.special",
    "string.escape",
    "number",
    "boolean",
    "type",
    "type.builtin",
    "constructor",
    "function",
    "function.builtin",
    "function.macro",
    "property",
    "constant",
    "variable",
    "variable.builtin",
    "variable.parameter",
    "operator",
    "punctuation",
    "tag",
    "attribute",
    "label",
    "embedded",
    "error",
];

const CAPTURE_KINDS: &[HighlightKind] = &[
    HighlightKind::Comment,
    HighlightKind::Keyword,
    HighlightKind::String,
    HighlightKind::StringSpecial,
    HighlightKind::Escape,
    HighlightKind::Number,
    HighlightKind::Boolean,
    HighlightKind::Type,
    HighlightKind::TypeBuiltin,
    HighlightKind::Constructor,
    HighlightKind::Function,
    HighlightKind::FunctionBuiltin,
    HighlightKind::Macro,
    HighlightKind::Property,
    HighlightKind::Constant,
    HighlightKind::Variable,
    HighlightKind::VariableSpecial,
    HighlightKind::Parameter,
    HighlightKind::Operator,
    HighlightKind::Punctuation,
    HighlightKind::Tag,
    HighlightKind::Attribute,
    HighlightKind::Label,
    HighlightKind::Embedded,
    HighlightKind::Invalid,
];

pub fn detect_language(
    path: Option<&str>,
    fence_tag: Option<&str>,
    first_line: Option<&str>,
) -> Option<LanguageId> {
    fence_tag
        .and_then(language_for_alias)
        .or_else(|| path.and_then(language_for_path))
        .or_else(|| first_line.and_then(language_for_shebang))
}

pub fn language_for_alias(alias: &str) -> Option<LanguageId> {
    let alias = alias
        .trim()
        .split_ascii_whitespace()
        .next()?
        .to_ascii_lowercase();
    Some(match alias.as_str() {
        "rust" | "rs" => LanguageId::Rust,
        "javascript" | "js" | "mjs" | "cjs" => LanguageId::JavaScript,
        "jsx" => LanguageId::Jsx,
        "typescript" | "ts" | "mts" | "cts" => LanguageId::TypeScript,
        "tsx" => LanguageId::Tsx,
        "python" | "py" | "python3" => LanguageId::Python,
        "go" | "golang" => LanguageId::Go,
        "json" => LanguageId::Json,
        "jsonc" => LanguageId::Jsonc,
        "bash" | "sh" | "shell" | "zsh" | "console" => LanguageId::Bash,
        "toml" => LanguageId::Toml,
        "markdown" | "md" => LanguageId::Markdown,
        "html" | "htm" => LanguageId::Html,
        "css" => LanguageId::Css,
        "yaml" | "yml" => LanguageId::Yaml,
        "c" => LanguageId::C,
        "cpp" | "c++" | "cc" | "cxx" | "hpp" => LanguageId::Cpp,
        "csharp" | "c#" | "cs" => LanguageId::CSharp,
        "java" => LanguageId::Java,
        "kotlin" | "kt" | "kts" => LanguageId::Kotlin,
        "swift" => LanguageId::Swift,
        "ruby" | "rb" => LanguageId::Ruby,
        "php" => LanguageId::Php,
        "sql" => LanguageId::Sql,
        "lua" => LanguageId::Lua,
        "dockerfile" | "docker" => LanguageId::Dockerfile,
        "nix" => LanguageId::Nix,
        "make" | "makefile" => LanguageId::Make,
        _ => return None,
    })
}

pub fn language_for_path(path: &str) -> Option<LanguageId> {
    let path = Path::new(path);
    let name = path.file_name()?.to_str()?;
    match name.to_ascii_lowercase().as_str() {
        "dockerfile" | "containerfile" => return Some(LanguageId::Dockerfile),
        "makefile" | "gnumakefile" => return Some(LanguageId::Make),
        "cargo.lock" | "cargo.toml" | "pyproject.toml" => return Some(LanguageId::Toml),
        _ => {}
    }
    language_for_alias(path.extension()?.to_str()?)
}

fn language_for_shebang(line: &str) -> Option<LanguageId> {
    let line = line.strip_prefix("#!")?.to_ascii_lowercase();
    if line.contains("python") {
        Some(LanguageId::Python)
    } else if line.contains("node") {
        Some(LanguageId::JavaScript)
    } else if line.contains("ruby") {
        Some(LanguageId::Ruby)
    } else if ["bash", "zsh", "/sh", " sh"]
        .iter()
        .any(|name| line.contains(name))
    {
        Some(LanguageId::Bash)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_keep_language_variants_distinct() {
        let cases = [
            ("js", LanguageId::JavaScript),
            ("jsx", LanguageId::Jsx),
            ("ts", LanguageId::TypeScript),
            ("tsx", LanguageId::Tsx),
            ("RS", LanguageId::Rust),
            ("shell", LanguageId::Bash),
        ];
        for (alias, expected) in cases {
            assert_eq!(language_for_alias(alias), Some(expected), "{alias}");
        }
        assert_eq!(language_for_alias("unknown-lang"), None);
    }

    #[test]
    fn paths_and_exact_names_are_table_driven() {
        let cases = [
            ("src/main.rs", LanguageId::Rust),
            ("web/app.tsx", LanguageId::Tsx),
            ("Cargo.toml", LanguageId::Toml),
            ("Dockerfile", LanguageId::Dockerfile),
            ("GNUmakefile", LanguageId::Make),
            ("config.jsonc", LanguageId::Jsonc),
        ];
        for (path, expected) in cases {
            assert_eq!(language_for_path(path), Some(expected), "{path}");
        }
        assert_eq!(language_for_path("README"), None);
        assert_eq!(language_for_path("image.png"), None);
    }

    #[test]
    fn shebang_is_only_used_after_explicit_hints() {
        assert_eq!(
            detect_language(None, None, Some("#!/usr/bin/env python3")),
            Some(LanguageId::Python)
        );
        assert_eq!(detect_language(None, None, Some("let x = 1")), None);
    }

    #[test]
    fn spans_are_valid_sorted_non_overlapping_and_line_relative() {
        let source = "let café = \"x\";\nnext";
        let document = HighlightedDocument::from_absolute_spans(
            LanguageId::Rust,
            source,
            [
                HighlightSpan {
                    range: 0..9,
                    kind: HighlightKind::Variable,
                },
                HighlightSpan {
                    range: 0..3,
                    kind: HighlightKind::Keyword,
                },
                HighlightSpan {
                    range: 12..15,
                    kind: HighlightKind::String,
                },
                HighlightSpan {
                    range: 17..21,
                    kind: HighlightKind::Function,
                },
            ],
        )
        .unwrap();
        assert_eq!(document.lines.len(), 2);
        assert_eq!(
            document.lines[0][0],
            HighlightSpan {
                range: 0..3,
                kind: HighlightKind::Keyword
            }
        );
        for line in document.lines {
            assert!(
                line.windows(2)
                    .all(|pair| pair[0].range.end <= pair[1].range.start)
            );
        }
        assert_eq!(
            HighlightedDocument::from_absolute_spans(
                LanguageId::Rust,
                source,
                [HighlightSpan {
                    range: 8..9,
                    kind: HighlightKind::Type
                }]
            ),
            Err(HighlightError::InvalidUtf8Boundary { start: 8, end: 9 })
        );
    }

    fn highlighted_fragments(source: &str) -> Vec<(&str, HighlightKind)> {
        let document = highlight(HighlightRequest {
            source,
            path: Some("src/lib.rs"),
            fence_tag: None,
        })
        .unwrap();
        source
            .lines()
            .zip(document.lines)
            .flat_map(|(line, spans)| {
                spans
                    .into_iter()
                    .map(move |span| (&line[span.range], span.kind))
            })
            .collect()
    }

    #[test]
    fn rust_highlighting_distinguishes_structural_categories() {
        let source = r#"pub struct Widget { field: usize }
fn build(value: usize) -> Widget {
    let name = format!("item-{value}");
    Widget { field: 42 }
}"#;
        let fragments = highlighted_fragments(source);
        for (text, expected) in [
            ("pub", HighlightKind::Keyword),
            ("Widget", HighlightKind::Type),
            ("build", HighlightKind::Function),
            ("format!", HighlightKind::Macro),
            ("42", HighlightKind::Number),
        ] {
            assert!(
                fragments.iter().any(|item| *item == (text, expected)),
                "missing {text:?} as {expected:?}: {fragments:?}"
            );
        }
    }

    #[test]
    fn rust_multiline_raw_unicode_and_incomplete_code_remain_valid() {
        let source = "/* café\ncomment */\nlet raw = r#\"héllo\nworld\"#;\nlet before = 7;\nfn incomplete( {";
        let document = highlight(HighlightRequest {
            source,
            path: None,
            fence_tag: Some("rust"),
        })
        .unwrap();
        assert!(
            document.lines[1]
                .iter()
                .any(|span| span.kind == HighlightKind::Comment)
        );
        assert!(
            document.lines[3]
                .iter()
                .any(|span| span.kind == HighlightKind::String)
        );
        assert!(
            document
                .lines
                .iter()
                .flatten()
                .any(|span| span.kind == HighlightKind::Number)
        );
        for (line, spans) in source.lines().zip(&document.lines) {
            for span in spans {
                assert!(line.is_char_boundary(span.range.start));
                assert!(line.is_char_boundary(span.range.end));
            }
        }
    }

    #[test]
    fn limits_and_unbundled_languages_degrade_with_typed_errors() {
        assert_eq!(
            highlight_with_limits(
                HighlightRequest {
                    source: "fn main() {}",
                    path: Some("main.rs"),
                    fence_tag: None,
                },
                HighlightLimits {
                    max_source_bytes: 2,
                    max_spans: 10
                },
                None,
            ),
            Err(HighlightError::SourceTooLarge)
        );
        assert_eq!(
            highlight(HighlightRequest {
                source: "const x = 1;",
                path: Some("app.ts"),
                fence_tag: None,
            }),
            Err(HighlightError::GrammarUnavailable(LanguageId::TypeScript))
        );
    }

    #[test]
    fn rust_queries_load_for_the_bundled_abi() {
        assert!(rust_configuration().is_ok());
        assert!(tree_sitter::LANGUAGE_VERSION >= tree_sitter::MIN_COMPATIBLE_LANGUAGE_VERSION);
    }
}
