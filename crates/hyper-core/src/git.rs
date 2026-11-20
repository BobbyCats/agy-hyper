use crate::jail::WorkspaceJail;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitSummary {
    pub oid: String,
    pub author: String,
    pub time_secs: i64,
    pub subject: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitInspectResult {
    pub is_git_repo: bool,
    pub engine: String,
    pub branch: Option<String>,
    pub head_oid: Option<String>,
    pub dirty_files: Vec<String>,
    pub dirty_truncated: bool,
    pub recent_commits: Vec<CommitSummary>,
    pub hotspot_files: Vec<(String, usize)>,
}

/// Pure-Rust, physically READ-ONLY Git inspection powered by `gix` (gitoxide).
///
/// Security guarantee by construction:
/// - Zero `git` CLI subprocesses -> impossible to inject `--color -D` / `-d` / `--upload-pack` flags.
/// - Compiled with `gix` `default-features = false` (no transport/http/ssh/mutation features) ->
///   physically incapable of mutating refs, deleting branches/tags, or touching the network.
pub fn inspect_repo(
    jail: &WorkspaceJail,
    max_commits: usize,
    max_dirty: usize,
    include_hotspots: bool,
) -> GitInspectResult {
    let repo = match gix::discover(jail.root()) {
        Ok(r) => r,
        Err(_) => {
            return GitInspectResult {
                is_git_repo: false,
                engine: "gix-readonly".into(),
                branch: None,
                head_oid: None,
                dirty_files: Vec::new(),
                dirty_truncated: false,
                recent_commits: Vec::new(),
                hotspot_files: Vec::new(),
            };
        }
    };

    let mut branch: Option<String> = None;
    let mut head_oid_short: Option<String> = None;
    let mut head_id_obj = None;

    if let Ok(head) = repo.head() {
        if let Some(name) = head.referent_name() {
            branch = Some(name.shorten().to_string());
        }
        if let Some(id) = head.id() {
            head_oid_short = Some(id.shorten_or_id().to_string());
            head_id_obj = Some(id.detach());
        }
    }

    // Recent commits via pure-Rust rev_walk
    let mut recent_commits = Vec::new();
    let commit_limit = max_commits.clamp(1, 50);
    if let Some(start_id) = head_id_obj {
        if let Ok(walk) = repo.rev_walk([start_id]).all() {
            for info in walk.take(commit_limit).flatten() {
                if let Ok(commit) = info.object() {
                    let oid = info.id.to_hex_with_len(8).to_string();
                    let subject = commit
                        .message_raw_sloppy()
                        .to_string()
                        .lines()
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    let author = commit
                        .author()
                        .map(|a| a.name.to_string())
                        .unwrap_or_else(|_| "unknown".into());
                    let time_secs = commit.time().map(|t| t.seconds).unwrap_or(0);
                    recent_commits.push(CommitSummary {
                        oid,
                        author,
                        time_secs,
                        subject,
                    });
                }
            }
        }
    }

    // Working-tree + index status via pure-Rust gix status (read-only!)
    let mut dirty_files: Vec<String> = Vec::new();
    let mut dirty_truncated = false;
    let dirty_cap = max_dirty.clamp(1, 200);

    if let Ok(platform) = repo.status(gix::progress::Discard) {
        if let Ok(iter) = platform.into_iter(Vec::<gix::bstr::BString>::new()) {
            for item in iter.flatten() {
                let rel = item.location().to_string();
                if !dirty_files.contains(&rel) {
                    if dirty_files.len() < dirty_cap {
                        dirty_files.push(rel);
                    } else {
                        dirty_truncated = true;
                        break;
                    }
                }
            }
        }
    }
    dirty_files.sort();

    // Lightweight co-change / churn hotspot scoring from dirty files + recent tree diffs (bounded!)
    let mut hotspot_files: Vec<(String, usize)> = Vec::new();
    if include_hotspots {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for f in &dirty_files {
            *counts.entry(f.clone()).or_insert(0) += 3;
        }
        if let Some(start_id) = head_id_obj {
            if let Ok(walk) = repo.rev_walk([start_id]).all() {
                let mut prev_tree = None;
                for info in walk.take(18).flatten() {
                    if let Ok(commit) = info.object() {
                        if let Ok(tree) = commit.tree() {
                            if let Some(newer_tree) = prev_tree.take() {
                                collect_changed_paths(&repo, &tree, &newer_tree, &mut counts);
                            }
                            prev_tree = Some(tree);
                        }
                    }
                }
            }
        }
        hotspot_files = counts.into_iter().collect();
        hotspot_files.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        hotspot_files.truncate(15);
    }

    GitInspectResult {
        is_git_repo: true,
        engine: "gix-readonly".into(),
        branch,
        head_oid: head_oid_short,
        dirty_files,
        dirty_truncated,
        recent_commits,
        hotspot_files,
    }
}

fn collect_changed_paths(
    repo: &gix::Repository,
    older: &gix::Tree<'_>,
    newer: &gix::Tree<'_>,
    counts: &mut HashMap<String, usize>,
) {
    let Ok(changes) = repo.diff_tree_to_tree(Some(older), Some(newer), None) else {
        return;
    };
    for change in changes.into_iter().take(60) {
        let loc = change.location().to_string();
        if !loc.is_empty() && !loc.ends_with(".lock") {
            *counts.entry(loc).or_insert(0) += 1;
        }
    }
}

/// Fast single-call branch/head read for SessionStart (<3ms).
pub fn quick_head(root: &Path) -> Option<(String, String)> {
    let repo = gix::discover(root).ok()?;
    let head = repo.head().ok()?;
    let branch = head
        .referent_name()
        .map(|n| n.shorten().to_string())
        .unwrap_or_else(|| "HEAD".into());
    let oid = head
        .id()
        .map(|id| id.shorten_or_id().to_string())
        .unwrap_or_else(|| "unborn".into());
    Some((branch, oid))
}
