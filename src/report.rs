//! What a run prints: one line per Host, plus a summary of the run.
//!
//! stdout carries one line per Host and stderr carries the human chrome, so the
//! two are coloured independently: stdout is often a pipe while stderr is a
//! terminal.

use std::io::Write;
use std::time::Duration;

use anstream::AutoStream;
use anstyle::{AnsiColor, Style};
use clap::ValueEnum;

use crate::cause::Cause;
use crate::run::{Outcome, Status};

/// `--color`'s values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ColorWhen {
    /// Colour when the stream being written is a terminal, and `NO_COLOR` is
    /// unset. This is the default.
    Auto,
    /// Always colour, even into a pipe.
    Always,
    /// Never colour.
    Never,
}

/// A stream that decides its own colouring, from the terminal it is attached
/// to, unless `--color` overrides that.
fn stream<T: anstream::stream::RawStream>(when: ColorWhen, raw: T) -> AutoStream<T> {
    match when {
        ColorWhen::Auto => AutoStream::auto(raw),
        ColorWhen::Always => AutoStream::always(raw),
        ColorWhen::Never => AutoStream::never(raw),
    }
}

/// Which streams the report shows.
#[derive(Debug, Clone, Copy, Default)]
pub struct Detail {
    /// Show stdout. Without it, stdout is shown only for a Host that is not
    /// `ok` — an `ok` Host's output is the thing a wide run must not drown in.
    pub stdout: bool,
    /// Show the stderr of a Host that is `ok`. A Host that is not `ok` always
    /// shows its stderr, because that is where the reason is.
    pub stderr: bool,
}

/// How a Host's result is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    /// One line per Host, with its output as an indented block.
    Plain,
    /// One JSON object per Host, per line.
    Json,
}

/// Prints a run's report.
pub struct Reporter {
    stdout: AutoStream<std::io::Stdout>,
    stderr: AutoStream<std::io::Stderr>,
    detail: Detail,
    format: Format,
    /// Whether the run's chrome — the heading, and the heartbeat the caller
    /// draws — belongs on stderr. True only when stderr is a terminal: a
    /// redirected stderr is a file a heading would only pollute.
    chrome: bool,
    styles: Styles,
}

impl Reporter {
    pub fn new(when: ColorWhen, detail: Detail, format: Format, chrome: bool) -> Reporter {
        Reporter {
            // JSON is consumed by a program, so it is never coloured, whatever
            // the terminal it happens to be written to.
            stdout: if format == Format::Json {
                AutoStream::never(std::io::stdout())
            } else {
                stream(when, std::io::stdout())
            },
            stderr: stream(when, std::io::stderr()),
            detail,
            format,
            chrome,
            styles: Styles::new(),
        }
    }

    /// One line about rshx's own doing, before anything has run.
    ///
    /// Not chrome: the heading is about the run and is dropped when nobody is
    /// watching, while a warning is about the command rshx was asked to run and
    /// belongs wherever a run's diagnostics go. It is written through the same
    /// stream as the rest, so `--color` decides whether it is coloured.
    pub fn warning(&mut self, message: &str) {
        let _ = writeln!(
            self.stderr,
            "rshx: {}warning:{} {message}",
            self.styles.warn.render(),
            self.styles.warn.render_reset()
        );
    }

    /// One line naming what is about to run, so the report has a heading.
    ///
    /// This is chrome, not report: it goes to stderr, and only when stderr is a
    /// terminal. Being written before the first Host settles, it also says how
    /// many Hosts the run selected — which `-g` can otherwise leave unclear
    /// until the summary.
    pub fn heading(&mut self, command: &[String], hosts: usize, fanout: u32) {
        if !self.chrome {
            return;
        }
        let host_word = if hosts == 1 { "host" } else { "hosts" };
        // Only worth saying when it is the fanout, not the number of Hosts,
        // that decides how many run at once.
        let fanout = if (fanout as usize) < hosts {
            format!(", fanout {fanout}")
        } else {
            String::new()
        };
        let mut line = String::new();
        line.push_str(&self.styles.command.render().to_string());
        line.push_str(&command.join(" "));
        line.push_str(&self.styles.command.render_reset().to_string());
        line.push_str(&self.styles.dim.render().to_string());
        line.push_str(&format!("  ·  {hosts} {host_word}{fanout}"));
        line.push_str(&self.styles.dim.render_reset().to_string());
        let _ = writeln!(self.stderr, "{line}");
    }

    /// Prints one Host's result, as soon as that Host settles.
    pub fn outcome(&mut self, outcome: &Outcome) {
        match self.format {
            Format::Plain => self.plain(outcome),
            Format::Json => self.json(outcome),
        }
        // Flushed per Host, so a consumer reading a pipe sees each result as
        // it settles rather than when the buffer happens to fill.
        let _ = self.stdout.flush();
    }

    /// One Host, as a line of JSON. Written by hand rather than through a
    /// `Serializer` so the object's field order matches the documented one.
    fn json(&mut self, outcome: &Outcome) {
        let line = JsonLine {
            host: &outcome.host,
            status: outcome.status.as_str(),
            exit_code: outcome.exit_code,
            cause: outcome.cause.map(Cause::as_str),
            duration_ms: outcome.duration.as_millis() as u64,
            // Lossy: a stream is remote bytes, and the alternative is failing
            // the whole run on one stray byte.
            stdout: String::from_utf8_lossy(&outcome.stdout),
            stderr: String::from_utf8_lossy(&outcome.stderr),
            truncated: outcome.stdout_truncated || outcome.stderr_truncated,
        };
        match serde_json::to_string(&line) {
            Ok(text) => {
                let _ = writeln!(self.stdout, "{text}");
            }
            // Nothing in a JsonLine can fail to serialize, so this is
            // unreachable in practice.
            Err(err) => {
                let _ = writeln!(self.stderr, "rshx: could not encode a result: {err}");
            }
        }
    }

    /// One Host, as a line plus an indented block.
    fn plain(&mut self, outcome: &Outcome) {
        let show_stdout = self.detail.stdout || outcome.status != Status::Ok;
        let show_stderr = self.detail.stderr || outcome.status != Status::Ok;

        let mut line = self.line(outcome);
        // A single-line stream folds onto the status line, which is what makes
        // `du -hs /data` readable. Anything longer goes below as an indented
        // block, so a chatty Host cannot destroy the report's shape.
        let folded = if show_stdout {
            single_line(&outcome.stdout)
        } else {
            None
        };
        if let Some(text) = &folded {
            line.push_str("  ");
            line.push_str(text);
        }
        let _ = writeln!(self.stdout, "{line}");

        if show_stdout && folded.is_none() {
            write_block(&mut self.stdout, &outcome.stdout);
            if outcome.stdout_truncated {
                let _ = writeln!(self.stdout, "  {}", TRUNCATED);
            }
        }
        if show_stderr {
            write_block(&mut self.stdout, &outcome.stderr);
            if outcome.stderr_truncated {
                let _ = writeln!(self.stdout, "  {}", TRUNCATED);
            }
        }
    }

    /// The one line that carries a Host's name, status, duration and cause.
    ///
    /// Only rshx's own tokens are styled. A Host's output is remote bytes rshx
    /// cannot interpret, so it is passed through uncoloured.
    fn line(&self, outcome: &Outcome) -> String {
        let s = &self.styles;
        let status = outcome.status.as_str();
        let style = s.status(outcome.status);

        let mut line = String::new();
        // The name is bold rather than coloured: colour is reserved for what a
        // Host's outcome *is*, the name is what it *is called*, and weight
        // survives every terminal theme where a colour may not.
        line.push_str(&format!(
            "{}{}{}",
            s.host.render(),
            outcome.host,
            s.host.render_reset()
        ));
        // Padded to the longest status, so the duration and any folded output
        // line up into columns however mixed the run's outcomes are. Spaces
        // have no colour, so the reset lands where the next column starts.
        line.push_str(&format!(
            " {}{status:<STATUS_WIDTH$}{}",
            style.render(),
            style.render_reset(),
        ));
        line.push_str(&format!(
            " {}{:.2}s{}",
            s.dim.render(),
            outcome.duration.as_secs_f64(),
            s.dim.render_reset()
        ));
        if let Some(cause) = outcome.cause {
            line.push_str(&format!(
                " {}({}){}",
                s.dim.render(),
                cause.as_str(),
                s.dim.render_reset()
            ));
        }
        line
    }

    /// The run's outcome, on stderr so that stdout stays one line per Host.
    pub fn summary(&mut self, outcomes: &[Outcome], not_started: usize, elapsed: Duration) {
        let _ = writeln!(
            self.stderr,
            "{}",
            self.summary_line(outcomes, not_started, elapsed)
        );
        // A run that was cut short must not read as though the remote work
        // stopped with it.
        if outcomes
            .iter()
            .any(|outcome| outcome.status.is_unfinished())
        {
            let style = self.styles.cancelled;
            let _ = writeln!(
                self.stderr,
                "{}{}{}",
                style.render(),
                UNFINISHED_NOTE,
                style.render_reset()
            );
        }
        let _ = self.stderr.flush();
    }

    /// The run's outcome. Only the counts carry colour, so the line still
    /// reads as plain text when it is stripped. The elapsed time is the one
    /// part a reader almost never needs, so it is the one part that is dimmed.
    fn summary_line(&self, outcomes: &[Outcome], not_started: usize, elapsed: Duration) -> String {
        let total = outcomes.len() + not_started;
        let host_word = if total == 1 { "host" } else { "hosts" };

        let mut parts = Vec::new();
        for status in Status::ALL {
            let n = outcomes.iter().filter(|o| o.status == status).count();
            // `ok` is always shown; the others only when they happened, which
            // is what keeps a clean run to a single short line.
            if status == Status::Ok || n > 0 {
                let style = self.styles.status(status);
                parts.push(format!(
                    "{n} {}{}{}",
                    style.render(),
                    status.as_str(),
                    style.render_reset()
                ));
            }
        }
        // A Host that never started is not a status: its command never ran.
        // Saying so is the only way the counts add up to the Hosts selected.
        if not_started > 0 {
            parts.push(format!("{not_started} not started"));
        }

        let mut line = format!("{total} {host_word}: {}", parts.join(", "));
        line.push(' ');
        line.push_str(&self.styles.dim.render().to_string());
        line.push_str(&format!("in {:.2}s", elapsed.as_secs_f64()));
        line.push_str(&self.styles.dim.render_reset().to_string());
        line
    }
}

/// Said once, under the summary, when a run stopped waiting for anything.
const UNFINISHED_NOTE: &str =
    "cancelled and timeout mean rshx stopped waiting; the remote commands may still be running";

/// The width the status column is padded to: the longest status, so a run with
/// mixed outcomes still lines its durations and output up. Static, so the
/// report can be written as Hosts settle rather than buffered to measure them.
const STATUS_WIDTH: usize = 11; // "unreachable"

/// Colours for the parts of the report that carry meaning.
///
/// Colour is reserved for a Host's *outcome*, the one thing a reader scans for;
/// the Host's name is bold instead, and everything secondary is dimmed. Every
/// styled token is also written as plain text, so the report never depends on
/// colour to be read.
struct Styles {
    ok: Style,
    failed: Style,
    unreachable: Style,
    timeout: Style,
    cancelled: Style,
    /// A Host's name: what it is called, as opposed to how it ended.
    host: Style,
    /// The command, in the heading.
    command: Style,
    /// The word `warning`, on a line about what rshx is doing rather than how a
    /// Host ended. Yellow like `unreachable`, because both are worth looking at
    /// without being a failure.
    warn: Style,
    /// Everything a reader only looks at when they need it: a duration, a
    /// cause, the run's elapsed time.
    dim: Style,
}

impl Styles {
    fn new() -> Styles {
        Styles {
            ok: AnsiColor::Green.on_default().bold(),
            failed: AnsiColor::Red.on_default().bold(),
            unreachable: AnsiColor::Yellow.on_default().bold(),
            timeout: AnsiColor::Magenta.on_default().bold(),
            cancelled: AnsiColor::BrightBlack.on_default(),
            host: Style::new().bold(),
            command: Style::new().bold(),
            warn: AnsiColor::Yellow.on_default().bold(),
            dim: Style::new().dimmed(),
        }
    }

    fn status(&self, status: Status) -> Style {
        match status {
            Status::Ok => self.ok,
            Status::Failed => self.failed,
            Status::Unreachable => self.unreachable,
            Status::Timeout => self.timeout,
            Status::Cancelled => self.cancelled,
        }
    }
}

/// One Host's result, as it appears under `--json`. Field order is the
/// documented one; absent fields are omitted rather than written as `null`.
#[derive(serde::Serialize)]
struct JsonLine<'a> {
    host: &'a str,
    status: &'a str,
    /// Absent when rshx killed the child: there is no exit code to report.
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    /// Absent unless rshx recognised one.
    #[serde(skip_serializing_if = "Option::is_none")]
    cause: Option<&'a str>,
    duration_ms: u64,
    stdout: std::borrow::Cow<'a, str>,
    stderr: std::borrow::Cow<'a, str>,
    /// Whether either stream lost bytes to rshx's cap.
    truncated: bool,
}

/// Written where a stream lost bytes, so a reader is never left trusting a
/// silently shortened stream.
const TRUNCATED: &str = "[output truncated]";

/// The longest stream that still folds onto the status line. A single line
/// longer than this is a block like any other: folding a megabyte of text onto
/// one line would defeat the point of folding.
const FOLD_LIMIT: usize = 200;

/// A stream that is exactly one line, with no trailing newline. `None` for an
/// empty or multi-line stream, which need different treatment.
fn single_line(stream: &[u8]) -> Option<String> {
    if stream.len() > FOLD_LIMIT {
        return None;
    }
    let text = String::from_utf8_lossy(stream);
    let text = text.strip_suffix('\n').unwrap_or(&text);
    if text.is_empty() || text.contains('\n') || text.contains('\r') {
        return None;
    }
    Some(text.to_string())
}

/// Writes a stream as an indented block beneath its Host's line.
fn write_block<W: Write>(out: &mut W, stream: &[u8]) {
    let text = String::from_utf8_lossy(stream);
    for line in text.lines() {
        let _ = writeln!(out, "  {line}");
    }
}
