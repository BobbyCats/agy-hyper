use serde::{Deserialize, Serialize};
use std::path::Path;
use tree_sitter::{Language, Node, Parser};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolInfo {
    pub name: String,
    pub qualified_name: String,
    pub kind: String,
    pub start_line: usize,
    pub end_line: usize,
    pub start_byte: usize,
    pub end_byte: usize,
    pub signature: String,
    pub depth: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyntaxDiagnostic {
    pub line: usize,
    pub col: usize,
    pub kind: String,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileOutline {
    pub language: String,
    pub parser_engine: String,
    pub total_lines: usize,
    pub has_bom: bool,
    pub symbols: Vec<SymbolInfo>,
    pub imports: Vec<String>,
    pub syntax_errors: Vec<SyntaxDiagnostic>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportedLang {
    TypeScript,
    Tsx,
    JavaScript,
    Python,
    Rust,
    Go,
    Json,
    Markdown,
    Fallback,
}

impl SupportedLang {
    pub fn from_path(path: &Path) -> Self {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        match ext.as_str() {
            "ts" | "mts" | "cts" => Self::TypeScript,
            "tsx" => Self::Tsx,
            "js" | "mjs" | "cjs" | "jsx" => Self::JavaScript,
            "py" | "pyi" => Self::Python,
            "rs" => Self::Rust,
            "go" => Self::Go,
            "json" => Self::Json,
            "md" | "markdown" | "mdx" => Self::Markdown,
            _ => Self::Fallback,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::TypeScript => "typescript",
            Self::Tsx => "tsx",
            Self::JavaScript => "javascript",
            Self::Python => "python",
            Self::Rust => "rust",
            Self::Go => "go",
            Self::Json => "json",
            Self::Markdown => "markdown",
            Self::Fallback => "generic",
        }
    }

    fn ts_language(&self) -> Option<Language> {
        match self {
            Self::TypeScript => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
            Self::Tsx => Some(tree_sitter_typescript::LANGUAGE_TSX.into()),
            Self::JavaScript => Some(tree_sitter_javascript::LANGUAGE.into()),
            Self::Python => Some(tree_sitter_python::LANGUAGE.into()),
            Self::Rust => Some(tree_sitter_rust::LANGUAGE.into()),
            Self::Go => Some(tree_sitter_go::LANGUAGE.into()),
            Self::Json => Some(tree_sitter_json::LANGUAGE.into()),
            Self::Markdown | Self::Fallback => None,
        }
    }
}

/// Parse a source file using true Tree-sitter AST (or clean BOM-safe fallback for Markdown/other).
/// Never breaks on Rust lifetimes (`<'a>`), C++ digit separators (`1'000`), Go raw strings (`` `\` ``),
/// JS template literals, or UTF-8 BOM.
pub fn outline_source(path: &Path, raw_source: &str) -> FileOutline {
    let (bom_offset, source) = if let Some(stripped) = raw_source.strip_prefix('\u{feff}') {
        (3usize, stripped)
    } else {
        (0usize, raw_source)
    };
    let total_lines = if source.is_empty() {
        0
    } else {
        source.bytes().filter(|&b| b == b'\n').count() + 1
    };
    let lang = SupportedLang::from_path(path);

    if let Some(ts_lang) = lang.ts_language() {
        let mut parser = Parser::new();
        if parser.set_language(&ts_lang).is_ok() {
            if let Some(tree) = parser.parse(source, None) {
                let root = tree.root_node();
                let mut symbols = Vec::new();
                let mut imports = Vec::new();
                let mut errors = Vec::new();
                let mut scope_stack: Vec<String> = Vec::new();
                walk_ts_node(
                    root,
                    lang,
                    source.as_bytes(),
                    bom_offset,
                    &mut scope_stack,
                    &mut symbols,
                    &mut imports,
                );
                collect_syntax_errors(root, source.as_bytes(), &mut errors, 24);
                imports.sort();
                imports.dedup();
                return FileOutline {
                    language: lang.name().into(),
                    parser_engine: "tree-sitter".into(),
                    total_lines,
                    has_bom: bom_offset > 0,
                    symbols,
                    imports,
                    syntax_errors: errors,
                };
            }
        }
    }

    if lang == SupportedLang::Markdown {
        return outline_markdown(source, bom_offset, total_lines);
    }

    outline_generic_fallback(source, bom_offset, total_lines)
}

/// Fast `<1ms` post-edit Tree-sitter syntax verification.
pub fn check_syntax(path: &Path, raw_source: &str) -> Vec<SyntaxDiagnostic> {
    let source = raw_source.strip_prefix('\u{feff}').unwrap_or(raw_source);
    let lang = SupportedLang::from_path(path);
    let Some(ts_lang) = lang.ts_language() else {
        return Vec::new();
    };
    let mut parser = Parser::new();
    if parser.set_language(&ts_lang).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let mut errors = Vec::new();
    collect_syntax_errors(tree.root_node(), source.as_bytes(), &mut errors, 12);
    errors
}

/// Find a unique symbol by exact `qualified_name` or `name` for AST-targeted editing.
pub fn find_symbol(path: &Path, raw_source: &str, query: &str) -> Result<SymbolInfo, String> {
    let outline = outline_source(path, raw_source);
    let q = query.trim();
    let exact_q: Vec<SymbolInfo> = outline
        .symbols
        .iter()
        .filter(|s| s.qualified_name == q)
        .cloned()
        .collect();
    if exact_q.len() == 1 {
        return Ok(exact_q.into_iter().next().unwrap());
    }
    if exact_q.len() > 1 {
        return Err(format!(
            "ambiguous qualified symbol '{}' ({} matches)",
            q,
            exact_q.len()
        ));
    }

    let exact_n: Vec<SymbolInfo> = outline
        .symbols
        .iter()
        .filter(|s| s.name == q)
        .cloned()
        .collect();
    if exact_n.len() == 1 {
        return Ok(exact_n.into_iter().next().unwrap());
    }
    if exact_n.len() > 1 {
        let candidates: Vec<String> = exact_n
            .iter()
            .take(5)
            .map(|s| format!("{} (L{})", s.qualified_name, s.start_line))
            .collect();
        return Err(format!(
            "ambiguous symbol '{}' — disambiguate with qualified name: {}",
            q,
            candidates.join(", ")
        ));
    }
    Err(format!("symbol '{}' not found in {}", q, path.display()))
}

fn walk_ts_node(
    node: Node,
    lang: SupportedLang,
    src: &[u8],
    bom_offset: usize,
    scope: &mut Vec<String>,
    symbols: &mut Vec<SymbolInfo>,
    imports: &mut Vec<String>,
) {
    let kind = node.kind();

    // Extract import/use edges
    match (lang, kind) {
        (SupportedLang::TypeScript | SupportedLang::Tsx | SupportedLang::JavaScript, "import_statement" | "export_statement") => {
            if let Some(src_node) = node.child_by_field_name("source") {
                if let Ok(txt) = src_node.utf8_text(src) {
                    let clean = txt.trim_matches(&['"', '\'', '`'][..]);
                    if !clean.is_empty() {
                        imports.push(clean.to_string());
                    }
                }
            }
        }
        (SupportedLang::Rust, "use_declaration") => {
            if let Ok(txt) = node.utf8_text(src) {
                extract_rust_use_specs(txt, imports);
            }
        }
        (SupportedLang::Python, "import_from_statement") => {
            if let Some(mod_node) = node.child_by_field_name("module_name") {
                if let Ok(txt) = mod_node.utf8_text(src) {
                    let clean = txt.trim();
                    if !clean.is_empty() {
                        imports.push(clean.to_string());
                    }
                }
            }
        }
        (SupportedLang::Python, "import_statement") => {
            let mut c = node.walk();
            for ch in node.children_by_field_name("name", &mut c) {
                let target = if ch.kind() == "aliased_import" {
                    ch.child_by_field_name("name").unwrap_or(ch)
                } else {
                    ch
                };
                if let Ok(txt) = target.utf8_text(src) {
                    let clean = txt.trim();
                    if !clean.is_empty() {
                        imports.push(clean.to_string());
                    }
                }
            }
        }
        (SupportedLang::Go, "import_spec") => {
            if let Some(p) = node.child_by_field_name("path") {
                if let Ok(txt) = p.utf8_text(src) {
                    imports.push(txt.trim_matches('"').to_string());
                }
            }
        }
        _ => {}
    }

    let extracted = extract_symbol_from_node(node, lang, src);
    let mut pushed_scope = false;

    if let Some((sym_kind, sym_name, is_container)) = extracted {
        let qualified = if scope.is_empty() {
            sym_name.clone()
        } else {
            format!("{}::{}", scope.join("::"), sym_name)
        };
        let sig = node
            .utf8_text(src)
            .map(|t| first_line_compact(t, 110))
            .unwrap_or_else(|_| sym_name.clone());

        // For JSON, only record top-level / depth<=1 keys to avoid noise
        if lang != SupportedLang::Json || scope.len() <= 1 {
            symbols.push(SymbolInfo {
                name: sym_name.clone(),
                qualified_name: qualified,
                kind: sym_kind.to_string(),
                start_line: node.start_position().row + 1,
                end_line: node.end_position().row + 1,
                start_byte: node.start_byte() + bom_offset,
                end_byte: node.end_byte() + bom_offset,
                signature: sig,
                depth: scope.len(),
            });
        }
        if is_container {
            scope.push(sym_name);
            pushed_scope = true;
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        // Don't descend infinitely into deeply nested JSON values
        if lang == SupportedLang::Json && scope.len() >= 2 {
            continue;
        }
        walk_ts_node(child, lang, src, bom_offset, scope, symbols, imports);
    }

    if pushed_scope {
        scope.pop();
    }
}

fn extract_symbol_from_node(
    node: Node,
    lang: SupportedLang,
    src: &[u8],
) -> Option<(&'static str, String, bool)> {
    let kind = node.kind();
    let text_of = |n: Node| n.utf8_text(src).ok().map(|s| s.trim().to_string());
    let field_name = |f: &str| node.child_by_field_name(f).and_then(text_of);

    match lang {
        SupportedLang::TypeScript | SupportedLang::Tsx | SupportedLang::JavaScript => match kind {
            "function_declaration" | "generator_function_declaration" => {
                Some(("fn", field_name("name")?, false))
            }
            "class_declaration" | "abstract_class_declaration" => {
                Some(("class", field_name("name")?, true))
            }
            "interface_declaration" => Some(("interface", field_name("name")?, true)),
            "type_alias_declaration" => Some(("type", field_name("name")?, false)),
            "enum_declaration" => Some(("enum", field_name("name")?, true)),
            "method_definition" | "method_signature" => {
                Some(("method", field_name("name")?, false))
            }
            "variable_declarator" => {
                let val = node.child_by_field_name("value")?;
                let vk = val.kind();
                if vk == "arrow_function" || vk == "function_expression" || vk == "class" {
                    let name = field_name("name")?;
                    Some(("const_fn", name, vk == "class"))
                } else {
                    None
                }
            }
            _ => None,
        },
        SupportedLang::Rust => match kind {
            "function_item" | "function_signature_item" => {
                Some(("fn", field_name("name")?, false))
            }
            "struct_item" => Some(("struct", field_name("name")?, true)),
            "enum_item" => Some(("enum", field_name("name")?, true)),
            "trait_item" => Some(("trait", field_name("name")?, true)),
            "mod_item" => Some(("mod", field_name("name")?, true)),
            "type_item" => Some(("type", field_name("name")?, false)),
            "const_item" | "static_item" => Some(("const", field_name("name")?, false)),
            "impl_item" => {
                let ty = field_name("type")?;
                let tr = field_name("trait");
                let label = match tr {
                    Some(t) => format!("{} for {}", t, ty),
                    None => ty,
                };
                Some(("impl", label, true))
            }
            _ => None,
        },
        SupportedLang::Python => match kind {
            "function_definition" => Some(("def", field_name("name")?, false)),
            "class_definition" => Some(("class", field_name("name")?, true)),
            _ => None,
        },
        SupportedLang::Go => match kind {
            "function_declaration" => Some(("func", field_name("name")?, false)),
            "method_declaration" => {
                let name = field_name("name")?;
                let recv = node
                    .child_by_field_name("receiver")
                    .and_then(text_of)
                    .unwrap_or_default();
                let label = if recv.is_empty() {
                    name
                } else {
                    format!("{} {}", recv, name)
                };
                Some(("method", label, false))
            }
            "type_spec" => Some(("type", field_name("name")?, false)),
            _ => None,
        },
        SupportedLang::Json => match kind {
            "pair" => {
                let key = field_name("key")?
                    .trim_matches('"')
                    .to_string();
                let val_is_obj = node
                    .child_by_field_name("value")
                    .map(|v| v.kind() == "object")
                    .unwrap_or(false);
                Some(("key", key, val_is_obj))
            }
            _ => None,
        },
        SupportedLang::Markdown | SupportedLang::Fallback => None,
    }
}

fn collect_syntax_errors(
    node: Node,
    src: &[u8],
    out: &mut Vec<SyntaxDiagnostic>,
    limit: usize,
) {
    if out.len() >= limit || !node.has_error() {
        return;
    }
    if node.is_error() || node.is_missing() {
        let pos = node.start_position();
        let snippet = node
            .utf8_text(src)
            .map(|t| first_line_compact(t, 80))
            .unwrap_or_default();
        out.push(SyntaxDiagnostic {
            line: pos.row + 1,
            col: pos.column + 1,
            kind: if node.is_missing() {
                format!("MISSING {}", node.kind())
            } else {
                "SYNTAX_ERROR".into()
            },
            snippet,
        });
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if out.len() >= limit {
            break;
        }
        if child.has_error() || child.is_error() || child.is_missing() {
            collect_syntax_errors(child, src, out, limit);
        }
    }
}

fn outline_markdown(source: &str, bom_offset: usize, total_lines: usize) -> FileOutline {
    let mut symbols = Vec::new();
    let mut byte_pos = 0usize;
    let mut in_fence = false;
    for (idx, line) in source.split_inclusive('\n').enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
        } else if !in_fence && trimmed.starts_with('#') {
            let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
            if hashes <= 4 && trimmed.as_bytes().get(hashes) == Some(&b' ') {
                let title = trimmed[hashes..].trim().to_string();
                if !title.is_empty() {
                    symbols.push(SymbolInfo {
                        name: title.clone(),
                        qualified_name: title.clone(),
                        kind: format!("h{}", hashes),
                        start_line: idx + 1,
                        end_line: idx + 1,
                        start_byte: byte_pos + bom_offset,
                        end_byte: byte_pos + line.len() + bom_offset,
                        signature: first_line_compact(trimmed, 100),
                        depth: hashes.saturating_sub(1),
                    });
                }
            }
        }
        byte_pos += line.len();
    }
    FileOutline {
        language: "markdown".into(),
        parser_engine: "markdown-headings".into(),
        total_lines,
        has_bom: bom_offset > 0,
        symbols,
        imports: Vec::new(),
        syntax_errors: Vec::new(),
    }
}

fn outline_generic_fallback(source: &str, bom_offset: usize, total_lines: usize) -> FileOutline {
    let mut symbols = Vec::new();
    let mut byte_pos = 0usize;
    let mut in_block_comment = false;

    for (idx, line) in source.split_inclusive('\n').enumerate() {
        let trimmed = line.trim();
        if in_block_comment {
            if trimmed.contains("*/") {
                in_block_comment = false;
            }
            byte_pos += line.len();
            continue;
        }
        if trimmed.starts_with("/*") && !trimmed.contains("*/") {
            in_block_comment = true;
            byte_pos += line.len();
            continue;
        }
        // Lightweight declarative line capture (TOML sections, Makefile targets, C/Swift/Java/Shell defs)
        if let Some(sec) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            if !sec.is_empty() && !sec.contains(' ') {
                symbols.push(SymbolInfo {
                    name: sec.to_string(),
                    qualified_name: sec.to_string(),
                    kind: "section".into(),
                    start_line: idx + 1,
                    end_line: idx + 1,
                    start_byte: byte_pos + bom_offset,
                    end_byte: byte_pos + line.len() + bom_offset,
                    signature: first_line_compact(trimmed, 90),
                    depth: 0,
                });
            }
        } else if (trimmed.starts_with("fn ")
            || trimmed.starts_with("func ")
            || trimmed.starts_with("def ")
            || trimmed.starts_with("class ")
            || trimmed.starts_with("struct ")
            || trimmed.starts_with("enum ")
            || trimmed.starts_with("protocol ")
            || trimmed.starts_with("public ")
            || trimmed.starts_with("private "))
            && trimmed.len() <= 160
        {
            let tokens: Vec<&str> = trimmed.split_whitespace().collect();
            if tokens.len() >= 2 {
                let name = tokens[1]
                    .split(&['(', '{', ':', '<'][..])
                    .next()
                    .unwrap_or(tokens[1])
                    .to_string();
                if !name.is_empty() {
                    symbols.push(SymbolInfo {
                        name: name.clone(),
                        qualified_name: name,
                        kind: tokens[0].to_string(),
                        start_line: idx + 1,
                        end_line: idx + 1,
                        start_byte: byte_pos + bom_offset,
                        end_byte: byte_pos + line.len() + bom_offset,
                        signature: first_line_compact(trimmed, 100),
                        depth: 0,
                    });
                }
            }
        }
        byte_pos += line.len();
    }

    FileOutline {
        language: "generic".into(),
        parser_engine: "line-heuristic".into(),
        total_lines,
        has_bom: bom_offset > 0,
        symbols,
        imports: Vec::new(),
        syntax_errors: Vec::new(),
    }
}

/// Normalize `pub(...) use ...;` and expand brace groups (`crate::{a, b}`) into individual specs.
fn extract_rust_use_specs(raw: &str, out: &mut Vec<String>) {
    let mut s = raw.trim().trim_end_matches(';').trim();
    if let Some(rest) = s.strip_prefix("pub") {
        let rest = rest.trim_start();
        if let Some(after_paren) = rest.strip_prefix('(') {
            if let Some(close) = after_paren.find(')') {
                s = after_paren[close + 1..].trim_start();
            }
        } else {
            s = rest;
        }
    }
    let s = s.strip_prefix("use ").unwrap_or(s).trim();
    if s.is_empty() {
        return;
    }
    if let (Some(open), Some(close)) = (s.find('{'), s.rfind('}')) {
        if open < close {
            let prefix = s[..open].trim().trim_end_matches("::");
            for item in s[open + 1..close].split(',') {
                let item = item.split(" as ").next().unwrap_or(item).trim();
                if item.is_empty() || item == "self" {
                    if !prefix.is_empty() {
                        out.push(prefix.to_string());
                    }
                } else if !prefix.is_empty() {
                    out.push(format!("{}::{}", prefix, item));
                } else {
                    out.push(item.to_string());
                }
            }
            return;
        }
    }
    let clean = s.split(" as ").next().unwrap_or(s).trim();
    if !clean.is_empty() {
        out.push(clean.to_string());
    }
}

fn first_line_compact(s: &str, max_bytes: usize) -> String {
    let first = s.lines().next().unwrap_or("").trim();
    let collapsed = first.split_whitespace().collect::<Vec<_>>().join(" ");
    let (cut, truncated) = crate::config::truncate_utf8_bytes(&collapsed, max_bytes);
    if truncated {
        format!("{}…", cut)
    } else {
        cut.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_lifetimes_and_raw_strings_never_break_ast_outliner() {
        let src = "\u{feff}pub struct Borrowed<'a> {\n    data: &'a str,\n}\n\nimpl<'a> Borrowed<'a> {\n    pub fn get(&self) -> &'a str {\n        let _quote = r#\"'unbalanced\"#;\n        self.data\n    }\n}\n";
        let out = outline_source(Path::new("lib.rs"), src);
        assert_eq!(out.parser_engine, "tree-sitter");
        assert!(out.has_bom);
        assert!(out.syntax_errors.is_empty());
        assert!(out.symbols.iter().any(|s| s.name == "Borrowed" && s.kind == "struct"));
        assert!(out.symbols.iter().any(|s| s.name == "get" && s.end_line == 9));
    }

    #[test]
    fn go_raw_string_ending_with_backslash_parses_cleanly() {
        let src = "package main\n\nfunc Backslash() string {\n\treturn `\\`\n}\n\nfunc NextFunc() int {\n\treturn 42\n}\n";
        let out = outline_source(Path::new("main.go"), src);
        assert!(out.syntax_errors.is_empty());
        let names: Vec<&str> = out.symbols.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["Backslash", "NextFunc"]);
    }

    #[test]
    fn ts_template_literals_with_braces_and_syntax_error_detection() {
        let ok_src = "import { auth } from './auth';\nexport class Service {\n  render(x: string) {\n    return `hello ${x} } {`;\n  }\n}\n";
        let out = outline_source(Path::new("svc.ts"), ok_src);
        assert!(out.syntax_errors.is_empty());
        assert_eq!(out.imports, vec!["./auth"]);
        assert!(out.symbols.iter().any(|s| s.qualified_name == "Service::render"));

        let broken = "export function oops(x: number {\n  return x + ;\n}\n";
        let errs = check_syntax(Path::new("broken.ts"), broken);
        assert!(!errs.is_empty());
    }
}
