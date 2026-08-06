//! Root-cause extraction from Lake and Lean build output.

use std::collections::BTreeSet;

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildRootCauseKind {
    LeanDiagnostic,
    LakeError,
    FailedJob,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct BuildRootCause {
    pub kind: BuildRootCauseKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<u64>,
    pub message: String,
}

#[derive(Debug, Default)]
pub struct BuildDiagnostics {
    pub root_causes: Vec<BuildRootCause>,
    pub failed_targets: Vec<String>,
}

pub fn parse(stdout: &[u8], stderr: &[u8]) -> BuildDiagnostics {
    let mut parser = Parser::default();
    parser.read(&String::from_utf8_lossy(stdout));
    parser.read(&String::from_utf8_lossy(stderr));
    parser.finish()
}

#[derive(Default)]
struct Parser {
    causes: Vec<BuildRootCause>,
    seen_causes: BTreeSet<BuildRootCause>,
    failed_jobs: Vec<BuildRootCause>,
    seen_failed_jobs: BTreeSet<BuildRootCause>,
    failed_targets: Vec<String>,
    seen_targets: BTreeSet<String>,
}

impl Parser {
    fn read(&mut self, output: &str) {
        let mut reading_failed_targets = false;
        for raw_line in output.lines() {
            let line = strip_ansi(raw_line);
            let line = line.trim();

            if is_failed_targets_header(line) {
                reading_failed_targets = true;
                continue;
            }
            if reading_failed_targets {
                if let Some(target) = line.strip_prefix("- ").map(str::trim) {
                    self.push_target(target);
                    continue;
                }
                if line.is_empty() {
                    continue;
                }
                reading_failed_targets = false;
            }

            if let Some((action, target)) = parse_failed_job(line) {
                self.push_target(&target);
                self.push_failed_job(BuildRootCause {
                    kind: BuildRootCauseKind::FailedJob,
                    target: Some(target.clone()),
                    file: None,
                    line: None,
                    column: None,
                    message: format!("{action} {target}"),
                });
                continue;
            }
            if let Some(cause) = parse_lean_diagnostic(line) {
                self.push_cause(cause);
                continue;
            }
            if let Some(message) = line.strip_prefix("error: ").map(str::trim)
                && !is_generic_error(message)
            {
                self.push_cause(BuildRootCause {
                    kind: BuildRootCauseKind::LakeError,
                    target: None,
                    file: None,
                    line: None,
                    column: None,
                    message: message.to_owned(),
                });
            }
        }
    }

    fn push_cause(&mut self, cause: BuildRootCause) {
        if self.seen_causes.insert(cause.clone()) {
            self.causes.push(cause);
        }
    }

    fn push_failed_job(&mut self, cause: BuildRootCause) {
        if self.seen_failed_jobs.insert(cause.clone()) {
            self.failed_jobs.push(cause);
        }
    }

    fn push_target(&mut self, target: &str) {
        if !target.is_empty() && self.seen_targets.insert(target.to_owned()) {
            self.failed_targets.push(target.to_owned());
        }
    }

    fn finish(self) -> BuildDiagnostics {
        BuildDiagnostics {
            root_causes: if self.causes.is_empty() {
                self.failed_jobs
            } else {
                self.causes
            },
            failed_targets: self.failed_targets,
        }
    }
}

fn is_failed_targets_header(line: &str) -> bool {
    matches!(
        line,
        "Some required targets logged failures:" | "Some required builds logged failures:"
    )
}

fn parse_failed_job(line: &str) -> Option<(String, String)> {
    let line = line.strip_prefix("✖ [")?;
    let (progress, detail) = line.split_once("] ")?;
    let (completed, total) = progress.split_once('/')?;
    if completed.parse::<usize>().is_err() || total.parse::<usize>().is_err() {
        return None;
    }
    let detail = detail.strip_prefix("(Optional) ").unwrap_or(detail);
    let detail = strip_duration(detail);
    let (action, target) = detail.split_once(' ')?;
    if !matches!(
        action,
        "Building" | "Fetching" | "Replaying" | "Reusing" | "Running" | "Unpacking"
    ) || target.is_empty()
    {
        return None;
    }
    Some((action.to_owned(), target.to_owned()))
}

fn strip_duration(detail: &str) -> &str {
    let Some((caption, duration)) = detail.rsplit_once(" (") else {
        return detail;
    };
    let Some(duration) = duration.strip_suffix(')') else {
        return detail;
    };
    let numeric = duration
        .strip_suffix("ms")
        .or_else(|| duration.strip_suffix("us"))
        .or_else(|| duration.strip_suffix("μs"))
        .or_else(|| duration.strip_suffix("ns"))
        .or_else(|| duration.strip_suffix('s'));
    if numeric.is_some_and(|value| value.trim().parse::<f64>().is_ok()) {
        caption
    } else {
        detail
    }
}

fn parse_lean_diagnostic(line: &str) -> Option<BuildRootCause> {
    let (location, message) = line.split_once(": error: ")?;
    let (file_and_line, column) = location.rsplit_once(':')?;
    let (file, line) = file_and_line.rsplit_once(':')?;
    let line = line.parse::<u64>().ok()?;
    let column = column.parse::<u64>().ok()?;
    if file.is_empty() || message.trim().is_empty() {
        return None;
    }
    Some(BuildRootCause {
        kind: BuildRootCauseKind::LeanDiagnostic,
        target: None,
        file: Some(file.to_owned()),
        line: Some(line),
        column: Some(column),
        message: message.trim().to_owned(),
    })
}

fn is_generic_error(message: &str) -> bool {
    message == "build failed"
        || message.starts_with("Lean exited with code ")
        || (message.starts_with("external command ") && message.contains(" exited with code "))
}

fn strip_ansi(line: &str) -> String {
    let mut clean = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for control in chars.by_ref() {
                if control.is_ascii() && ('@'..='~').contains(&control) {
                    break;
                }
            }
        } else {
            clean.push(character);
        }
    }
    clean
}

#[cfg(test)]
mod tests {
    use super::{BuildRootCauseKind, parse};

    #[test]
    fn extracts_and_deduplicates_lean_errors_and_failed_targets() {
        let output = concat!(
            "\u{1b}[31m✖ [2/5] Building Root (1.2s)\u{1b}[0m\n",
            "Root.lean:12:4: error: unknown identifier 'missing'\n",
            "Some required targets logged failures:\n",
            "- Root\n",
            "- Root\n",
            "error: build failed\n",
        );
        let diagnostics = parse(output.as_bytes(), output.as_bytes());

        assert_eq!(diagnostics.failed_targets, ["Root"]);
        assert_eq!(diagnostics.root_causes.len(), 1);
        let cause = &diagnostics.root_causes[0];
        assert_eq!(cause.kind, BuildRootCauseKind::LeanDiagnostic);
        assert_eq!(cause.file.as_deref(), Some("Root.lean"));
        assert_eq!(cause.line, Some(12));
        assert_eq!(cause.column, Some(4));
        assert_eq!(cause.message, "unknown identifier 'missing'");
    }

    #[test]
    fn keeps_non_generic_lake_errors_and_windows_locations() {
        let output = concat!(
            "C:\\work\\Root.lean:8:2: error: type mismatch\n",
            "error: package configuration is invalid\n",
            "error: Lean exited with code 1\n",
        );
        let diagnostics = parse(output.as_bytes(), &[]);

        assert_eq!(diagnostics.root_causes.len(), 2);
        assert_eq!(
            diagnostics.root_causes[0].file.as_deref(),
            Some("C:\\work\\Root.lean")
        );
        assert_eq!(
            diagnostics.root_causes[1].kind,
            BuildRootCauseKind::LakeError
        );
        assert_eq!(
            diagnostics.root_causes[1].message,
            "package configuration is invalid"
        );
    }

    #[test]
    fn falls_back_to_the_failed_job_when_lake_logs_no_error() {
        let output = "✖ [1/1] Fetching dependency:foo (20ms)\nerror: build failed\n";
        let diagnostics = parse(output.as_bytes(), &[]);

        assert_eq!(diagnostics.failed_targets, ["dependency:foo"]);
        assert_eq!(diagnostics.root_causes.len(), 1);
        assert_eq!(
            diagnostics.root_causes[0].kind,
            BuildRootCauseKind::FailedJob
        );
        assert_eq!(
            diagnostics.root_causes[0].message,
            "Fetching dependency:foo"
        );
    }
}
