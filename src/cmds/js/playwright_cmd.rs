//! Filters Playwright E2E test output to show only failures.

use crate::core::stream::exec_capture;
use crate::core::tracking;
use crate::core::utils::{detect_package_manager, resolved_command, strip_ansi};
use anyhow::{Context, Result};
use regex::Regex;
use serde::Deserialize;
use std::collections::HashSet;

use crate::parser::{
    emit_degradation_warning, emit_passthrough_warning, truncate_passthrough, FormatMode,
    OutputParser, ParseResult, TestFailure, TestResult, TokenFormatter,
};

/// Cap on stdout/stderr tail per failed test, in characters (post-ANSI-strip).
const STDOUT_TAIL_CAP: usize = 500;
/// Max stack frames kept per failure (after node_modules pruning).
const MAX_STACK_FRAMES: usize = 5;

/// Matches real Playwright JSON reporter output (suites → specs → tests → results)
#[derive(Debug, Deserialize)]
struct PlaywrightJsonOutput {
    stats: PlaywrightStats,
    #[serde(default)]
    suites: Vec<PlaywrightSuite>,
}

#[derive(Debug, Deserialize)]
struct PlaywrightStats {
    expected: usize,
    unexpected: usize,
    skipped: usize,
    /// Duration in milliseconds (float in real Playwright output)
    #[serde(default)]
    duration: f64,
}

/// File-level or describe-level suite
#[derive(Debug, Deserialize)]
struct PlaywrightSuite {
    title: String,
    #[serde(default)]
    file: Option<String>,
    /// Individual test specs (test functions)
    #[serde(default)]
    specs: Vec<PlaywrightSpec>,
    /// Nested describe blocks
    #[serde(default)]
    suites: Vec<PlaywrightSuite>,
}

/// A single test function (may run in multiple browsers/projects)
#[derive(Debug, Deserialize)]
struct PlaywrightSpec {
    title: String,
    /// Overall pass/fail status across all projects
    ok: bool,
    /// Per-project/browser executions
    #[serde(default)]
    tests: Vec<PlaywrightExecution>,
}

/// A test execution in a specific browser/project
#[derive(Debug, Deserialize)]
struct PlaywrightExecution {
    /// "expected", "unexpected", "skipped", "flaky"
    status: String,
    #[serde(default)]
    results: Vec<PlaywrightAttempt>,
}

/// A single attempt/result for a test execution
#[derive(Debug, Deserialize)]
struct PlaywrightAttempt {
    /// "passed", "failed", "timedOut", "interrupted"
    status: String,
    /// Error details (array in Playwright >= v1.30)
    #[serde(default)]
    errors: Vec<PlaywrightError>,
    /// Artifact references (video, screenshot, trace paths)
    #[serde(default)]
    attachments: Vec<PlaywrightAttachment>,
    /// Captured stdout streams for this attempt
    #[serde(default)]
    stdout: Vec<PlaywrightStream>,
    /// Captured stderr streams for this attempt
    #[serde(default)]
    stderr: Vec<PlaywrightStream>,
}

#[derive(Debug, Deserialize)]
struct PlaywrightError {
    #[serde(default)]
    message: String,
    #[serde(default)]
    stack: Option<String>,
    #[serde(default)]
    snippet: Option<String>,
}

/// A Playwright attachment (video, screenshot, trace). We only surface `path`
/// references — inline `body` bytes are never emitted.
#[derive(Debug, Deserialize)]
struct PlaywrightAttachment {
    #[serde(default)]
    name: String,
    #[serde(default)]
    path: Option<String>,
}

/// A stdout/stderr chunk from Playwright's reporter.
#[derive(Debug, Deserialize)]
struct PlaywrightStream {
    #[serde(default)]
    text: Option<String>,
}

/// Parser for Playwright JSON output
pub struct PlaywrightParser;

impl OutputParser for PlaywrightParser {
    type Output = TestResult;

    fn parse(input: &str) -> ParseResult<TestResult> {
        // Tier 1: Try JSON parsing
        match serde_json::from_str::<PlaywrightJsonOutput>(input) {
            Ok(json) => {
                let mut failures = Vec::new();
                let mut total = 0;
                collect_test_results(&json.suites, &mut total, &mut failures);

                let result = TestResult {
                    total,
                    passed: json.stats.expected,
                    failed: json.stats.unexpected,
                    skipped: json.stats.skipped,
                    duration_ms: Some(json.stats.duration as u64),
                    failures,
                };

                ParseResult::Full(result)
            }
            Err(e) => {
                // Tier 2: Try regex extraction
                match extract_playwright_regex(input) {
                    Some(result) => {
                        ParseResult::Degraded(result, vec![format!("JSON parse failed: {}", e)])
                    }
                    None => {
                        // Tier 3: Passthrough
                        ParseResult::Passthrough(truncate_passthrough(input))
                    }
                }
            }
        }
    }
}

fn collect_test_results(
    suites: &[PlaywrightSuite],
    total: &mut usize,
    failures: &mut Vec<TestFailure>,
) {
    for suite in suites {
        let file_path = suite.file.as_deref().unwrap_or(&suite.title);

        for spec in &suite.specs {
            *total += 1;

            if !spec.ok {
                // Find the first failed execution and its error message
                let error_msg = spec
                    .tests
                    .iter()
                    .find(|t| t.status == "unexpected")
                    .and_then(|t| {
                        t.results
                            .iter()
                            .find(|r| r.status == "failed" || r.status == "timedOut")
                    })
                    .and_then(|r| r.errors.first())
                    .map(|e| e.message.clone())
                    .unwrap_or_else(|| "Test failed".to_string());

                failures.push(TestFailure {
                    test_name: spec.title.clone(),
                    file_path: file_path.to_string(),
                    error_message: error_msg,
                    stack_trace: None,
                });
            }
        }

        // Recurse into nested suites (describe blocks)
        collect_test_results(&suite.suites, total, failures);
    }
}

/// Tier 2: Extract test statistics using regex (degraded mode)
fn extract_playwright_regex(output: &str) -> Option<TestResult> {
    lazy_static::lazy_static! {
        static ref SUMMARY_RE: Regex = Regex::new(
            r"(\d+)\s+(passed|failed|flaky|skipped)"
        ).unwrap();
        static ref DURATION_RE: Regex = Regex::new(
            r"\((\d+(?:\.\d+)?)(ms|s|m)\)"
        ).unwrap();
    }

    let clean_output = strip_ansi(output);

    let mut passed = 0;
    let mut failed = 0;
    let mut skipped = 0;

    // Parse summary counts
    for caps in SUMMARY_RE.captures_iter(&clean_output) {
        let count: usize = caps[1].parse().unwrap_or(0);
        match &caps[2] {
            "passed" => passed = count,
            "failed" => failed = count,
            "skipped" => skipped = count,
            _ => {}
        }
    }

    // Parse duration
    let duration_ms = DURATION_RE.captures(&clean_output).and_then(|caps| {
        let value: f64 = caps[1].parse().ok()?;
        let unit = &caps[2];
        Some(match unit {
            "ms" => value as u64,
            "s" => (value * 1000.0) as u64,
            "m" => (value * 60000.0) as u64,
            _ => value as u64,
        })
    });

    // Only return if we found valid data
    let total = passed + failed + skipped;
    if total > 0 {
        Some(TestResult {
            total,
            passed,
            failed,
            skipped,
            duration_ms,
            failures: extract_failures_regex(&clean_output),
        })
    } else {
        None
    }
}

/// Extract failures using regex
fn extract_failures_regex(output: &str) -> Vec<TestFailure> {
    lazy_static::lazy_static! {
        static ref TEST_PATTERN: Regex = Regex::new(
            r"[×✗]\s+.*?›\s+([^›]+\.spec\.[tj]sx?)"
        ).unwrap();
    }

    let mut failures = Vec::new();

    for caps in TEST_PATTERN.captures_iter(output) {
        if let Some(spec) = caps.get(1) {
            failures.push(TestFailure {
                test_name: caps[0].to_string(),
                file_path: spec.as_str().to_string(),
                error_message: String::new(),
                stack_trace: None,
            });
        }
    }

    failures
}

/// Returns true when the user explicitly asked for a specific reporter.
/// In that case we honor their intent: no `--reporter=json` injection, no
/// filtering — pure passthrough. Rewriting their reporter silently is a
/// correctness footgun.
fn user_set_reporter(args: &[String]) -> bool {
    args.iter().any(|a| a == "--reporter" || a.starts_with("--reporter="))
}

fn is_install_subcommand(args: &[String]) -> bool {
    matches!(args.first().map(String::as_str), Some("install") | Some("install-deps"))
}

/// Compress `playwright install` progress output.
///
/// Playwright's install command dumps hundreds of lines of download progress
/// (progress bars, byte counts, per-chunk noise). We keep only actionable
/// lines: install confirmations, warnings, errors, and browser version marks.
fn filter_install(input: &str) -> String {
    lazy_static::lazy_static! {
        // Drop `|====   | 45%` style progress bars
        static ref PROGRESS_BAR_RE: Regex = Regex::new(r"^\s*\|[^|]*\|\s*\d+%").unwrap();
        // Drop leading `1.2 MiB / 3.4 MiB` byte counts
        static ref BYTES_RE: Regex = Regex::new(r"^\s*\d+(\.\d+)?\s*[KMG]i?B\s*/").unwrap();
        // Drop `Downloading … 1234 bytes` progress noise
        static ref DOWNLOAD_RE: Regex = Regex::new(r"^\s*Downloading[^\n]*\d+\s*(bytes|[KMG]?i?B)").unwrap();
    }

    let clean = strip_ansi(input);
    let mut kept: Vec<&str> = Vec::new();
    let mut dropped_downloads = 0usize;

    for line in clean.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if PROGRESS_BAR_RE.is_match(line) || BYTES_RE.is_match(line) {
            continue;
        }
        if DOWNLOAD_RE.is_match(line) {
            dropped_downloads += 1;
            continue;
        }
        // Pure separator lines like `====================` or `|----|----|`
        if trimmed.chars().all(|c| matches!(c, '=' | '-' | '|' | ' ' | '.')) {
            continue;
        }
        kept.push(line);
    }

    if kept.is_empty() {
        // Fallback per RTK rules: never silently swallow all output.
        return input.to_string();
    }

    let mut out: Vec<String> = kept.iter().map(|s| s.to_string()).collect();
    if dropped_downloads > 0 {
        out.push(format!("({} download progress lines omitted)", dropped_downloads));
    }
    out.join("\n")
}

/// Rich per-failure formatter that surfaces artifact paths, trimmed stack,
/// snippet window, and stdout tail — the fields the plain `TokenFormatter`
/// impl drops. Called only when Tier 1 JSON parse succeeds.
fn format_rich_output(json: &PlaywrightJsonOutput) -> String {
    let stats = &json.stats;
    let mut summary = format!("PASS ({}) FAIL ({})", stats.expected, stats.unexpected);
    if stats.skipped > 0 {
        summary.push_str(&format!(" skipped ({})", stats.skipped));
    }
    let mut out = String::new();
    out.push_str(&summary);
    out.push('\n');

    let mut idx = 0usize;
    walk_failures(&json.suites, &mut idx, &mut out);

    out.push_str(&format!("\nTime: {}ms\n", stats.duration as u64));
    out
}

fn walk_failures(suites: &[PlaywrightSuite], idx: &mut usize, out: &mut String) {
    for suite in suites {
        let file = suite.file.as_deref().unwrap_or(&suite.title);
        for spec in &suite.specs {
            if !spec.ok {
                *idx += 1;
                emit_failure_block(spec, file, *idx, out);
            }
        }
        walk_failures(&suite.suites, idx, out);
    }
}

fn emit_failure_block(spec: &PlaywrightSpec, file: &str, idx: usize, out: &mut String) {
    // Prefer the "unexpected" execution; fall back to first if none marked
    // (defensive — Playwright always marks failed execs as unexpected).
    let Some(exec) = spec
        .tests
        .iter()
        .find(|t| t.status == "unexpected")
        .or_else(|| spec.tests.first())
    else {
        return;
    };
    // The FIRST attempt carries the richest error context (full message
    // with diff, stack, snippet). Retries usually re-emit a shortened
    // message. Use the first attempt for display; walk all attempts for
    // attachment dedup below.
    let Some(primary_attempt) = exec.results.first() else {
        return;
    };

    out.push_str(&format!("\n{}. {} ({})\n", idx, spec.title, file));

    if let Some(err) = primary_attempt.errors.first() {
        for line in err.message.lines() {
            out.push_str("   ");
            out.push_str(line);
            out.push('\n');
        }
        if let Some(stack) = &err.stack {
            let trimmed = trim_stack(stack);
            if !trimmed.is_empty() {
                for line in trimmed.lines() {
                    out.push_str("   ");
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }
        if let Some(snippet) = &err.snippet {
            let s = trim_snippet(snippet);
            if !s.is_empty() {
                out.push_str("   ── snippet ──\n");
                for line in s.lines() {
                    out.push_str("   ");
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }
    }

    // Attachments: dedupe by (name, path) across all attempts. Retry runs
    // typically re-emit the same artifact list — inflating token count for
    // no gain.
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut attachments: Vec<(String, String)> = Vec::new();
    for r in &exec.results {
        for a in &r.attachments {
            let Some(path) = &a.path else { continue };
            if seen.insert((a.name.clone(), path.clone())) {
                attachments.push((a.name.clone(), path.clone()));
            }
        }
    }
    if !attachments.is_empty() {
        out.push_str("   ── artifacts ──\n");
        for (name, path) in &attachments {
            let label = if name.is_empty() { "file" } else { name.as_str() };
            out.push_str(&format!("   {:12} {}\n", format!("{}:", label), path));
        }
    }

    // Combined stdout/stderr tail from all attempts, ANSI-stripped, capped.
    let mut combined = String::new();
    for r in &exec.results {
        for chunk in &r.stdout {
            if let Some(t) = &chunk.text {
                combined.push_str(t);
            }
        }
        for chunk in &r.stderr {
            if let Some(t) = &chunk.text {
                combined.push_str(t);
            }
        }
    }
    if !combined.trim().is_empty() {
        let clean = strip_ansi(&combined);
        let char_count = clean.chars().count();
        let skip = char_count.saturating_sub(STDOUT_TAIL_CAP);
        let tail: String = clean.chars().skip(skip).collect();
        let tail_trimmed = tail.trim();
        if !tail_trimmed.is_empty() {
            out.push_str("   ── stdout (tail) ──\n");
            for line in tail_trimmed.lines() {
                out.push_str("   ");
                out.push_str(line);
                out.push('\n');
            }
        }
    }

    // Retry note. Any duplicate attachment paths across retries were
    // deduped in the loop above; distinct retry paths are still listed.
    if exec.results.len() > 1 {
        out.push_str(&format!(
            "   retries: {} (artifact paths deduped by name+path)\n",
            exec.results.len() - 1
        ));
    }
}

/// Keep up to `MAX_STACK_FRAMES` frames. Frames whose path contains
/// `node_modules/` are dropped once at least one user frame is kept, since
/// they're rarely actionable in a filter context.
fn trim_stack(stack: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    for line in stack.lines() {
        let t = line.trim_start();
        if !t.starts_with("at ") {
            continue;
        }
        if line.contains("node_modules/") && !kept.is_empty() {
            continue;
        }
        kept.push(line);
        if kept.len() >= MAX_STACK_FRAMES {
            break;
        }
    }
    kept.join("\n")
}

/// Playwright's `snippet` field is a multi-line code excerpt with `>` marking
/// the failure line. Keep ±2 lines around the marker; if no marker, keep the
/// first 5 lines as a defensive fallback.
fn trim_snippet(snippet: &str) -> String {
    let lines: Vec<&str> = snippet.lines().collect();
    let marker = lines
        .iter()
        .position(|l| l.trim_start().starts_with('>'));
    let Some(m) = marker else {
        return lines.iter().take(5).copied().collect::<Vec<_>>().join("\n");
    };
    let start = m.saturating_sub(2);
    let end = (m + 3).min(lines.len());
    lines[start..end].join("\n")
}

pub fn run(args: &[String], verbose: u8) -> Result<i32> {
    let timer = tracking::TimedExecution::start();

    // Skip `which playwright` — it can find pyenv shims or other non-Node
    // binaries. Always resolve through the package manager.
    let pm = detect_package_manager();
    let mut cmd = match pm {
        "pnpm" => {
            let mut c = resolved_command("pnpm");
            c.arg("exec").arg("--").arg("playwright");
            c
        }
        "yarn" => {
            let mut c = resolved_command("yarn");
            c.arg("exec").arg("--").arg("playwright");
            c
        }
        _ => {
            let mut c = resolved_command("npx");
            c.arg("--no-install").arg("--").arg("playwright");
            c
        }
    };

    let is_test = args.first().map(|a| a == "test").unwrap_or(false);
    let is_install = is_install_subcommand(args);
    let honor_user_reporter = is_test && user_set_reporter(args);

    if is_test && !honor_user_reporter {
        cmd.arg("test");
        cmd.arg("--reporter=json");
        for arg in &args[1..] {
            cmd.arg(arg);
        }
    } else {
        for arg in args {
            cmd.arg(arg);
        }
    }

    if verbose > 0 {
        eprintln!("Running: playwright {}", args.join(" "));
    }

    let result = exec_capture(&mut cmd)
        .context("Failed to run playwright (try: npm install -g playwright)")?;

    let raw = format!("{}\n{}", result.stdout, result.stderr);

    let filtered = if honor_user_reporter {
        // User picked their reporter — passthrough. Filtering the wrong
        // format shape would corrupt their intended output.
        if verbose > 0 {
            eprintln!("playwright: honoring user --reporter, skipping RTK filter");
        }
        raw.clone()
    } else if is_install {
        filter_install(&raw)
    } else if is_test {
        match serde_json::from_str::<PlaywrightJsonOutput>(&result.stdout) {
            Ok(json) => {
                if verbose > 0 {
                    eprintln!("playwright test (Tier 1: rich JSON parse)");
                }
                format_rich_output(&json)
            }
            Err(_) => {
                // Fall back to the pre-existing Tier 2/3 pipeline.
                let parse_result = PlaywrightParser::parse(&result.stdout);
                let mode = FormatMode::from_verbosity(verbose);
                match parse_result {
                    ParseResult::Full(data) => data.format(mode),
                    ParseResult::Degraded(data, warnings) => {
                        if verbose > 0 {
                            emit_degradation_warning("playwright", &warnings.join(", "));
                        }
                        data.format(mode)
                    }
                    ParseResult::Passthrough(raw_p) => {
                        emit_passthrough_warning("playwright", "All parsing tiers failed");
                        raw_p
                    }
                }
            }
        }
    } else {
        // show-report, codegen, and other subcommands: passthrough.
        raw.clone()
    };

    let hint = crate::core::tee::tee_and_hint(&raw, "playwright", result.exit_code);
    let shown = crate::core::runner::emit_guarded(&filtered, hint.as_deref(), &raw);

    timer.track(
        &format!("playwright {}", args.join(" ")),
        &format!("rtk playwright {}", args.join(" ")),
        &raw,
        &shown,
    );

    if !result.success() {
        return Ok(result.exit_code);
    }

    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_playwright_parser_json() {
        // Real Playwright JSON structure: suites → specs, with float duration
        let json = r#"{
            "config": {},
            "stats": {
                "startTime": "2026-01-01T00:00:00.000Z",
                "expected": 1,
                "unexpected": 0,
                "skipped": 0,
                "flaky": 0,
                "duration": 7300.5
            },
            "suites": [
                {
                    "title": "auth",
                    "specs": [],
                    "suites": [
                        {
                            "title": "login.spec.ts",
                            "specs": [
                                {
                                    "title": "should login",
                                    "ok": true,
                                    "tests": [
                                        {
                                            "status": "expected",
                                            "results": [{"status": "passed", "errors": [], "duration": 2300}]
                                        }
                                    ]
                                }
                            ],
                            "suites": []
                        }
                    ]
                }
            ],
            "errors": []
        }"#;

        let result = PlaywrightParser::parse(json);
        assert_eq!(result.tier(), 1);
        assert!(result.is_ok());

        let data = result.unwrap();
        assert_eq!(data.passed, 1);
        assert_eq!(data.failed, 0);
        assert_eq!(data.duration_ms, Some(7300));
    }

    #[test]
    fn test_playwright_parser_json_float_duration() {
        // Real Playwright output uses float duration (e.g. 3519.7039999999997)
        let json = r#"{
            "stats": {
                "startTime": "2026-02-18T10:17:53.187Z",
                "expected": 4,
                "unexpected": 0,
                "skipped": 0,
                "flaky": 0,
                "duration": 3519.7039999999997
            },
            "suites": [],
            "errors": []
        }"#;

        let result = PlaywrightParser::parse(json);
        assert_eq!(result.tier(), 1);
        assert!(result.is_ok());

        let data = result.unwrap();
        assert_eq!(data.passed, 4);
        assert_eq!(data.duration_ms, Some(3519));
    }

    #[test]
    fn test_playwright_parser_json_with_failure() {
        let json = r#"{
            "stats": {
                "expected": 0,
                "unexpected": 1,
                "skipped": 0,
                "duration": 1500.0
            },
            "suites": [
                {
                    "title": "my.spec.ts",
                    "specs": [
                        {
                            "title": "should work",
                            "ok": false,
                            "tests": [
                                {
                                    "status": "unexpected",
                                    "results": [
                                        {
                                            "status": "failed",
                                            "errors": [{"message": "Expected true to be false"}],
                                            "duration": 500
                                        }
                                    ]
                                }
                            ]
                        }
                    ],
                    "suites": []
                }
            ],
            "errors": []
        }"#;

        let result = PlaywrightParser::parse(json);
        assert_eq!(result.tier(), 1);
        assert!(result.is_ok());

        let data = result.unwrap();
        assert_eq!(data.failed, 1);
        assert_eq!(data.failures.len(), 1);
        assert_eq!(data.failures[0].test_name, "should work");
        assert_eq!(data.failures[0].error_message, "Expected true to be false");
    }

    #[test]
    fn test_playwright_parser_regex_fallback() {
        let text = "3 passed (7.3s)";
        let result = PlaywrightParser::parse(text);
        assert_eq!(result.tier(), 2); // Degraded
        assert!(result.is_ok());

        let data = result.unwrap();
        assert_eq!(data.passed, 3);
        assert_eq!(data.failed, 0);
    }

    #[test]
    fn test_playwright_parser_passthrough() {
        let invalid = "random output";
        let result = PlaywrightParser::parse(invalid);
        assert_eq!(result.tier(), 3); // Passthrough
        assert!(!result.is_ok());
    }

    fn count_tokens(s: &str) -> usize {
        s.split_whitespace().count()
    }

    #[test]
    fn test_failure_emits_artifact_block() {
        let input = include_str!("../../../tests/fixtures/playwright_test_failed_with_artifacts.json");
        let json: PlaywrightJsonOutput = serde_json::from_str(input).expect("fixture must parse");
        let output = format_rich_output(&json);

        // Summary line
        assert!(output.starts_with("PASS (2) FAIL (1)"), "summary missing: {output}");

        // Artifact paths surfaced
        assert!(
            output.contains("test-results/auth-login-invalid/video.webm"),
            "video path must appear:\n{output}"
        );
        assert!(
            output.contains("test-results/auth-login-invalid/test-failed-1.png"),
            "screenshot path must appear:\n{output}"
        );
        assert!(
            output.contains("test-results/auth-login-invalid/trace.zip"),
            "trace path must appear:\n{output}"
        );

        // Snippet window around the marker
        assert!(output.contains("── snippet ──"), "snippet header missing:\n{output}");
        assert!(output.contains("> 44 |"), "marker line missing:\n{output}");

        // Duration
        assert!(output.contains("Time: 12480ms"), "duration missing");
    }

    #[test]
    fn test_retries_dedupe_attachments() {
        let input = include_str!("../../../tests/fixtures/playwright_test_failed_with_artifacts.json");
        let json: PlaywrightJsonOutput = serde_json::from_str(input).expect("fixture must parse");
        let output = format_rich_output(&json);

        // Both retry attempts share a video.webm — retry1 has its own path,
        // but the primary run's video should appear exactly once.
        let primary_video = "test-results/auth-login-invalid/video.webm";
        let count = output.matches(primary_video).count();
        assert_eq!(count, 1, "primary video path should appear once, not per attempt: {output}");

        // Retry note present
        assert!(
            output.contains("retries: 1"),
            "retry count line missing:\n{output}"
        );
    }

    #[test]
    fn test_stack_strips_node_modules() {
        let stack = "Error: boom\n    at userFn (/app/src/a.ts:10:1)\n    at userFn2 (/app/src/b.ts:20:1)\n    at nmFn (/app/node_modules/@playwright/test/lib/x.js:1:1)\n    at nmFn2 (/app/node_modules/deep/y.js:2:2)\n    at userFn3 (/app/src/c.ts:30:1)\n    at userFn4 (/app/src/d.ts:40:1)\n    at userFn5 (/app/src/e.ts:50:1)\n    at userFn6 (/app/src/f.ts:60:1)";
        let trimmed = trim_stack(stack);
        assert!(!trimmed.contains("node_modules/"), "node_modules frames should be dropped:\n{trimmed}");
        assert!(trimmed.lines().count() <= MAX_STACK_FRAMES, "frame cap violated");
        assert!(trimmed.contains("userFn ("), "first user frame must remain");
    }

    #[test]
    fn test_stack_keeps_lone_node_modules_frame() {
        // If the ONLY frame is in node_modules, we still keep it — better
        // than an empty stack.
        let stack = "Error: boom\n    at internalCall (/app/node_modules/foo/bar.js:1:1)";
        let trimmed = trim_stack(stack);
        assert!(!trimmed.is_empty(), "lone node_modules frame must be kept");
    }

    #[test]
    fn test_snippet_window_around_marker() {
        let snippet = "  40 | a\n  41 | b\n  42 | c\n> 43 | d\n     | ^\n  44 | e\n  45 | f\n  46 | g";
        let trimmed = trim_snippet(snippet);
        // Should include lines 41..=45 (marker ±2)
        assert!(trimmed.contains("41 | b"), "context above missing:\n{trimmed}");
        assert!(trimmed.contains("> 43 | d"), "marker missing:\n{trimmed}");
        assert!(trimmed.contains("45 | f") || trimmed.contains("     | ^"), "context below missing:\n{trimmed}");
        assert!(!trimmed.contains("46 | g"), "too much context kept:\n{trimmed}");
    }

    #[test]
    fn test_stdout_tail_only_on_failure() {
        let input = include_str!("../../../tests/fixtures/playwright_test_failed_with_artifacts.json");
        let json: PlaywrightJsonOutput = serde_json::from_str(input).expect("fixture must parse");
        let output = format_rich_output(&json);

        // Failed test's stdout tail present
        assert!(
            output.contains("Uncaught TypeError"),
            "failed test stdout must appear:\n{output}"
        );

        // Passing test's stdout ("should NOT appear") must be dropped
        assert!(
            !output.contains("should NOT appear"),
            "passing test stdout leaked into output:\n{output}"
        );
    }

    #[test]
    fn test_reporter_flag_disables_injection() {
        let args_a: [String; 2] = ["test".into(), "--reporter=list".into()];
        let args_b: [String; 3] = ["test".into(), "--reporter".into(), "line".into()];
        let args_c: [String; 2] = ["test".into(), "--headed".into()];

        assert!(user_set_reporter(&args_a), "--reporter=X must be detected");
        assert!(user_set_reporter(&args_b), "--reporter X must be detected");
        assert!(!user_set_reporter(&args_c), "unrelated flag must not trigger");
    }

    #[test]
    fn test_install_subcommand_detection() {
        let empty: [String; 0] = [];
        assert!(is_install_subcommand(&["install".to_string()]));
        assert!(is_install_subcommand(&["install-deps".to_string()]));
        assert!(!is_install_subcommand(&["test".to_string()]));
        assert!(!is_install_subcommand(&empty));
    }

    #[test]
    fn test_install_savings() {
        let input = include_str!("../../../tests/fixtures/playwright_install_raw.txt");
        let output = filter_install(input);

        let raw_tokens = count_tokens(input);
        let filtered_tokens = count_tokens(&output);
        let savings = 100.0 - (filtered_tokens as f64 / raw_tokens as f64 * 100.0);

        assert!(
            savings >= 60.0,
            "playwright install: expected ≥60% savings, got {:.1}% ({} → {} tokens)\n---\n{}",
            savings,
            raw_tokens,
            filtered_tokens,
            output
        );
    }

    #[test]
    fn test_install_structural() {
        let input = include_str!("../../../tests/fixtures/playwright_install_raw.txt");
        let output = filter_install(input);

        // Actionable lines preserved
        assert!(output.contains("Chromium 123.0.6312.4"), "chromium version line missing:\n{output}");
        assert!(output.contains("Firefox 124.0"), "firefox line missing:\n{output}");
        assert!(output.contains("Webkit 17.4"), "webkit line missing:\n{output}");
        assert!(output.contains("Host validation warning"), "warning line missing:\n{output}");
        assert!(output.contains("libgtk-4.so.1"), "dependency detail must survive");

        // Progress-bar noise stripped
        assert!(!output.contains("| 0% of"), "progress bar leaked:\n{output}");
        assert!(!output.contains("|====="), "progress bar leaked");
    }

    #[test]
    fn test_install_fallback_on_empty_filter() {
        // If every line matches drop rules and nothing survives, fall back
        // to raw — never emit an empty filter result.
        let raw = "|                        | 0% of 10 MiB\n|====                    | 12% of 10 MiB";
        let out = filter_install(raw);
        assert!(!out.is_empty(), "must not return empty output");
    }

    #[test]
    fn test_rich_savings_against_raw_json() {
        let input = include_str!("../../../tests/fixtures/playwright_test_failed_with_artifacts.json");
        let json: PlaywrightJsonOutput = serde_json::from_str(input).expect("fixture must parse");
        let output = format_rich_output(&json);

        let raw_tokens = count_tokens(input);
        let filtered_tokens = count_tokens(&output);
        let savings = 100.0 - (filtered_tokens as f64 / raw_tokens as f64 * 100.0);

        // Small dense fixture — savings floor is 40% here. Real workloads
        // (dozens of tests, per-step data) hit 90%+ easily.
        assert!(
            savings >= 40.0,
            "playwright rich format savings floor: got {:.1}% ({} → {})\n---\n{}",
            savings,
            raw_tokens,
            filtered_tokens,
            output
        );
    }
}
