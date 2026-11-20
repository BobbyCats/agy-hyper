use crate::config::truncate_utf8_bytes;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::OnceLock;

pub const HEAD_CAPTURE_BYTES: usize = 64 * 1024;
pub const TAIL_CAPTURE_BYTES: usize = 512 * 1024;

/// Head + Tail ring buffer: NEVER drops the tail where fatal compiler/test errors live.
#[derive(Debug, Default)]
pub struct HeadTailBuffer {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total_bytes: usize,
    spilled_middle_bytes: usize,
}

impl HeadTailBuffer {
    pub fn push(&mut self, chunk: &[u8]) {
        self.total_bytes += chunk.len();
        let mut rest = chunk;
        if self.head.len() < HEAD_CAPTURE_BYTES {
            let take = (HEAD_CAPTURE_BYTES - self.head.len()).min(rest.len());
            self.head.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
        }
        if !rest.is_empty() {
            if rest.len() >= TAIL_CAPTURE_BYTES {
                self.spilled_middle_bytes += self.tail.len() + (rest.len() - TAIL_CAPTURE_BYTES);
                self.tail.clear();
                self.tail
                    .extend(rest[rest.len() - TAIL_CAPTURE_BYTES..].iter().copied());
            } else {
                let overflow = (self.tail.len() + rest.len()).saturating_sub(TAIL_CAPTURE_BYTES);
                if overflow > 0 {
                    self.spilled_middle_bytes += overflow;
                    for _ in 0..overflow {
                        self.tail.pop_front();
                    }
                }
                self.tail.extend(rest.iter().copied());
            }
        }
    }

    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    pub fn spilled_middle_bytes(&self) -> usize {
        self.spilled_middle_bytes
    }

    pub fn render_lossy(&self) -> String {
        let head_str = String::from_utf8_lossy(&self.head);
        if self.tail.is_empty() {
            return head_str.into_owned();
        }
        let tail_vec: Vec<u8> = self.tail.iter().copied().collect();
        let tail_str = String::from_utf8_lossy(&tail_vec);
        if self.spilled_middle_bytes > 0 {
            format!(
                "{}\n\n… [{} bytes elided in middle; head 64KB + tail 512KB preserved] …\n\n{}",
                head_str, self.spilled_middle_bytes, tail_str
            )
        } else {
            format!("{}{}", head_str, tail_str)
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticDigest {
    pub summary_lines: Vec<String>,
    pub warning_count: usize,
    pub error_count: usize,
    pub truncated: bool,
    pub toc_hint: Option<String>,
}

fn ansi_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").unwrap())
}

fn error_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)(^error(\[E\d+\])?:|error TS\d+:|FAIL\b|FAILED\b|Traceback \(most recent|AssertionError|TypeError|SyntaxError|ReferenceError|panic:|panicked at|cannot find|undefined:|BUILD FAILED|exit status [1-9]|^\s*✗|^\s*×|\bHTTP/?\d?(?:\.\d)?\s+[45]\d\d\b|\bstatus[:= ]+[45]\d\d\b)",
        )
        .unwrap()
    })
}

fn warning_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)(^warning(\[[^\]]+\])?:|warning TS\d+:|DeprecationWarning|\bWARN\b)").unwrap())
}

fn test_summary_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(test result:|Tests?:.*\d+|passed|failed|skipped|Suites:|ran \d+ test|ok\s+\S+\s+\d|\d+\s+passing|\d+\s+failing)")
            .unwrap()
    })
}

pub fn strip_ansi(input: &str) -> String {
    ansi_re().replace_all(input, "").into_owned()
}

/// Salience-ranked diagnostic filter that:
/// 1. Preserves warnings + test summaries even when `exit_code == 0`.
/// 2. Always reserves tail root-cause lines (stderr + last 15 lines) when `exit_code != 0`
///    BEFORE filling middle matches (eliminating tail-truncation blindness).
/// 3. Strictly respects `max_bytes` UTF-8 budget.
pub fn distill_output(
    stdout_raw: &str,
    stderr_raw: &str,
    exit_code: i32,
    max_bytes: usize,
) -> DiagnosticDigest {
    let stdout = strip_ansi(stdout_raw);
    let stderr = strip_ansi(stderr_raw);
    let combined_lines: Vec<String> = stdout
        .lines()
        .chain(stderr.lines())
        .map(|l| l.trim_end().to_string())
        .filter(|l| !l.trim().is_empty())
        .collect();

    let err_re = error_re();
    let warn_re = warning_re();
    let sum_re = test_summary_re();

    let mut error_lines = Vec::new();
    let mut warning_lines = Vec::new();
    let mut summary_hits = Vec::new();

    for (idx, line) in combined_lines.iter().enumerate() {
        if err_re.is_match(line) {
            error_lines.push((idx, line.as_str()));
        } else if warn_re.is_match(line) {
            warning_lines.push((idx, line.as_str()));
        } else if sum_re.is_match(line) {
            summary_hits.push((idx, line.as_str()));
        }
    }

    let error_count = error_lines.len();
    let warning_count = warning_lines.len();

    let mut selected: Vec<String> = Vec::new();
    let mut used_bytes = 0usize;
    let budget = max_bytes.max(512);

    let push_capped = |target: &mut Vec<String>, used: &mut usize, line: &str| -> bool {
        let (clean, _) = truncate_utf8_bytes(line, 260);
        let need = clean.len() + 1;
        if *used + need > budget {
            return false;
        }
        *used += need;
        target.push(clean.to_string());
        true
    };

    if exit_code == 0 {
        // Keep test summaries + up to 10 warnings + last 4 lines so warnings are NEVER silently erased!
        for (_, l) in summary_hits.iter().rev().take(6).rev() {
            push_capped(&mut selected, &mut used_bytes, l);
        }
        if !warning_lines.is_empty() {
            push_capped(
                &mut selected,
                &mut used_bytes,
                &format!("--- {} warning(s) ---", warning_lines.len()),
            );
            for (_, l) in warning_lines.iter().take(12) {
                if !push_capped(&mut selected, &mut used_bytes, l) {
                    break;
                }
            }
        }
        if selected.is_empty() {
            for l in combined_lines.iter().rev().take(6).rev() {
                if !push_capped(&mut selected, &mut used_bytes, l) {
                    break;
                }
            }
        }
    } else {
        // Reserve ~40% of budget strictly for the TAIL (root-cause & stderr finale),
        // then fill top salient errors + 1 line of context after each error!
        let tail_lines: Vec<&str> = {
            let stderr_nonempty: Vec<&str> = stderr
                .lines()
                .map(|l| l.trim_end())
                .filter(|l| !l.trim().is_empty())
                .collect();
            if !stderr_nonempty.is_empty() {
                stderr_nonempty.into_iter().rev().take(14).rev().collect()
            } else {
                combined_lines
                    .iter()
                    .rev()
                    .take(14)
                    .rev()
                    .map(|s| s.as_str())
                    .collect()
            }
        };

        let tail_reserve: usize = tail_lines
            .iter()
            .map(|l| l.len().min(260) + 1)
            .sum::<usize>()
            .min(budget * 45 / 100);
        let head_budget = budget.saturating_sub(tail_reserve + 32);

        // Salient errors + immediate next context line (e.g. Rust arrow `--> src/foo.rs:12:4`)
        let mut included_indices = std::collections::HashSet::new();
        for &(idx, line) in error_lines.iter().take(24) {
            if used_bytes >= head_budget {
                break;
            }
            if included_indices.insert(idx) {
                let (clean, _) = truncate_utf8_bytes(line, 260);
                if used_bytes + clean.len() + 1 <= head_budget {
                    used_bytes += clean.len() + 1;
                    selected.push(clean.to_string());
                }
            }
            for ctx in 1..=2 {
                if let Some(next_line) = combined_lines.get(idx + ctx) {
                    if included_indices.insert(idx + ctx) {
                        let (clean, _) = truncate_utf8_bytes(next_line, 260);
                        if used_bytes + clean.len() + 1 <= head_budget {
                            used_bytes += clean.len() + 1;
                            selected.push(clean.to_string());
                        }
                    }
                }
            }
        }

        // Always append tail root-cause section
        if !tail_lines.is_empty() {
            push_capped(&mut selected, &mut used_bytes, "--- tail / stderr ---");
            for l in tail_lines {
                if !selected.iter().any(|s| s == l) {
                    if !push_capped(&mut selected, &mut used_bytes, l) {
                        break;
                    }
                }
            }
        }
    }

    let truncated = combined_lines.len() > selected.len();
    let toc_hint = if truncated {
        Some(format!(
            "distilled {} -> {} lines ({} errors, {} warnings)",
            combined_lines.len(),
            selected.len(),
            error_count,
            warning_count
        ))
    } else {
        None
    };

    DiagnosticDigest {
        summary_lines: selected,
        warning_count,
        error_count,
        truncated,
        toc_hint,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_tail_buffer_preserves_fatal_tail_after_two_megabytes() {
        let mut buf = HeadTailBuffer::default();
        buf.push(b"START_MARKER_HEADER\n");
        let noise = vec![b'x'; 1024];
        for _ in 0..2048 {
            buf.push(&noise);
        }
        buf.push(b"\nFATAL_TAIL_ROOT_CAUSE: E0425 cannot find value\n");
        let rendered = buf.render_lossy();
        assert!(rendered.contains("START_MARKER_HEADER"));
        assert!(rendered.contains("FATAL_TAIL_ROOT_CAUSE"));
        assert!(buf.spilled_middle_bytes() > 1_000_000);
    }

    #[test]
    fn exit_zero_still_preserves_compiler_warnings() {
        let out = "Compiling demo v0.1.0\nwarning: unused variable: `secret`\n512 tests passed in 450ms\n";
        let d = distill_output(out, "", 0, 4000);
        assert_eq!(d.warning_count, 1);
        assert_eq!(d.error_count, 0, "450ms and 512 passed must never be misclassified as HTTP 4xx/5xx errors");
        assert!(d.summary_lines.iter().any(|l| l.contains("unused variable")));
    }

    #[test]
    fn exit_nonzero_never_chops_off_stderr_root_cause() {
        let mut spam = String::new();
        for i in 0..200 {
            spam.push_str(&format!("FAIL step {}: noise error line\n", i));
        }
        let stderr = "panicked at src/auth.rs:88:12: invalid token signature\n";
        let d = distill_output(&spam, stderr, 1, 2000);
        assert!(d
            .summary_lines
            .iter()
            .any(|l| l.contains("src/auth.rs:88:12")));
    }
}
