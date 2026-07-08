//! Filters Prettier output to show only files that need formatting.

use crate::core::runner::{self, RunOptions};
use crate::core::truncate::CAP_WARNINGS;
use crate::core::utils::package_manager_exec;
use anyhow::Result;

pub fn run(args: &[String], verbose: u8) -> Result<i32> {
    let mut cmd = package_manager_exec("prettier");

    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: prettier {}", args.join(" "));
    }

    runner::run_filtered(
        cmd,
        "prettier",
        &args.join(" "),
        filter_prettier_output,
        RunOptions::default(),
    )
}

fn has_source_extension(s: &str) -> bool {
    s.ends_with(".ts")
        || s.ends_with(".tsx")
        || s.ends_with(".js")
        || s.ends_with(".jsx")
        || s.ends_with(".mjs")
        || s.ends_with(".cjs")
        || s.ends_with(".json")
        || s.ends_with(".md")
        || s.ends_with(".mdx")
        || s.ends_with(".yml")
        || s.ends_with(".yaml")
        || s.ends_with(".html")
        || s.ends_with(".vue")
        || s.ends_with(".svelte")
        || s.ends_with(".css")
        || s.ends_with(".scss")
        || s.ends_with(".less")
}

/// Filter Prettier output - show only files that need formatting.
///
/// Prettier 2 wrote file paths as bare lines to stdout. Prettier 3 writes them
/// to stderr prefixed with `[warn]` (e.g. `[warn] src/foo.ts`) plus a summary
/// line `[warn] Code style issues found in N file(s)...`. Both must be
/// recognized so failing files are not silently swallowed (#2878).
pub fn filter_prettier_output(output: &str) -> String {
    // #221: empty or whitespace-only output means prettier didn't run
    if output.trim().is_empty() {
        return "Error: prettier produced no output".to_string();
    }

    let mut files_to_format: Vec<String> = Vec::new();
    let mut files_checked = 0;
    let mut is_check_mode = true;
    let mut prettier3_issues_summary = false;

    for line in output.lines() {
        let trimmed = line.trim();

        // Detect check mode vs write mode
        if trimmed.contains("Checking formatting") {
            is_check_mode = true;
        }

        // Prettier 3 emits `[warn] Code style issues found in N file(s)...` as
        // the failure summary. Track it so an empty file list still surfaces as
        // a failure rather than "all formatted correctly".
        if trimmed.starts_with("[warn] Code style") || trimmed.starts_with("[error]") {
            prettier3_issues_summary = true;
            continue;
        }

        // Prettier 3: `[warn] path/to/file.ts` means the file needs formatting.
        let unprefixed = trimmed
            .strip_prefix("[warn] ")
            .or_else(|| trimmed.strip_prefix("[error] "))
            .unwrap_or(trimmed);

        // Count files that need formatting (check mode)
        if !unprefixed.is_empty()
            && !unprefixed.starts_with("Checking")
            && !unprefixed.starts_with("All matched")
            && !unprefixed.starts_with("Code style")
            && has_source_extension(unprefixed)
        {
            files_to_format.push(unprefixed.to_string());
        }

        // Count total files checked
        if trimmed.contains("All matched files use Prettier") {
            if let Some(count_str) = trimmed.split_whitespace().next() {
                if let Ok(count) = count_str.parse::<usize>() {
                    files_checked = count;
                }
            }
        }
    }

    // Check if all files are formatted
    if files_to_format.is_empty()
        && !prettier3_issues_summary
        && output.contains("All matched files use Prettier")
    {
        return "Prettier: All files formatted correctly".to_string();
    }

    // Prettier 3 with issues but somehow no captured files (e.g. --list-different
    // suppressed): make the failure explicit rather than silently claiming success.
    if files_to_format.is_empty() && prettier3_issues_summary {
        return "Prettier: code style issues found (see raw output for details)".to_string();
    }

    // Check if files were written (write mode)
    if output.contains("modified") || output.contains("formatted") {
        is_check_mode = false;
    }

    let mut result = String::new();

    if is_check_mode {
        // Check mode: show files that need formatting
        if files_to_format.is_empty() {
            result.push_str("Prettier: All files formatted correctly\n");
        } else {
            result.push_str(&format!(
                "Prettier: {} files need formatting\n",
                files_to_format.len()
            ));

            const MAX_PRETTIER_FILES: usize = CAP_WARNINGS;
            for (i, file) in files_to_format.iter().take(MAX_PRETTIER_FILES).enumerate() {
                result.push_str(&format!("{}. {}\n", i + 1, file));
            }

            if files_to_format.len() > MAX_PRETTIER_FILES {
                result.push_str(&format!(
                    "\n... +{} more files\n",
                    files_to_format.len() - MAX_PRETTIER_FILES
                ));
            }

            if files_checked > 0 {
                result.push_str(&format!(
                    "\n{} files already formatted\n",
                    files_checked - files_to_format.len()
                ));
            }
        }
    } else {
        // Write mode: show what was formatted
        result.push_str(&format!(
            "Prettier: {} files formatted\n",
            files_to_format.len()
        ));
    }

    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_all_formatted() {
        let output = r#"
Checking formatting...
All matched files use Prettier code style!
        "#;
        let result = filter_prettier_output(output);
        assert!(result.contains("Prettier"));
        assert!(result.contains("All files formatted correctly"));
    }

    #[test]
    fn test_filter_files_need_formatting() {
        let output = r#"
Checking formatting...
src/components/ui/button.tsx
src/lib/auth/session.ts
src/pages/dashboard.tsx
Code style issues found in the above file(s). Forgot to run Prettier?
        "#;
        let result = filter_prettier_output(output);
        assert!(result.contains("3 files need formatting"));
        assert!(result.contains("button.tsx"));
        assert!(result.contains("session.ts"));
    }

    #[test]
    fn test_filter_many_files() {
        let mut output = String::from("Checking formatting...\n");
        for i in 0..15 {
            output.push_str(&format!("src/file{}.ts\n", i));
        }
        let result = filter_prettier_output(&output);
        assert!(result.contains("15 files need formatting"));
        assert!(result.contains("... +5 more files"));
    }

    // --- #221: empty output should not say "All files formatted" ---

    #[test]
    fn test_filter_empty_output() {
        let result = filter_prettier_output("");
        assert!(result.contains("Error"));
        assert!(!result.contains("All files formatted"));
    }

    #[test]
    fn test_filter_whitespace_only_output() {
        let result = filter_prettier_output("   \n\n  ");
        assert!(result.contains("Error"));
        assert!(!result.contains("All files formatted"));
    }

    // --- #2878: Prettier 3.x writes failures to stderr as [warn] lines ---

    #[test]
    fn test_filter_prettier3_warn_files_are_surfaced() {
        // Real Prettier 3 output (goes to stderr, hence why RunOptions::default
        // is required to see it in the filter input).
        let output = r#"Checking formatting...
[warn] src/components/ui/button.tsx
[warn] src/lib/auth/session.ts
[warn] src/pages/dashboard.tsx
[warn] Code style issues found in 3 files. Run Prettier with --write to fix.
"#;
        let result = filter_prettier_output(output);
        assert!(
            result.contains("3 files need formatting"),
            "expected 3 failing files, got: {result}"
        );
        assert!(result.contains("button.tsx"));
        assert!(result.contains("session.ts"));
        assert!(result.contains("dashboard.tsx"));
        assert!(
            !result.contains("All files formatted"),
            "must not report success when Prettier 3 flagged issues"
        );
    }

    #[test]
    fn test_filter_prettier3_summary_without_files() {
        // Edge case: --list-different or similar shows only the summary.
        // Must not claim success.
        let output = "[warn] Code style issues found in 2 files. Run Prettier with --write to fix.";
        let result = filter_prettier_output(output);
        assert!(!result.contains("All files formatted"));
        assert!(result.to_lowercase().contains("issues"));
    }

    #[test]
    fn test_filter_prettier3_all_good() {
        // Prettier 3 success path — no [warn] lines, "All matched" summary.
        let output = "Checking formatting...\nAll matched files use Prettier code style!";
        let result = filter_prettier_output(output);
        assert!(result.contains("All files formatted correctly"));
    }

    // --- Token savings ---

    fn count_tokens(s: &str) -> usize {
        s.split_whitespace().count()
    }

    #[test]
    fn test_prettier_savings_over_60_percent() {
        // Realistic Prettier 3 failure output for a mid-sized project.
        let mut input = String::from("Checking formatting...\n");
        for i in 0..40 {
            input.push_str(&format!("[warn] src/module{i}/component{i}.tsx\n"));
        }
        input.push_str(
            "[warn] Code style issues found in 40 files. Run Prettier with --write to fix.\n",
        );
        let output = filter_prettier_output(&input);
        let input_tokens = count_tokens(&input);
        let output_tokens = count_tokens(&output);
        let savings = 100.0 - (output_tokens as f64 / input_tokens as f64 * 100.0);
        assert!(
            savings >= 60.0,
            "Prettier filter: expected >=60% savings, got {savings:.1}%"
        );
    }
}
