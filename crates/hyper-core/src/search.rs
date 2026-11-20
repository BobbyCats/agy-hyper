use crate::jail::{is_secret_path, WorkspaceJail};
use anyhow::Result;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::sinks::UTF8;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchQuery {
    pub pattern: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub glob: Option<String>,
    #[serde(default)]
    pub fixed_strings: bool,
    #[serde(default)]
    pub case_sensitive: bool,
    #[serde(default = "default_max_matches")]
    pub max_matches: usize,
}

fn default_max_matches() -> usize {
    40
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchMatch {
    pub path: String,
    pub line: usize,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchQueryResult {
    pub pattern: String,
    pub matches: Vec<SearchMatch>,
    pub total_files_scanned: usize,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobResult {
    pub files: Vec<String>,
    pub total_matched: usize,
    pub truncated: bool,
}

fn is_ignored_dir(name: &str) -> bool {
    matches!(
        name,
        ".git" | "node_modules" | "target" | ".next" | "dist" | "build" | ".venv" | "__pycache__"
    )
}

fn build_walker(root: &Path, glob: Option<&str>, max_filesize: Option<u64>) -> WalkBuilder {
    let mut walk = WalkBuilder::new(root);
    walk.hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        .follow_links(false)
        .max_filesize(max_filesize)
        .filter_entry(|e| {
            if e.depth() > 0 && e.file_type().is_some_and(|t| t.is_dir()) {
                return !is_ignored_dir(&e.file_name().to_string_lossy());
            }
            true
        });

    if let Some(pat) = glob.map(str::trim).filter(|s| !s.is_empty()) {
        let mut ob = OverrideBuilder::new(root);
        for part in pat.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let _ = ob.add(part);
        }
        if let Ok(ov) = ob.build() {
            walk.overrides(ov);
        }
    }
    walk
}

/// Pure-Rust parallel search powered by ripgrep's `ignore` + `grep-searcher` + `grep-regex`.
pub fn batch_search(
    jail: &WorkspaceJail,
    queries: &[SearchQuery],
    block_secrets: bool,
) -> Vec<SearchQueryResult> {
    if queries.is_empty() {
        return Vec::new();
    }
    let avail = std::thread::available_parallelism()
        .map(|n| n.get().clamp(2, 8))
        .unwrap_or(4);
    let threads_per_query = (avail / queries.len().max(1)).max(1);
    let results: Mutex<Vec<(usize, SearchQueryResult)>> =
        Mutex::new(Vec::with_capacity(queries.len()));
    std::thread::scope(|scope| {
        for (idx, q) in queries.iter().enumerate() {
            let results_ref = &results;
            scope.spawn(move || {
                let res = run_single_search_with_threads(jail, q, block_secrets, threads_per_query);
                if let Ok(mut guard) = results_ref.lock() {
                    guard.push((idx, res));
                }
            });
        }
    });
    let mut out = results.into_inner().unwrap_or_default();
    out.sort_by_key(|(idx, _)| *idx);
    out.into_iter().map(|(_, r)| r).collect()
}

pub fn run_single_search(
    jail: &WorkspaceJail,
    query: &SearchQuery,
    block_secrets: bool,
) -> SearchQueryResult {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get().clamp(2, 8))
        .unwrap_or(4);
    run_single_search_with_threads(jail, query, block_secrets, threads)
}

/// Single-pass parallel search matching ANY of `terms` via one compiled alternation regex.
pub fn search_multi_terms(
    jail: &WorkspaceJail,
    terms: &[String],
    max_matches: usize,
    block_secrets: bool,
) -> SearchQueryResult {
    let escaped: Vec<String> = terms
        .iter()
        .map(|t| regex::escape(t.trim()))
        .filter(|s| !s.is_empty())
        .collect();
    if escaped.is_empty() {
        return SearchQueryResult {
            pattern: String::new(),
            matches: Vec::new(),
            total_files_scanned: 0,
            truncated: false,
            error: None,
        };
    }
    run_single_search(
        jail,
        &SearchQuery {
            pattern: escaped.join("|"),
            path: None,
            glob: None,
            fixed_strings: false,
            case_sensitive: false,
            max_matches,
        },
        block_secrets,
    )
}

fn run_single_search_with_threads(
    jail: &WorkspaceJail,
    query: &SearchQuery,
    block_secrets: bool,
    threads: usize,
) -> SearchQueryResult {
    let max_matches = query.max_matches.clamp(1, 200);
    let root = match &query.path {
        Some(p) if !p.trim().is_empty() => match jail.resolve_existing(p) {
            Ok(resolved) => resolved,
            Err(e) => {
                return SearchQueryResult {
                    pattern: query.pattern.clone(),
                    matches: Vec::new(),
                    total_files_scanned: 0,
                    truncated: false,
                    error: Some(e.to_string()),
                };
            }
        },
        _ => jail.root().to_path_buf(),
    };

    let matcher = match RegexMatcherBuilder::new()
        .case_insensitive(!query.case_sensitive)
        .fixed_strings(query.fixed_strings)
        .build(&query.pattern)
    {
        Ok(m) => m,
        Err(e) => {
            return SearchQueryResult {
                pattern: query.pattern.clone(),
                matches: Vec::new(),
                total_files_scanned: 0,
                truncated: false,
                error: Some(format!("invalid pattern: {}", e)),
            };
        }
    };

    let mut walk = build_walker(&root, query.glob.as_deref(), Some(2 * 1024 * 1024));
    walk.threads(threads.clamp(1, 8));

    let matches: Mutex<Vec<SearchMatch>> = Mutex::new(Vec::new());
    let match_count = AtomicUsize::new(0);
    let files_scanned = AtomicUsize::new(0);
    let truncated = AtomicBool::new(false);

    walk.build_parallel().run(|| {
        let matcher = matcher.clone();
        let matches = &matches;
        let match_count = &match_count;
        let files_scanned = &files_scanned;
        let truncated = &truncated;
        let mut searcher = SearcherBuilder::new()
            .binary_detection(BinaryDetection::quit(b'\x00'))
            .line_number(true)
            .build();

        Box::new(move |entry| {
            if match_count.load(Ordering::Relaxed) >= max_matches {
                truncated.store(true, Ordering::Relaxed);
                return ignore::WalkState::Quit;
            }
            let Ok(dent) = entry else {
                return ignore::WalkState::Continue;
            };
            if !dent.file_type().is_some_and(|t| t.is_file()) {
                return ignore::WalkState::Continue;
            }
            let path = dent.path();
            if block_secrets && is_secret_path(path) {
                return ignore::WalkState::Continue;
            }
            files_scanned.fetch_add(1, Ordering::Relaxed);

            let rel = jail.relative_display(path);
            let mut local_hits: Vec<SearchMatch> = Vec::new();
            let _ = searcher.search_path(
                &matcher,
                path,
                UTF8(|lnum, line| {
                    if match_count.fetch_add(1, Ordering::Relaxed) >= max_matches {
                        truncated.store(true, Ordering::Relaxed);
                        return Ok(false);
                    }
                    let trimmed = line.trim_end_matches(&['\r', '\n'][..]);
                    let clean = if trimmed.len() > 220 {
                        let (s, _) = crate::config::truncate_utf8_bytes(trimmed, 220);
                        format!("{}…", s)
                    } else {
                        trimmed.to_string()
                    };
                    local_hits.push(SearchMatch {
                        path: rel.clone(),
                        line: lnum as usize,
                        text: clean,
                    });
                    Ok(true)
                }),
            );

            if !local_hits.is_empty() {
                if let Ok(mut guard) = matches.lock() {
                    guard.extend(local_hits);
                }
            }
            ignore::WalkState::Continue
        })
    });

    let mut out = matches.into_inner().unwrap_or_default();
    out.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.line.cmp(&b.line)));
    if out.len() > max_matches {
        out.truncate(max_matches);
        truncated.store(true, Ordering::Relaxed);
    }

    SearchQueryResult {
        pattern: query.pattern.clone(),
        matches: out,
        total_files_scanned: files_scanned.load(Ordering::Relaxed),
        truncated: truncated.load(Ordering::Relaxed),
        error: None,
    }
}

pub fn glob_files(
    jail: &WorkspaceJail,
    pattern: Option<&str>,
    subpath: Option<&str>,
    max_files: usize,
    block_secrets: bool,
) -> Result<GlobResult> {
    let limit = max_files.clamp(1, 500);
    let root: PathBuf = match subpath {
        Some(p) if !p.trim().is_empty() => jail.resolve_existing(p)?,
        _ => jail.root().to_path_buf(),
    };

    let walk = build_walker(&root, pattern, None);
    let mut files: Vec<String> = Vec::new();
    let mut total_matched = 0usize;
    let mut truncated = false;

    for dent in walk.build().flatten() {
        if !dent.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = dent.path();
        if block_secrets && is_secret_path(path) {
            continue;
        }
        total_matched += 1;
        if files.len() < limit {
            files.push(jail.relative_display(path));
        } else {
            truncated = true;
            if total_matched > limit * 4 {
                break;
            }
        }
    }

    files.sort();
    Ok(GlobResult {
        files,
        total_matched,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn native_ripgrep_search_skips_binary_and_secrets_and_reports_truncation() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("a.rs"),
            "pub fn hyper_speed() {}\nlet hyper_speed = 1;\nlet hyper_speed = 2;\n",
        )
        .unwrap();
        fs::write(dir.path().join(".env"), "API_KEY=hyper_speed_secret\n").unwrap();
        fs::write(dir.path().join("bin.dat"), b"hyper_speed\0binary\0data").unwrap();

        let jail = WorkspaceJail::new(dir.path(), false, true).unwrap();
        let res = run_single_search(
            &jail,
            &SearchQuery {
                pattern: "hyper_speed".into(),
                path: None,
                glob: None,
                fixed_strings: true,
                case_sensitive: true,
                max_matches: 2,
            },
            true,
        );
        assert!(res.error.is_none());
        assert!(res.truncated);
        assert_eq!(res.matches.len(), 2);
        assert!(res.matches.iter().all(|m| m.path == "a.rs"));
    }
}
