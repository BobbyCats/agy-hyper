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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineKind {
    TestSummaryOk,
    TestSummaryFailed,
    PassingTestCase,
    Error,
    Warning,
    Plain,
}

fn passing_test_case_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?x)
            ^\s*test\s+\S+.*\s+\.\.\.\s+(ok|ignored)\s*$
            | ^\s*PASS\s+\[\s*[0-9.]+s\]\s+
            | ^\s*---\s+(PASS|SKIP):\s+
            | ^\s*\S+::\S+.*\s+(PASSED|SKIPPED|XFAIL)\b
            | ^\s*[✓✔]\s+
            | ^\s*PASS\s+\S+
            ",
        )
        .unwrap()
    })
}

fn test_summary_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?xi)
            ^test\s+result:\s*(ok|FAILED)\.
            | ^\s*Summary\s+\[[^\]]+\]\s+\d+\s+tests?\s+run:
            | ^(ok|FAIL)\s+\S+\s+(\(cached\)|\[build\s+failed\]|[0-9.]+s\b)
            | ^=+\s*.*\b\d+\s+(passed|failed|error|errors|skipped|xfailed)\b.*\bin\s+[0-9.]+s\s*=*$
            | ^\s*\d+\s+(passed|failed|error|errors|skipped|xfailed)\b.*\bin\s+[0-9.]+s\s*$
            | ^Ran\s+\d+\s+tests?\s+in\s+[0-9.]+s
            | ^FAILED\s+\((failures|errors)=\d+
            | ^\s*(Test\s+Files|Test\s+Suites|Tests|Snapshots|Suites)\s*:?\s+.*\b\d+\s+(passed|failed|skipped|total|todo)\b
            | ^\s*\d+\s+(passing|failing)\b
            | ^\s*\d+\s+tests?\s+(passed|failed)\b
            | ^\s*\#\s*(tests|pass|fail|skipped|cancelled)\s+\d+\s*$
            | ^\s*\d+\s+(pass|fail)\s*$
            ",
        )
        .unwrap()
    })
}

fn nonzero_failure_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?xi)
            ^test\s+result:\s*FAILED\.
            | ^FAIL\s+\S+\s+(\[build\s+failed\]|[0-9.]+s\b)
            | ^FAILED\s+\((failures|errors)=[1-9]\d*
            | ^\s*\#\s*fail\s+[1-9]\d*\s*$
            | ^\s*[1-9]\d*\s+fail\s*$
            | \b[1-9]\d*\s+(failed|failing|failures|error|errors)\b
            | \b[1-9]\d*\s+tests?\s+failed\b
            ",
        )
        .unwrap()
    })
}

fn zero_count_benign_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b0\s+(errors?|failed|failing|failures)\b").unwrap()
    })
}

fn error_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)(^error(\[E\d+\])?:|error TS\d+:|\bFAIL\b|\bFAILED\b|Traceback \(most recent|AssertionError|TypeError|SyntaxError|ReferenceError|panic:|panicked at|cannot find|undefined:|BUILD FAILED|exit status [1-9]|^\s*✗|^\s*×|\bHTTP/?\d?(?:\.\d)?\s+[45]\d\d\b|\bstatus[:= ]+[45]\d\d\b)",
        )
        .unwrap()
    })
}

fn warning_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)(^warning(\[[^\]]+\])?:|warning TS\d+:|DeprecationWarning|\bWARN\b)").unwrap())
}

fn classify_line(line: &str) -> LineKind {
    // 1. Single passing test-case lines (e.g. `test foo::bar_failed ... ok`) must NEVER
    //    be classified as errors or crowd out aggregate test summaries.
    if passing_test_case_re().is_match(line) {
        return LineKind::PassingTestCase;
    }

    // 2. Authoritative test-runner summary lines take precedence over generic error regexes,
    //    so `test result: ok. 44 passed; 0 failed` is classified as TestSummaryOk, not Error.
    if test_summary_re().is_match(line) {
        return if nonzero_failure_re().is_match(line) {
            LineKind::TestSummaryFailed
        } else {
            LineKind::TestSummaryOk
        };
    }

    // 3. Genuine errors (after neutralizing zero-count phrases like `0 errors` / `0 failed`).
    let scrubbed = zero_count_benign_re().replace_all(line, "");
    if error_re().is_match(&scrubbed) {
        return LineKind::Error;
    }

    // 4. Compiler/tool warnings.
    if warning_re().is_match(line) {
        return LineKind::Warning;
    }

    LineKind::Plain
}

pub fn strip_ansi(input: &str) -> String {
    ansi_re().replace_all(input, "").into_owned()
}

/// Salience-ranked diagnostic filter that:
/// 1. Classifies lines via a single semantic model (`PassingTestCase`, `TestSummaryOk`,
///    `TestSummaryFailed`, `Error`, `Warning`, `Plain`) so `0 failed` and passing test names
///    containing `failed`/`error` are NEVER miscounted as errors.
/// 2. Preserves all authoritative test summaries + compiler warnings on `exit_code == 0`.
/// 3. Reserves tail root-cause lines + failure summaries on `exit_code != 0` BEFORE filling
///    salient errors with 3 lines of trailing context (capturing `left:`/`right:` diffs).
/// 4. Strictly respects `max_bytes` UTF-8 budget.
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

    let mut error_lines = Vec::new();
    let mut warning_lines = Vec::new();
    let mut ok_summary_lines = Vec::new();
    let mut failed_summary_lines = Vec::new();

    for (idx, line) in combined_lines.iter().enumerate() {
        match classify_line(line) {
            LineKind::TestSummaryOk => ok_summary_lines.push((idx, line.as_str())),
            LineKind::TestSummaryFailed => failed_summary_lines.push((idx, line.as_str())),
            LineKind::Error => error_lines.push((idx, line.as_str())),
            LineKind::Warning => warning_lines.push((idx, line.as_str())),
            LineKind::PassingTestCase | LineKind::Plain => {}
        }
    }

    let error_count = error_lines.len() + failed_summary_lines.len();
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
        // Keep all authoritative test summaries (up to 16 suites/crates), plus warnings and
        // any non-fatal error lines, without letting individual passing test names crowd them out.
        for (_, l) in failed_summary_lines.iter().take(6) {
            push_capped(&mut selected, &mut used_bytes, l);
        }
        for (_, l) in ok_summary_lines.iter().rev().take(16).rev() {
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
        if !error_lines.is_empty() {
            push_capped(
                &mut selected,
                &mut used_bytes,
                &format!("--- {} non-fatal error line(s) ---", error_lines.len()),
            );
            for (_, l) in error_lines.iter().take(4) {
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
        // Reserve up to 45% of budget strictly for failure summaries + TAIL (root-cause & stderr finale),
        // then fill top salient errors + up to 3 lines of context after each error (e.g. `-->`, `left:`, `right:`).
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

        let failed_sum_bytes: usize = failed_summary_lines
            .iter()
            .take(6)
            .map(|(_, l)| l.len().min(260) + 1)
            .sum();
        let tail_reserve: usize = (tail_lines
            .iter()
            .map(|l| l.len().min(260) + 1)
            .sum::<usize>()
            + failed_sum_bytes)
            .min(budget * 45 / 100);
        let head_budget = budget.saturating_sub(tail_reserve + 32);

        // Salient errors + up to 3 trailing context lines (Rust `-->`, `assertion`, `left:`, `right:`)
        let mut included_indices = std::collections::HashSet::new();
        for &(idx, line) in error_lines.iter().take(24) {
            if used_bytes >= head_budget {
                break;
            }
            if included_indices.insert(idx) {
                let (clean, _) = truncate_utf8_bytes(line, 260);
                if used_bytes + clean.len() < head_budget {
                    used_bytes += clean.len() + 1;
                    selected.push(clean.to_string());
                }
            }
            for ctx in 1..=3 {
                if let Some(next_line) = combined_lines.get(idx + ctx) {
                    if included_indices.insert(idx + ctx) {
                        let (clean, _) = truncate_utf8_bytes(next_line, 260);
                        if used_bytes + clean.len() < head_budget {
                            used_bytes += clean.len() + 1;
                            selected.push(clean.to_string());
                        }
                    }
                }
            }
        }

        // Preserve authoritative failing test summaries before tail/stderr
        for (_, l) in failed_summary_lines.iter().take(6) {
            if !selected.iter().any(|s| s == l)
                && !push_capped(&mut selected, &mut used_bytes, l)
            {
                break;
            }
        }

        // Always append tail root-cause section
        if !tail_lines.is_empty() {
            push_capped(&mut selected, &mut used_bytes, "--- tail / stderr ---");
            for l in tail_lines {
                if !selected.iter().any(|s| s == l)
                    && !push_capped(&mut selected, &mut used_bytes, l)
                {
                    break;
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
        assert!(d.summary_lines.iter().any(|l| l.contains("512 tests passed")));
    }

    #[test]
    fn cargo_test_passing_workspace_reports_zero_errors_and_all_test_results() {
        let out = "\
running 6 tests
test mcp::tests::mcp_workspace_guard_blocks_plugin_root_home_and_fs_root ... ok
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

running 44 tests
test native_capture::tests::skips_ambiguous_duplicate_replacement_and_failed_write_mismatch ... ok
test runner::tests::fail_fast_does_not_false_kill_normal_exiting_traceback ... ok
test jail::tests::rejects_on_error_and_fail ... ok
test result: ok. 44 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.13s

running 3 tests
test state::tests::stalled_subagent_requires_active_and_90s_silence ... ok
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

running 0 tests
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

running 0 tests
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
";
        let err = "    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.14s\n     Running unittests src/lib.rs (target/debug/deps/hyper_core)\n";
        let d = distill_output(out, err, 0, 4000);
        assert_eq!(d.error_count, 0, "0 failed and passing test names must NEVER count as errors");
        assert_eq!(d.warning_count, 0);
        let result_lines: Vec<&String> = d
            .summary_lines
            .iter()
            .filter(|l| l.starts_with("test result: ok."))
            .collect();
        assert_eq!(result_lines.len(), 5, "all 5 crate test result summaries must be preserved inline");
        assert!(
            !d.summary_lines.iter().any(|l| l.contains("failed_write_mismatch")),
            "individual passing test names must not crowd out summary lines"
        );
    }

    #[test]
    fn multi_ecosystem_test_summaries_classify_ok_vs_failed_accurately() {
        // Passing summaries & zero-count build lines
        for ok_line in [
            "test result: ok. 44 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.13s",
            "Summary [ 2.14s] 53 tests run: 53 passed, 0 failed, 0 skipped",
            "ok  \tgithub.com/org/pkg\t0.412s",
            "ok  \tgithub.com/org/pkg\t(cached)",
            "=== 42 passed, 1 skipped in 1.24s ===",
            "Ran 15 tests in 0.123s",
            "Tests:       12 passed, 12 total",
            "Test Files  3 passed (3)",
            "  18 passing (412ms)",
            "  0 failing",
            "# fail 0",
            " 0 fail",
        ] {
            assert_eq!(classify_line(ok_line), LineKind::TestSummaryOk, "expected TestSummaryOk for: {ok_line}");
        }

        // Failing summaries
        for fail_line in [
            "test result: FAILED. 42 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.98s",
            "Summary [ 2.14s] 53 tests run: 51 passed, 2 failed, 0 skipped",
            "FAIL\tgithub.com/org/pkg\t0.412s",
            "=== 40 passed, 2 failed in 1.80s ===",
            "FAILED (failures=2)",
            "Tests:       2 failed, 10 passed, 12 total",
            "  2 failing",
            "# fail 2",
            " 2 fail",
        ] {
            assert_eq!(classify_line(fail_line), LineKind::TestSummaryFailed, "expected TestSummaryFailed for: {fail_line}");
        }

        // Benign zero-error build lines must not trigger Error
        assert_eq!(
            classify_line("Build finished in 1.2s (0 errors, 0 warnings, 0 failed)"),
            LineKind::Plain
        );
        // Passing individual test names with fail/error keywords must not trigger Error
        assert_eq!(
            classify_line("test jail::tests::rejects_failed_or_error_write ... ok"),
            LineKind::PassingTestCase
        );
        assert_eq!(
            classify_line("tests/test_auth.py::test_failed_login PASSED [ 50%]"),
            LineKind::PassingTestCase
        );
        assert_eq!(
            classify_line("  ✓ rejects failed token cleanly (4ms)"),
            LineKind::PassingTestCase
        );
    }

    #[test]
    fn exit_nonzero_excludes_passing_crate_summary_and_keeps_assertion_diff() {
        let out = "\
running 6 tests
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

running 44 tests
test audit::tests::layout_guard ... FAILED
thread 'audit::tests::layout_guard' panicked at crates/hyper-core/src/audit.rs:939:9:
assertion `left == right` failed
  left: 195.0
 right: 164.0
test result: FAILED. 43 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.01s
";
        let err = "error: test failed, to rerun pass `-p hyper-core --lib`\n";
        let d = distill_output(out, err, 101, 2000);
        assert!(
            !d.summary_lines.iter().any(|l| l.starts_with("test result: ok.")),
            "passing crate summaries must not be misclassified as errors on exit != 0"
        );
        assert!(d.summary_lines.iter().any(|l| l.contains("right: 164.0")));
        assert!(d.summary_lines.iter().any(|l| l.starts_with("test result: FAILED.")));
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
