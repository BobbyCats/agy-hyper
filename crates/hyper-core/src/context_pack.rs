use crate::ast::{outline_source, FileOutline, SymbolInfo};
use crate::config::truncate_utf8_bytes;
use crate::git::{inspect_repo, quick_head};
use crate::jail::WorkspaceJail;
use crate::runner::discover_presets;
use crate::search::{glob_files, search_multi_terms};
use crate::wal::blake3_etag;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

const FULL_SLICE_MAX_BYTES: usize = 2_400;
const MAX_FULL_SLICES: usize = 4;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextPackRequest {
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub focus_paths: Vec<String>,
    #[serde(default)]
    pub symbols: Vec<String>,
    #[serde(default = "default_true")]
    pub include_outlines: bool,
    #[serde(default = "default_true")]
    pub include_git: bool,
    /// Hard UTF-8 byte budget (clamped to <= 8,500 bytes to stay strictly inline in Antigravity).
    #[serde(default = "default_pack_bytes")]
    pub max_bytes: usize,
}

impl Default for ContextPackRequest {
    fn default() -> Self {
        Self {
            query: None,
            focus_paths: Vec::new(),
            symbols: Vec::new(),
            include_outlines: true,
            include_git: true,
            max_bytes: default_pack_bytes(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_pack_bytes() -> usize {
    8_200
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PackedFileCard {
    pub path: String,
    pub score: i32,
    pub reason: String,
    pub language: String,
    pub total_lines: usize,
    pub etag: String,
    #[serde(default)]
    pub mtime_ns: u128,
    #[serde(default)]
    pub byte_len: u64,
    pub imports: Vec<String>,
    pub symbols: Vec<String>,
    pub matched_lines: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextPackResult {
    pub workspace: String,
    pub branch: Option<String>,
    pub head_oid: Option<String>,
    pub dirty_files: Vec<String>,
    pub discovered_presets: Vec<String>,
    pub files: Vec<PackedFileCard>,
    pub budget_bytes: usize,
    pub used_bytes: usize,
    pub truncated: bool,
    pub compact_markdown: String,
}

fn file_mtime_and_len(abs: &Path) -> Option<(u128, u64)> {
    let meta = fs::metadata(abs).ok()?;
    let byte_len = meta.len();
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Some((mtime_ns, byte_len))
}

/// Normalize free-form pack queries into a canonical cache key token string so reordered or
/// differently punctuated queries (`"foo bar"` vs `"bar, foo!"`) share a single cache entry.
pub fn normalize_pack_query_key(query: Option<&str>) -> String {
    let Some(q) = query else {
        return String::new();
    };
    let mut tokens: Vec<String> = q
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect();
    tokens.sort();
    tokens.dedup();
    tokens.join(" ")
}

/// Fine-grained pack freshness check: verifies every packed file still exists on disk with
/// identical `(mtime_ns, byte_len)` and that Git `HEAD` OID matches when `pack.head_oid` is set.
pub fn is_pack_fresh(jail: &WorkspaceJail, pack: &ContextPackResult) -> bool {
    if let Some(expected_oid) = pack.head_oid.as_deref() {
        match quick_head(jail.root()) {
            Some((_, current_oid)) if current_oid == expected_oid => {}
            _ => return false,
        }
    }
    for card in &pack.files {
        let Ok(abs) = jail.resolve_existing(&card.path) else {
            return false;
        };
        if !abs.is_file() {
            return false;
        }
        let Some((mtime_ns, byte_len)) = file_mtime_and_len(&abs) else {
            return false;
        };
        if mtime_ns != card.mtime_ns || byte_len != card.byte_len {
            return false;
        }
    }
    true
}

#[derive(Debug, Clone)]
struct CachedAstOutline {
    mtime_ns: u128,
    byte_len: u64,
    last_used: u64,
    etag: String,
    outline: FileOutline,
}

/// True LRU in-memory AST cache keyed by `(abs_path, mtime_ns, byte_len)`.
#[derive(Debug, Default)]
pub struct AstOutlineCache {
    entries: HashMap<PathBuf, CachedAstOutline>,
    access_seq: u64,
    pub hits: u64,
    pub misses: u64,
}

impl AstOutlineCache {
    pub fn get_or_parse(&mut self, abs: &Path) -> Option<(FileOutline, String)> {
        self.get_or_parse_with_meta(abs)
            .map(|(outline, etag, _, _)| (outline, etag))
    }

    fn get_or_parse_with_meta(&mut self, abs: &Path) -> Option<(FileOutline, String, u128, u64)> {
        let (mtime_ns, byte_len) = file_mtime_and_len(abs)?;
        if byte_len > 512 * 1024 {
            return None;
        }

        self.access_seq += 1;
        let seq = self.access_seq;
        if let Some(hit) = self.entries.get_mut(abs) {
            if hit.mtime_ns == mtime_ns && hit.byte_len == byte_len {
                hit.last_used = seq;
                self.hits += 1;
                return Some((hit.outline.clone(), hit.etag.clone(), mtime_ns, byte_len));
            }
        }

        self.misses += 1;
        let raw_bytes = fs::read(abs).ok()?;
        let text = String::from_utf8(raw_bytes.clone()).ok()?;
        let etag = blake3_etag(&raw_bytes);
        let outline = outline_source(abs, &text);

        if self.entries.len() >= 2048 {
            if let Some(lru_key) = self
                .entries
                .iter()
                .min_by_key(|(_, v)| v.last_used)
                .map(|(k, _)| k.clone())
            {
                self.entries.remove(&lru_key);
            }
        }
        self.entries.insert(
            abs.to_path_buf(),
            CachedAstOutline {
                mtime_ns,
                byte_len,
                last_used: seq,
                etag: etag.clone(),
                outline: outline.clone(),
            },
        );
        Some((outline, etag, mtime_ns, byte_len))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

type FileScoreEntry = (i32, Vec<String>, Vec<String>, Vec<usize>);
type RankedFileEntry = (String, i32, Vec<String>, Vec<String>, Vec<usize>);

pub fn build_context_pack(
    jail: &WorkspaceJail,
    req: &ContextPackRequest,
    block_secrets: bool,
) -> Result<ContextPackResult> {
    build_context_pack_with_cache(jail, req, block_secrets, None)
}

pub fn build_context_pack_with_cache(
    jail: &WorkspaceJail,
    req: &ContextPackRequest,
    block_secrets: bool,
    mut ast_cache: Option<&mut AstOutlineCache>,
) -> Result<ContextPackResult> {
    let budget = req.max_bytes.clamp(1_500, 8_500);

    let git_info = req
        .include_git
        .then(|| inspect_repo(jail, 6, 20, true));
    let presets: Vec<String> = discover_presets(jail.root())
        .into_iter()
        .map(|p| p.id)
        .collect();

    let requested_symbols: Vec<String> = req
        .symbols
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    let query_is_empty = req
        .query
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty();

    let mut scores: HashMap<String, FileScoreEntry> = HashMap::new();
    let mut resolved_focus_files: HashSet<String> = HashSet::new();

    for fp in &req.focus_paths {
        if let Ok(resolved) = jail.resolve_existing(fp) {
            if resolved.is_file() {
                let rel = jail.relative_display(&resolved);
                resolved_focus_files.insert(rel.clone());
                add_score(&mut scores, rel, 100, "focus", None, None);
            } else if resolved.is_dir() {
                if let Ok(globbed) = glob_files(jail, None, Some(fp), 24, block_secrets) {
                    for f in globbed.files {
                        resolved_focus_files.insert(f.clone());
                        add_score(&mut scores, f, 35, "focus-dir", None, None);
                    }
                }
            }
        }
    }

    if let Some(g) = &git_info {
        for df in &g.dirty_files {
            if jail.resolve_existing(df).is_ok() {
                add_score(&mut scores, df.clone(), 28, "git-dirty", None, None);
            }
        }
        for (hf, churn) in g.hotspot_files.iter().take(10) {
            if jail.resolve_existing(hf).is_ok() {
                add_score(&mut scores, hf.clone(), (*churn as i32 * 4).clamp(4, 24), "hotspot", None, None);
            }
        }
    }

    // When `req.symbols` is supplied, also search leaf symbol identifiers across the workspace
    // so callers passing `symbols=["my_fn"]` without `focus_paths` still rank the defining file first.
    if !requested_symbols.is_empty() {
        let mut sym_terms: Vec<String> = requested_symbols
            .iter()
            .filter_map(|s| {
                let leaf = s
                    .rsplit([':', '.'])
                    .next()
                    .unwrap_or(s)
                    .trim();
                (!leaf.is_empty()).then(|| leaf.to_string())
            })
            .collect();
        sym_terms.sort();
        sym_terms.dedup();
        sym_terms.truncate(6);
        if !sym_terms.is_empty() {
            let sr = search_multi_terms(jail, &sym_terms, 120, block_secrets);
            let mut per_file_hits: HashMap<String, usize> = HashMap::new();
            for m in sr.matches {
                let c = per_file_hits.entry(m.path.clone()).or_insert(0);
                *c += 1;
                if *c <= 3 {
                    let hit = format!("L{}: {}", m.line, m.text.trim());
                    add_score(&mut scores, m.path, 55, "symbol", Some(hit), Some(m.line));
                } else {
                    add_score(&mut scores, m.path, 6, "symbol", None, None);
                }
            }
        }
    }

    // Single-pass parallel search across all query terms (1 directory walk instead of 5).
    if let Some(q) = &req.query {
        let terms: Vec<String> = extract_query_terms(q).into_iter().take(5).collect();
        if !terms.is_empty() {
            let sr = search_multi_terms(jail, &terms, 120, block_secrets);
            let mut per_file_hits: HashMap<String, usize> = HashMap::new();
            for m in sr.matches {
                let c = per_file_hits.entry(m.path.clone()).or_insert(0);
                *c += 1;
                if *c <= 3 {
                    let hit = format!("L{}: {}", m.line, m.text.trim());
                    add_score(&mut scores, m.path, 22, "search", Some(hit), Some(m.line));
                } else {
                    add_score(&mut scores, m.path, 4, "search", None, None);
                }
            }
        }
    }

    // Fallback when no query/focus/dirty hits: prioritize root manifests & entrypoints cleanly.
    if scores.is_empty() {
        if let Ok(globbed) = glob_files(jail, None, None, 120, block_secrets) {
            for f in globbed.files {
                let pts = entry_file_priority(&f);
                if pts > 0 {
                    add_score(&mut scores, f, pts, "entry", None, None);
                }
            }
        }
    }

    let mut ranked: Vec<RankedFileEntry> = scores
        .into_iter()
        .map(|(p, (sc, reasons, hits, hit_lines))| (p, sc, reasons, hits, hit_lines))
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    // Parse top 10 ranked files & collect 1-hop import neighbors.
    let mut outlines: HashMap<String, (FileOutline, String, u128, u64)> = HashMap::new();
    let mut neighbor_boosts: Vec<String> = Vec::new();
    let mut seen_paths: HashSet<String> = ranked.iter().take(10).map(|r| r.0.clone()).collect();

    for (rel, _, _, _, _) in ranked.iter().take(10) {
        if let Ok(abs) = jail.resolve_existing(rel) {
            let parsed = if let Some(cache) = ast_cache.as_deref_mut() {
                cache.get_or_parse_with_meta(&abs)
            } else {
                parse_file_uncached(&abs)
            };
            if let Some((outline, etag, mtime_ns, byte_len)) = parsed {
                for imp in &outline.imports {
                    if let Some(target_rel) = resolve_local_import(jail, &abs, imp) {
                        if seen_paths.insert(target_rel.clone()) {
                            neighbor_boosts.push(target_rel);
                        }
                    }
                }
                if outline.language == "go" {
                    collect_go_siblings(jail, &abs, &mut seen_paths, &mut neighbor_boosts);
                }
                outlines.insert(rel.clone(), (outline, etag, mtime_ns, byte_len));
            }
        }
    }

    // Keep top 10 primary candidates + up to 4 parsed 1-hop import neighbors (never dropped by >16 match lists!).
    ranked.truncate(10);
    for nb in neighbor_boosts.into_iter().take(4) {
        if let Ok(abs) = jail.resolve_existing(&nb) {
            let parsed = if let Some(cache) = ast_cache.as_deref_mut() {
                cache.get_or_parse_with_meta(&abs)
            } else {
                parse_file_uncached(&abs)
            };
            if let Some((outline, etag, mtime_ns, byte_len)) = parsed {
                outlines.insert(nb.clone(), (outline, etag, mtime_ns, byte_len));
                ranked.push((nb, 18, vec!["import-neighbor".into()], Vec::new(), Vec::new()));
            }
        }
    }

    // Boost files that contain an exact AST symbol match for `req.symbols` so their bodies slice first.
    if !requested_symbols.is_empty() {
        for (rel, score, reasons, _, _) in &mut ranked {
            if let Some((outline, _, _, _)) = outlines.get(rel) {
                let ast_matched = outline.symbols.iter().any(|sym| {
                    requested_symbols
                        .iter()
                        .any(|q| symbol_matches_request(sym, q))
                });
                if ast_matched {
                    *score += 90;
                    if !reasons.iter().any(|r| r == "symbol-ast") {
                        reasons.push("symbol-ast".into());
                    }
                }
            }
        }
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    }

    let mut md = String::with_capacity(budget.min(8_500));
    let branch = git_info.as_ref().and_then(|g| g.branch.clone());
    let head_oid = git_info.as_ref().and_then(|g| g.head_oid.clone());
    let dirty_files = git_info
        .as_ref()
        .map(|g| g.dirty_files.clone())
        .unwrap_or_default();

    let ws_name = jail
        .root()
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("workspace");
    if let Some(b) = branch.as_deref() {
        md.push_str(&format!("# hyper_pack · {} ({}@{})\n", ws_name, b, head_oid.as_deref().unwrap_or("-")));
    } else {
        md.push_str(&format!("# hyper_pack · {}\n", ws_name));
    }
    if !presets.is_empty() {
        md.push_str(&format!("presets: {}\n", presets.join(", ")));
    }
    if !dirty_files.is_empty() {
        let dirty_preview: Vec<&str> = dirty_files.iter().take(8).map(String::as_str).collect();
        md.push_str(&format!("dirty({}): {}\n", dirty_files.len(), dirty_preview.join(", ")));
    }
    if md.len() > budget {
        let (cut_hdr, _) = truncate_utf8_bytes(&md, budget);
        md = cut_hdr.to_string();
    }

    let has_requested_symbols = !requested_symbols.is_empty();
    let focused_small_mode =
        !has_requested_symbols && query_is_empty && (1..=2).contains(&resolved_focus_files.len());
    let full_slice_mode = has_requested_symbols || focused_small_mode;

    let mut cards: Vec<PackedFileCard> = Vec::new();
    let mut truncated = false;
    let mut snippets_inlined = 0usize;

    for (rel, score, reasons, hits, hit_lines) in ranked {
        let Some((outline, etag, mtime_ns, byte_len)) = outlines.remove(&rel) else {
            continue;
        };

        let mut sym_lines: Vec<String> = Vec::new();
        if req.include_outlines {
            for s in outline.symbols.iter().take(18) {
                sym_lines.push(format!("  L{}-{} {} {}", s.start_line, s.end_line, s.kind, s.qualified_name));
            }
            if outline.symbols.len() > 18 {
                sym_lines.push(format!("  … +{} more symbols", outline.symbols.len() - 18));
            }
        }

        let mut section = format!(
            "\n## {} [{} · {}L · etag:{} · {}]\n",
            rel, outline.language, outline.total_lines, etag, reasons.join("+")
        );
        if !outline.imports.is_empty() {
            let imps: Vec<&str> = outline.imports.iter().take(6).map(String::as_str).collect();
            section.push_str(&format!("imports: {}\n", imps.join(", ")));
        }
        for h in &hits {
            let (hc, _) = truncate_utf8_bytes(h, 150);
            section.push_str(&format!("  hit {}\n", hc));
        }
        for sl in &sym_lines {
            section.push_str(sl);
            section.push('\n');
        }
        if !outline.syntax_errors.is_empty() {
            section.push_str(&format!("  ⚠ {} syntax error(s)\n", outline.syntax_errors.len()));
        }

        let mut snippet: Option<String> = None;
        let remaining_for_snippets = budget.saturating_sub(md.len().saturating_add(section.len()).saturating_add(16));

        if full_slice_mode && snippets_inlined < MAX_FULL_SLICES && remaining_for_snippets >= 96 {
            if let Ok(abs) = jail.resolve_existing(&rel) {
                let per_file_cap = if has_requested_symbols {
                    MAX_FULL_SLICES - snippets_inlined
                } else if resolved_focus_files.contains(&rel) {
                    let per_focus_limit = if resolved_focus_files.len() == 2 { 2 } else { MAX_FULL_SLICES };
                    per_focus_limit.min(MAX_FULL_SLICES - snippets_inlined)
                } else {
                    0
                };
                if per_file_cap > 0 {
                    let (slices, count) = extract_full_symbol_slices(
                        &abs,
                        &outline,
                        &requested_symbols,
                        per_file_cap,
                        remaining_for_snippets,
                    );
                    if count > 0 && !slices.is_empty() {
                        snippet = Some(slices);
                        snippets_inlined += count;
                    }
                }
            }
        } else if !full_slice_mode
            && req.include_outlines
            && snippets_inlined < 2
            && (!hit_lines.is_empty() || reasons.iter().any(|r| r == "focus"))
            && remaining_for_snippets >= 80
        {
            if let Ok(abs) = jail.resolve_existing(&rel) {
                let cap = 520usize.min(remaining_for_snippets.saturating_sub(24));
                if cap >= 48 {
                    if let Some(snip) = extract_matched_symbol_snippet(&abs, &outline, &hit_lines, cap) {
                        snippet = Some(snip);
                        snippets_inlined += 1;
                    }
                }
            }
        }

        if let Some(snip) = &snippet {
            let mut code_block = String::with_capacity(snip.len() + 10);
            code_block.push_str("```\n");
            code_block.push_str(snip);
            if !snip.ends_with('\n') {
                code_block.push('\n');
            }
            code_block.push_str("```\n");
            if md.len() + section.len() + code_block.len() <= budget {
                section.push_str(&code_block);
            } else {
                snippet = None;
                truncated = true;
            }
        }

        if md.len() + section.len() > budget {
            truncated = true;
            let stub = format!("\n## {} [{}L · etag:{} · score:{}]\n", rel, outline.total_lines, etag, score);
            if md.len() + stub.len() <= budget {
                md.push_str(&stub);
            } else {
                break;
            }
        } else {
            md.push_str(&section);
        }

        cards.push(PackedFileCard {
            path: rel,
            score,
            reason: reasons.join("+"),
            language: outline.language,
            total_lines: outline.total_lines,
            etag,
            mtime_ns,
            byte_len,
            imports: outline.imports.into_iter().take(8).collect(),
            symbols: sym_lines,
            matched_lines: hits,
            snippet,
        });
    }

    let used_bytes = md.len();
    Ok(ContextPackResult {
        workspace: jail.root().display().to_string(),
        branch,
        head_oid,
        dirty_files,
        discovered_presets: presets,
        files: cards,
        budget_bytes: budget,
        used_bytes,
        truncated,
        compact_markdown: md,
    })
}

fn symbol_matches_request(sym: &SymbolInfo, req_sym: &str) -> bool {
    let q = req_sym.trim();
    if q.is_empty() {
        return false;
    }
    sym.name.eq_ignore_ascii_case(q)
        || sym.qualified_name.eq_ignore_ascii_case(q)
        || sym
            .qualified_name
            .replace("::", ".")
            .eq_ignore_ascii_case(&q.replace("::", "."))
}

fn format_annotated_symbol_slice(raw: &[u8], sym: &SymbolInfo, max_body_bytes: usize) -> Option<String> {
    if sym.end_byte > raw.len() || sym.start_byte >= sym.end_byte || max_body_bytes < 32 {
        return None;
    }
    let slice = std::str::from_utf8(&raw[sym.start_byte..sym.end_byte]).ok()?;
    let trimmed = slice.trim_end();
    let (cut, was_cut) = truncate_utf8_bytes(trimmed, max_body_bytes);
    if was_cut {
        let shown_lines = cut.split('\n').count().max(1);
        let shown_end = (sym.start_line + shown_lines - 1).min(sym.end_line);
        Some(format!(
            "// L{}-{} (of L{}-{}) {} {}\n{}…",
            sym.start_line, shown_end, sym.start_line, sym.end_line, sym.kind, sym.qualified_name, cut
        ))
    } else {
        Some(format!(
            "// L{}-{} {} {}\n{}",
            sym.start_line, sym.end_line, sym.kind, sym.qualified_name, cut
        ))
    }
}

fn extract_full_symbol_slices(
    abs: &Path,
    outline: &FileOutline,
    requested_symbols: &[String],
    max_slices: usize,
    available_bytes: usize,
) -> (String, usize) {
    let Ok(raw) = fs::read(abs) else {
        return (String::new(), 0);
    };
    let mut candidates: Vec<&SymbolInfo> = Vec::new();
    let mut seen_ranges: Vec<(usize, usize)> = Vec::new();

    if !requested_symbols.is_empty() {
        for sym in &outline.symbols {
            if sym.end_byte <= sym.start_byte {
                continue;
            }
            if requested_symbols
                .iter()
                .any(|q| symbol_matches_request(sym, q))
                && !seen_ranges.contains(&(sym.start_byte, sym.end_byte))
            {
                seen_ranges.push((sym.start_byte, sym.end_byte));
                candidates.push(sym);
                if candidates.len() >= max_slices {
                    break;
                }
            }
        }
    } else {
        // Focused <=2-file mode with empty query: prefer top-level symbols (`depth == 0`) first,
        // avoiding redundant nested slices already covered by an enclosing symbol slice.
        for sym in outline.symbols.iter().filter(|s| s.depth == 0 && s.end_byte > s.start_byte) {
            seen_ranges.push((sym.start_byte, sym.end_byte));
            candidates.push(sym);
            if candidates.len() >= max_slices {
                break;
            }
        }
        if candidates.is_empty() {
            for sym in outline.symbols.iter().filter(|s| s.end_byte > s.start_byte) {
                if !seen_ranges
                    .iter()
                    .any(|&(sb, eb)| sb <= sym.start_byte && sym.end_byte <= eb)
                {
                    seen_ranges.push((sym.start_byte, sym.end_byte));
                    candidates.push(sym);
                    if candidates.len() >= max_slices {
                        break;
                    }
                }
            }
        }
    }

    let mut blocks: Vec<String> = Vec::new();
    let mut used = 0usize;

    if candidates.is_empty() && requested_symbols.is_empty() && max_slices > 0 {
        if let Ok(text) = std::str::from_utf8(&raw) {
            let trimmed = text.trim_end();
            if !trimmed.is_empty() {
                let body_cap = FULL_SLICE_MAX_BYTES.min(available_bytes.saturating_sub(48));
                if body_cap >= 48 {
                    let (cut, was_cut) = truncate_utf8_bytes(trimmed, body_cap);
                    let shown_end = cut.split('\n').count().max(1);
                    let block = if was_cut {
                        format!("// L1-{} (of L1-{})\n{}…", shown_end, outline.total_lines.max(shown_end), cut)
                    } else {
                        format!("// L1-{}\n{}", outline.total_lines.max(shown_end), cut)
                    };
                    if block.len() <= available_bytes {
                        return (block, 1);
                    }
                }
            }
        }
        return (String::new(), 0);
    }

    for sym in candidates {
        let sep_len = if blocks.is_empty() { 0 } else { 2 };
        let rem = available_bytes.saturating_sub(used + sep_len);
        let body_cap = FULL_SLICE_MAX_BYTES.min(rem.saturating_sub(72));
        if body_cap < 48 {
            break;
        }
        if let Some(formatted) = format_annotated_symbol_slice(&raw, sym, body_cap) {
            if used + sep_len + formatted.len() <= available_bytes {
                used += sep_len + formatted.len();
                blocks.push(formatted);
            }
        }
    }

    let count = blocks.len();
    (blocks.join("\n\n"), count)
}

fn extract_matched_symbol_snippet(
    abs: &Path,
    outline: &FileOutline,
    hit_lines: &[usize],
    max_bytes: usize,
) -> Option<String> {
    let target = hit_lines
        .iter()
        .find_map(|&ln| {
            outline
                .symbols
                .iter()
                .filter(|s| s.start_line <= ln && ln <= s.end_line && s.end_byte > s.start_byte)
                .min_by_key(|s| s.end_line.saturating_sub(s.start_line))
        })
        .or_else(|| outline.symbols.iter().find(|s| s.end_byte > s.start_byte))?;

    let raw = fs::read(abs).ok()?;
    format_annotated_symbol_slice(&raw, target, max_bytes)
}

fn collect_go_siblings(
    jail: &WorkspaceJail,
    abs: &Path,
    seen: &mut HashSet<String>,
    out: &mut Vec<String>,
) {
    let Some(parent) = abs.parent() else { return };
    let Ok(entries) = fs::read_dir(parent) else { return };
    for ent in entries.flatten().take(4) {
        let p = ent.path();
        if p.is_file()
            && p != abs
            && p.extension().and_then(|s| s.to_str()) == Some("go")
            && !p.file_name().and_then(|s| s.to_str()).unwrap_or("").ends_with("_test.go")
        {
            if let Ok(res) = jail.resolve_existing(&p) {
                let rel = jail.relative_display(&res);
                if seen.insert(rel.clone()) {
                    out.push(rel);
                }
            }
        }
    }
}

fn add_score(
    scores: &mut HashMap<String, FileScoreEntry>,
    path: String,
    pts: i32,
    reason: &str,
    line_hit: Option<String>,
    line_num: Option<usize>,
) {
    let entry = scores
        .entry(path)
        .or_insert_with(|| (0, Vec::new(), Vec::new(), Vec::new()));
    entry.0 += pts;
    if !entry.1.iter().any(|r| r == reason) {
        entry.1.push(reason.to_string());
    }
    if let Some(lh) = line_hit {
        if entry.2.len() < 5 && !entry.2.contains(&lh) {
            entry.2.push(lh);
        }
    }
    if let Some(ln) = line_num {
        if entry.3.len() < 5 && !entry.3.contains(&ln) {
            entry.3.push(ln);
        }
    }
}

fn parse_file_uncached(abs: &Path) -> Option<(FileOutline, String, u128, u64)> {
    let (mtime_ns, byte_len) = file_mtime_and_len(abs)?;
    if byte_len > 512 * 1024 {
        return None;
    }
    let raw_bytes = fs::read(abs).ok()?;
    let text = String::from_utf8(raw_bytes.clone()).ok()?;
    let etag = blake3_etag(&raw_bytes);
    Some((outline_source(abs, &text), etag, mtime_ns, byte_len))
}

fn extract_query_terms(q: &str) -> Vec<String> {
    const STOP: &[&str] = &[
        "the", "and", "for", "with", "from", "that", "this", "into", "where", "how", "what",
        "file", "code", "fix", "add", "check", "update", "find", "in", "on", "of", "to", "is",
        "are", "be",
    ];
    let raw_tokens: Vec<String> = q
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-' && c != ':' && c != '.')
        .map(|s| s.trim_matches('.'))
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();

    let mut terms: Vec<String> = raw_tokens
        .iter()
        .filter(|s| s.chars().count() >= 2 && !STOP.contains(&s.to_ascii_lowercase().as_str()))
        .cloned()
        .collect();

    if terms.is_empty() {
        terms = raw_tokens;
    }
    terms.sort_by_key(|b| std::cmp::Reverse(b.len()));
    terms.dedup();
    terms
}

fn entry_file_priority(rel: &str) -> i32 {
    let lower = rel.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "package.json" | "cargo.toml" | "pyproject.toml" | "go.mod" | "readme.md" | "agents.md"
    ) {
        return 30;
    }
    if lower.ends_with("/main.rs")
        || lower.ends_with("/lib.rs")
        || lower.ends_with("/index.ts")
        || lower.ends_with("/index.tsx")
        || lower.ends_with("/app.tsx")
        || lower.ends_with("/main.py")
        || lower.ends_with("/main.go")
    {
        return 25;
    }
    if lower.starts_with("src/") || lower.starts_with("crates/") || lower.starts_with("app/") {
        return 10;
    }
    2
}

/// Append an extension safely without clobbering dotted filenames like `user.service` -> `user.ts`.
fn append_ext(base: &Path, ext: &str) -> PathBuf {
    let mut os: OsString = base.as_os_str().to_os_string();
    os.push(".");
    os.push(ext);
    PathBuf::from(os)
}

/// Multi-language 1-hop local import resolver:
/// Supports JS/TS (`./`, `../`, `@/`, dotted stems, `.js` -> `.ts` ESM), Rust (`crate::`, `super::`, `self::`),
/// and Python (`.mod`, `..mod`, local dotted modules).
fn resolve_local_import(jail: &WorkspaceJail, from_abs: &Path, imp: &str) -> Option<String> {
    let trimmed = imp.trim();
    let parent = from_abs.parent()?;

    // 1. JS / TS / MJS relative & `@/` alias imports
    if trimmed.starts_with("./") || trimmed.starts_with("../") || trimmed.starts_with("@/") {
        let base = if let Some(rest) = trimmed.strip_prefix("@/") {
            jail.root().join("src").join(rest)
        } else {
            parent.join(trimmed)
        };
        let mut candidates = vec![
            base.clone(),
            append_ext(&base, "ts"),
            append_ext(&base, "tsx"),
            append_ext(&base, "js"),
            append_ext(&base, "mjs"),
            base.join("index.ts"),
            base.join("index.tsx"),
            base.join("index.js"),
        ];
        // ESM TypeScript imports often write `./foo.js` for `./foo.ts`
        if base.extension().and_then(|e| e.to_str()) == Some("js") {
            let stem_path = base.with_extension("");
            candidates.push(append_ext(&stem_path, "ts"));
            candidates.push(append_ext(&stem_path, "tsx"));
        }
        for c in candidates {
            if c.is_file() {
                if let Ok(resolved) = jail.resolve_existing(&c) {
                    return Some(jail.relative_display(&resolved));
                }
            }
        }
        return None;
    }

    // 2. Rust (`crate::foo::bar`, `super::baz`, `self::qux`)
    if trimmed.starts_with("crate::")
        || trimmed.starts_with("super::")
        || trimmed.starts_with("self::")
    {
        let clean = trimmed.split('{').next().unwrap_or(trimmed).trim_end_matches("::");
        let parts: Vec<&str> = clean.split("::").filter(|s| !s.is_empty()).collect();
        if parts.len() >= 2 {
            let is_mod_root = matches!(
                from_abs.file_name().and_then(|s| s.to_str()),
                Some("mod.rs" | "lib.rs" | "main.rs")
            );
            let base_dir = match parts[0] {
                "crate" => find_rust_src_root(from_abs, jail.root())?,
                // For `src/foo.rs`, `super::bar` lives in `src/bar.rs` (same `parent`).
                // Only `src/foo/mod.rs` climbs to `parent.parent()` (`src/`).
                "super" if is_mod_root => parent.parent()?.to_path_buf(),
                "super" | "self" => parent.to_path_buf(),
                _ => return None,
            };
            for depth in (1..parts.len()).rev() {
                let mut p = base_dir.clone();
                for seg in &parts[1..=depth] {
                    p = p.join(seg);
                }
                for cand in [append_ext(&p, "rs"), p.join("mod.rs")] {
                    if cand.is_file() {
                        if let Ok(resolved) = jail.resolve_existing(&cand) {
                            return Some(jail.relative_display(&resolved));
                        }
                    }
                }
            }
        }
        return None;
    }

    // 3. Python relative & local dotted imports (`.utils`, `..core.auth`, `app.services.auth`)
    if from_abs.extension().and_then(|s| s.to_str()) == Some("py") {
        let dots = trimmed.bytes().take_while(|&b| b == b'.').count();
        let rest = &trimmed[dots..];
        let mut base = if dots > 0 {
            let mut b = parent.to_path_buf();
            for _ in 1..dots {
                b = b.parent()?.to_path_buf();
            }
            b
        } else {
            jail.root().to_path_buf()
        };
        for seg in rest.split('.').filter(|s| !s.is_empty()) {
            base = base.join(seg);
        }
        for cand in [append_ext(&base, "py"), base.join("__init__.py")] {
            if cand.is_file() {
                if let Ok(resolved) = jail.resolve_existing(&cand) {
                    return Some(jail.relative_display(&resolved));
                }
            }
        }
    }

    None
}

fn find_rust_src_root(from_abs: &Path, ws_root: &Path) -> Option<PathBuf> {
    let mut cur = from_abs.parent()?;
    while cur.starts_with(ws_root) {
        if cur.file_name().and_then(|s| s.to_str()) == Some("src") {
            return Some(cur.to_path_buf());
        }
        cur = cur.parent()?;
    }
    let fallback = ws_root.join("src");
    fallback.is_dir().then_some(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn resolves_rust_python_ts_1hop_imports_and_preserves_neighbors_past_16_files() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::create_dir_all(dir.path().join("pyapp/core")).unwrap();
        fs::create_dir_all(dir.path().join("web")).unwrap();

        // Rust: `pub(crate) use crate::{auth, cfg};` and `super::helper` from sibling `.rs`
        fs::write(
            dir.path().join("src/main.rs"),
            "pub(crate) use crate::{auth, helper};\npub fn add(a: i32) -> i32 { a + 1 }\n",
        )
        .unwrap();
        fs::write(dir.path().join("src/auth.rs"), "use super::helper;\npub fn verify() -> bool { true }\n").unwrap();
        fs::write(dir.path().join("src/helper.rs"), "pub fn help() {}\n").unwrap();

        // Python: relative `from .utils import sign`
        fs::write(dir.path().join("pyapp/core/handler.py"), "from .utils import sign\ndef py_entry():\n    return sign()\n").unwrap();
        fs::write(dir.path().join("pyapp/core/utils.py"), "def sign():\n    return 'ok'\n").unwrap();

        // TypeScript: dotted filename `./user.service`
        fs::write(dir.path().join("web/ctrl.ts"), "import { load } from './user.service';\nexport function ts_entry() { return load(); }\n").unwrap();
        fs::write(dir.path().join("web/user.service.ts"), "export function load() { return 1; }\n").unwrap();

        // Add 18 noise files matching `add` to prove 1-hop neighbors are NEVER truncated!
        for i in 0..18 {
            fs::write(dir.path().join(format!("noise_{:02}.txt", i)), "add\n").unwrap();
        }

        let jail = WorkspaceJail::new(dir.path(), false, true).unwrap();
        let mut cache = AstOutlineCache::default();

        // 1. Rust + >16 match neighbor retention + inline symbol snippet
        let pack_rs = build_context_pack_with_cache(
            &jail,
            &ContextPackRequest {
                query: Some("add".into()),
                focus_paths: vec!["src/main.rs".into()],
                symbols: vec![],
                include_outlines: true,
                include_git: false,
                max_bytes: 8200,
            },
            true,
            Some(&mut cache),
        )
        .unwrap();
        let rs_paths: Vec<&str> = pack_rs.files.iter().map(|f| f.path.as_str()).collect();
        assert!(rs_paths.contains(&"src/main.rs"));
        assert!(rs_paths.contains(&"src/auth.rs"), "Rust grouped pub(crate) use neighbor missing: {:?}", rs_paths);
        assert!(rs_paths.contains(&"src/helper.rs"), "Rust neighbor missing: {:?}", rs_paths);
        assert!(pack_rs.files[0].snippet.as_deref().unwrap_or("").contains("pub fn add"));

        // 2. Python 1-hop relative import
        let pack_py = build_context_pack_with_cache(
            &jail,
            &ContextPackRequest {
                query: Some("py_entry".into()),
                focus_paths: vec![],
                symbols: vec![],
                include_outlines: true,
                include_git: false,
                max_bytes: 8200,
            },
            true,
            Some(&mut cache),
        )
        .unwrap();
        let py_paths: Vec<&str> = pack_py.files.iter().map(|f| f.path.as_str()).collect();
        assert!(py_paths.contains(&"pyapp/core/handler.py"));
        assert!(py_paths.contains(&"pyapp/core/utils.py"), "Python 1-hop neighbor missing: {:?}", py_paths);

        // 3. TypeScript dotted filename 1-hop import
        let pack_ts = build_context_pack_with_cache(
            &jail,
            &ContextPackRequest {
                query: Some("ts_entry".into()),
                focus_paths: vec![],
                symbols: vec![],
                include_outlines: true,
                include_git: false,
                max_bytes: 8200,
            },
            true,
            Some(&mut cache),
        )
        .unwrap();
        let ts_paths: Vec<&str> = pack_ts.files.iter().map(|f| f.path.as_str()).collect();
        assert!(ts_paths.contains(&"web/ctrl.ts"));
        assert!(ts_paths.contains(&"web/user.service.ts"), "TS dotted 1-hop neighbor missing: {:?}", ts_paths);
        assert!(cache.hits > 0 || cache.misses >= 6);
    }

    #[test]
    fn symbol_sliced_pack_enforces_budget_line_ranges_and_freshness_checks() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();

        let body_a: String = (0..30)
            .map(|i| format!("    let step_{} = input.saturating_add({});\n", i, i))
            .collect();
        let rust_src = format!(
            "pub struct TokenGuard;\n\nimpl TokenGuard {{\n    pub fn validate_session(input: u64) -> u64 {{\n{}        input\n    }}\n}}\n\npub fn compute_checksum(seed: u64) -> u64 {{\n    seed ^ 0x5A5A\n}}\n",
            body_a
        );
        fs::write(dir.path().join("src/guard.rs"), &rust_src).unwrap();
        fs::write(
            dir.path().join("src/other.rs"),
            "pub fn helper_fn(x: i32) -> i32 {\n    x * 2\n}\n",
        )
        .unwrap();

        let jail = WorkspaceJail::new(dir.path(), false, true).unwrap();

        // 1. Explicit `symbols` matching case-insensitively on qualified_name & name without focus_paths
        let pack_sym = build_context_pack(
            &jail,
            &ContextPackRequest {
                symbols: vec!["tokenguard::validate_session".into(), "COMPUTE_CHECKSUM".into()],
                include_git: false,
                max_bytes: 99_999, // clamped strictly to <= 8,500 bytes
                ..Default::default()
            },
            true,
        )
        .unwrap();

        assert!(pack_sym.budget_bytes <= 8_500);
        assert!(pack_sym.used_bytes <= 8_500);
        assert_eq!(pack_sym.files[0].path, "src/guard.rs");
        assert!(pack_sym.files[0].byte_len > 0);
        assert!(pack_sym.files[0].mtime_ns > 0);
        let snip = pack_sym.files[0].snippet.as_deref().expect("expected symbol slice");
        assert!(snip.contains("// L4-36"), "expected exact L<start>-<end> range annotation, got:\n{}", snip);
        assert!(snip.contains("let step_29 = input.saturating_add(29);"), "fuller slice >520B should preserve body");
        assert!(snip.contains("pub fn compute_checksum"));

        // 2. Focused <=2 files with empty query also allocates fuller annotated symbol slices
        let pack_focus = build_context_pack(
            &jail,
            &ContextPackRequest {
                focus_paths: vec!["src/guard.rs".into(), "src/other.rs".into()],
                include_git: false,
                ..Default::default()
            },
            true,
        )
        .unwrap();
        assert!(pack_focus.used_bytes <= 8_500);
        assert!(pack_focus.files.iter().all(|c| c.snippet.is_some()));

        // 3. Query key normalization & fine-grained freshness check
        assert_eq!(
            normalize_pack_query_key(Some("Foo, BAR!  foo?")),
            normalize_pack_query_key(Some("bar foo"))
        );
        assert_eq!(normalize_pack_query_key(None), "");

        assert!(is_pack_fresh(&jail, &pack_sym));
        fs::write(
            dir.path().join("src/guard.rs"),
            format!("{}\n// appended\n", rust_src),
        )
        .unwrap();
        assert!(!is_pack_fresh(&jail, &pack_sym), "modified byte_len/mtime must invalidate freshness");
    }
}
