//! What Lake's build output says: job lines, failed targets, and root causes.
//!
//! Lake prints one line per finished job when its output is not a terminal:
//!
//! ```text
//! ✔ [12/40] Built Mathlib.Order.Basic (1.5s)
//! ✖ [13/40] (Optional) Building Docs.Main (20ms)
//! ```

use std::collections::BTreeSet;

use serde::Serialize;

/// The verb of a job line, without its failure form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobAction {
    Built,
    Fetched,
    Unpacked,
    Replayed,
    Reused,
    Ran,
    Canceled,
}

/// One finished Lake job.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JobLine {
    pub action: JobAction,
    pub target: String,
    pub elapsed_ms: Option<f64>,
    pub failed: bool,
    pub optional: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RootCauseKind {
    LeanDiagnostic,
    LakeError,
    FailedJob,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct RootCause {
    pub kind: RootCauseKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<u64>,
    pub message: String,
}

/// Everything read from the build's output streams.
#[derive(Debug, Default)]
pub struct BuildOutput {
    pub jobs: Vec<JobLine>,
    /// Lean and Lake errors, or the failed jobs when no error was printed.
    pub root_causes: Vec<RootCause>,
    pub failed_targets: Vec<String>,
}

/// Reads one output stream a line at a time.
#[derive(Debug, Default)]
pub struct OutputReader {
    jobs: Vec<JobLine>,
    errors: Vec<RootCause>,
    failed_jobs: Vec<RootCause>,
    failed_targets: Vec<String>,
    in_failed_targets: bool,
}

impl OutputReader {
    pub fn line(&mut self, raw: &str) {
        let line = strip_ansi(raw);
        let line = line.trim();

        if matches!(
            line,
            "Some required targets logged failures:" | "Some required builds logged failures:"
        ) {
            self.in_failed_targets = true;
            return;
        }
        if self.in_failed_targets {
            if let Some(target) = line.strip_prefix("- ").map(str::trim) {
                self.failed_targets.push(target.to_owned());
                return;
            }
            if line.is_empty() {
                return;
            }
            self.in_failed_targets = false;
        }

        if let Some(job) = parse_job_line(line) {
            if job.failed {
                self.failed_targets.push(job.target.clone());
                self.failed_jobs.push(RootCause {
                    kind: RootCauseKind::FailedJob,
                    file: None,
                    line: None,
                    column: None,
                    message: format!("{} {}", failure_verb(job.action), job.target),
                });
            }
            self.jobs.push(job);
        } else if let Some(cause) = parse_error(line) {
            self.errors.push(cause);
        }
    }
}

/// Merge the readers of the build's output streams, in order.
pub fn combine(readers: impl IntoIterator<Item = OutputReader>) -> BuildOutput {
    let mut output = BuildOutput::default();
    let mut errors = Vec::new();
    let mut failed_jobs = Vec::new();
    for reader in readers {
        output.jobs.extend(reader.jobs);
        errors.extend(reader.errors);
        failed_jobs.extend(reader.failed_jobs);
        output.failed_targets.extend(reader.failed_targets);
    }
    output.root_causes = unique(if errors.is_empty() {
        failed_jobs
    } else {
        errors
    });
    output.failed_targets = unique(output.failed_targets);
    output
}

/// Parse `{icon} [n/m]{ (Optional)} {verb} {caption}{ (time)}`.
pub fn parse_job_line(line: &str) -> Option<JobLine> {
    let mut characters = line.chars();
    if characters.next()?.is_ascii() {
        return None;
    }
    let rest = characters.as_str().strip_prefix(" [")?;
    let (progress, rest) = rest.split_once(']')?;
    let (done, total) = progress.split_once('/')?;
    if done.parse::<u64>().is_err() || total.parse::<u64>().is_err() {
        return None;
    }
    let (optional, rest) = match rest.strip_prefix(" (Optional) ") {
        Some(rest) => (true, rest),
        None => (false, rest.strip_prefix(' ')?),
    };
    let (verb, caption) = rest.split_once(' ')?;
    let (action, failed) = match verb {
        "Built" => (JobAction::Built, false),
        "Building" => (JobAction::Built, true),
        "Fetched" => (JobAction::Fetched, false),
        "Fetching" => (JobAction::Fetched, true),
        "Unpacked" => (JobAction::Unpacked, false),
        "Unpacking" => (JobAction::Unpacked, true),
        "Replayed" => (JobAction::Replayed, false),
        "Replaying" => (JobAction::Replayed, true),
        "Reused" => (JobAction::Reused, false),
        "Reusing" => (JobAction::Reused, true),
        "Ran" => (JobAction::Ran, false),
        "Running" => (JobAction::Ran, true),
        "Canceled" => (JobAction::Canceled, false),
        _ => return None,
    };
    let (target, elapsed_ms) = match caption.rsplit_once(" (") {
        Some((target, time)) => match time.strip_suffix(')').and_then(parse_time) {
            Some(ms) => (target, Some(ms)),
            None => (caption, None),
        },
        None => (caption, None),
    };
    if target.is_empty() {
        return None;
    }
    Some(JobLine {
        action,
        target: target.to_owned(),
        elapsed_ms,
        failed,
        optional,
    })
}

/// Lake prints `12ms` below a second, `1.5s` below ten, and whole seconds above.
fn parse_time(value: &str) -> Option<f64> {
    let (number, scale) = match value.strip_suffix("ms") {
        Some(number) => (number, 1.0),
        None => (value.strip_suffix('s')?, 1000.0),
    };
    if number.is_empty()
        || !number
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return None;
    }
    number.parse::<f64>().ok().map(|number| number * scale)
}

fn failure_verb(action: JobAction) -> &'static str {
    match action {
        JobAction::Built => "Building",
        JobAction::Fetched => "Fetching",
        JobAction::Unpacked => "Unpacking",
        JobAction::Replayed => "Replaying",
        JobAction::Reused => "Reusing",
        JobAction::Ran => "Running",
        JobAction::Canceled => "Canceled",
    }
}

/// Read an error line. Lake writes Lean errors as `error: FILE:LINE:COL: MSG`;
/// older toolchains wrote `FILE:LINE:COL: error: MSG`.
fn parse_error(line: &str) -> Option<RootCause> {
    if let Some(message) = line.strip_prefix("error: ") {
        let message = message.trim();
        if let Some(cause) = parse_position(message) {
            return Some(cause);
        }
        let generic = message == "build failed"
            || message.starts_with("Lean exited with code ")
            || (message.starts_with("external command ") && message.contains(" exited with code "));
        return (!generic && !message.is_empty()).then(|| RootCause {
            kind: RootCauseKind::LakeError,
            file: None,
            line: None,
            column: None,
            message: message.to_owned(),
        });
    }
    let (location, message) = line.split_once(": error: ")?;
    parse_position(&format!("{location}: {message}"))
}

fn parse_position(text: &str) -> Option<RootCause> {
    let (location, message) = text.split_once(": ")?;
    let (rest, column) = location.rsplit_once(':')?;
    let (file, line) = rest.rsplit_once(':')?;
    let line = line.parse::<u64>().ok()?;
    let column = column.parse::<u64>().ok()?;
    let message = message.trim();
    if file.is_empty() || message.is_empty() {
        return None;
    }
    Some(RootCause {
        kind: RootCauseKind::LeanDiagnostic,
        file: Some(file.to_owned()),
        line: Some(line),
        column: Some(column),
        message: message.to_owned(),
    })
}

fn unique<T: Ord + Clone>(values: Vec<T>) -> Vec<T> {
    let mut seen = BTreeSet::new();
    values
        .into_iter()
        .filter(|value| seen.insert(value.clone()))
        .collect()
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
    use super::{JobAction, OutputReader, RootCauseKind, combine, parse_job_line};

    fn read(lines: &str) -> OutputReader {
        let mut reader = OutputReader::default();
        for line in lines.lines() {
            reader.line(line);
        }
        reader
    }

    #[test]
    fn parses_job_lines_with_times_and_markers() {
        let built = parse_job_line("✔ [1/3] Built Root (12ms)").unwrap();
        assert_eq!(built.action, JobAction::Built);
        assert_eq!(built.target, "Root");
        assert_eq!(built.elapsed_ms, Some(12.0));
        assert!(!built.failed && !built.optional);

        let replayed = parse_job_line("ℹ [2/3] Replayed Mathlib").unwrap();
        assert_eq!(replayed.action, JobAction::Replayed);
        assert_eq!(replayed.elapsed_ms, None);

        let ran = parse_job_line("✔ [3/3] Ran job computation").unwrap();
        assert_eq!(ran.action, JobAction::Ran);
        assert_eq!(ran.target, "job computation");

        let slow = parse_job_line("✔ [4/9] Reused Mathlib (1.5s)").unwrap();
        assert_eq!(slow.elapsed_ms, Some(1500.0));
        let slower = parse_job_line("✔ [5/9] Unpacked Mathlib (12s)").unwrap();
        assert_eq!(slower.action, JobAction::Unpacked);
        assert_eq!(slower.elapsed_ms, Some(12000.0));

        let optional = parse_job_line("✔ [6/9] (Optional) Built Docs.Main (20ms)").unwrap();
        assert!(optional.optional);
        assert_eq!(optional.target, "Docs.Main");

        let failed = parse_job_line("✖ [7/9] Building Root (15ms)").unwrap();
        assert_eq!(failed.action, JobAction::Built);
        assert!(failed.failed);

        let canceled = parse_job_line("⊘ [8/9] Canceled Root.Slow").unwrap();
        assert_eq!(canceled.action, JobAction::Canceled);
        assert!(!canceled.failed);

        let odd_time = parse_job_line("✔ [9/9] Built Root (12us)").unwrap();
        assert_eq!(odd_time.target, "Root (12us)");
        assert_eq!(odd_time.elapsed_ms, None);

        assert!(parse_job_line("prefix] Built Root (12ms)").is_none());
        assert!(parse_job_line("✔ [one/3] Built Root (12ms)").is_none());
        assert!(parse_job_line("✔ [1/3] Compiled Root").is_none());
        assert!(parse_job_line("info: [1/3] Built Root").is_none());
    }

    #[test]
    fn extracts_and_deduplicates_lean_errors_and_failed_targets() {
        let output = concat!(
            "\u{1b}[31m✖ [2/5] Building Root (1.2s)\u{1b}[0m\n",
            "error: Root.lean:12:4: unknown identifier 'missing'\n",
            "Root.lean:12:4: error: unknown identifier 'missing'\n",
            "Some required targets logged failures:\n",
            "- Root\n",
            "- Root\n",
            "error: build failed\n",
        );
        let output = combine([read(output), read(output)]);

        assert_eq!(output.failed_targets, ["Root"]);
        assert_eq!(output.root_causes.len(), 1);
        let cause = &output.root_causes[0];
        assert_eq!(cause.kind, RootCauseKind::LeanDiagnostic);
        assert_eq!(cause.file.as_deref(), Some("Root.lean"));
        assert_eq!(cause.line, Some(12));
        assert_eq!(cause.column, Some(4));
        assert_eq!(cause.message, "unknown identifier 'missing'");
        assert_eq!(output.jobs.len(), 2);
    }

    #[test]
    fn keeps_specific_lake_errors_and_windows_locations() {
        let output = combine([read(concat!(
            "error: C:\\work\\Root.lean:8:2: type mismatch\n",
            "error: package configuration is invalid\n",
            "error: Lean exited with code 1\n",
        ))]);

        assert_eq!(output.root_causes.len(), 2);
        assert_eq!(
            output.root_causes[0].file.as_deref(),
            Some("C:\\work\\Root.lean")
        );
        assert_eq!(output.root_causes[1].kind, RootCauseKind::LakeError);
        assert_eq!(
            output.root_causes[1].message,
            "package configuration is invalid"
        );
    }

    #[test]
    fn falls_back_to_the_failed_job_when_lake_logs_no_error() {
        let output = combine([read(
            "✖ [1/1] Fetching dependency:foo (20ms)\nerror: build failed\n",
        )]);

        assert_eq!(output.failed_targets, ["dependency:foo"]);
        assert_eq!(output.root_causes.len(), 1);
        assert_eq!(output.root_causes[0].kind, RootCauseKind::FailedJob);
        assert_eq!(output.root_causes[0].message, "Fetching dependency:foo");
    }
}
