//! The line that says a long run is still alive.
//!
//! It is stderr chrome, never part of the report: a run's results are complete
//! without it. It is drawn only when stderr is an interactive terminal, so a
//! redirected file gets no heartbeat and no escape sequences, and it is off
//! under `--json`. See ADR-0011.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

use crate::run::Outcome;

/// How often the heartbeat is redrawn while Hosts are in flight. Only the
/// elapsed time changes between Hosts settling, so this is a readability
/// choice, not a scheduling one.
pub const TICK: Duration = Duration::from_millis(200);

/// A one-line summary of a run in progress.
pub struct Heartbeat {
    bar: ProgressBar,
    total: usize,
    state: RefCell<State>,
}

#[derive(Default)]
struct State {
    /// When each in-flight Host started, so the longest-running one can be
    /// named.
    running: BTreeMap<String, Instant>,
    done: usize,
}

impl Heartbeat {
    /// `json` hides the heartbeat: that mode exists to be parsed, not watched.
    pub fn new(total: usize, json: bool) -> Heartbeat {
        let target = if json {
            ProgressDrawTarget::hidden()
        } else {
            // Hides itself when stderr is not a terminal, which is what keeps
            // a redirected file free of escape sequences.
            ProgressDrawTarget::stderr()
        };
        let bar = ProgressBar::with_draw_target(Some(total as u64), target);
        // A bare `{msg}`: without a style of its own the bar would draw
        // indicatif's default progress bar, with a `{wide_bar}` and a count,
        // over a line that already says both. The template is a constant that
        // always parses, so the error arm is unreachable.
        if let Ok(style) = ProgressStyle::with_template("{msg}") {
            bar.set_style(style);
        }

        let heartbeat = Heartbeat {
            bar,
            total,
            state: RefCell::new(State::default()),
        };
        heartbeat.redraw();
        heartbeat
    }

    /// Records that a Host's remote command is starting.
    pub fn start(&self, host: &str) {
        let mut state = self.state.borrow_mut();
        state.running.insert(host.to_string(), Instant::now());
    }

    /// Records that a Host settled.
    pub fn finish(&self, outcome: &Outcome) {
        let mut state = self.state.borrow_mut();
        state.running.remove(&outcome.host);
        state.done += 1;
        let done = state.done;
        drop(state);
        self.bar.set_position(done as u64);
        self.redraw();
    }

    /// Redraws between Hosts settling, so a slow Host's elapsed time still
    /// moves rather than looking like a hung run.
    pub fn tick(&self) {
        self.redraw();
    }

    /// Runs `body` with the heartbeat out of the way, then redraws it.
    ///
    /// Result lines are the report, and a progress line that redrew itself
    /// over one would corrupt it. `suspend` clears the line first, which is
    /// what makes this hold in a redirected file too: unlike
    /// `ProgressBar::println`, it still writes when the bar is hidden.
    pub fn suspend<R>(&self, body: impl FnOnce() -> R) -> R {
        self.bar.suspend(body)
    }

    /// Removes the line, leaving the terminal to the summary.
    pub fn clear(&self) {
        self.bar.finish_and_clear();
    }

    /// The line's text, as it would be drawn.
    fn message(&self) -> String {
        let state = self.state.borrow();
        let done = state.done;
        let total = self.total;
        match state.longest_running() {
            None => format!("{done}/{total} done"),
            Some((host, elapsed)) => format!(
                "{done}/{total} done, {} running, {host} {:.1}s",
                state.running.len(),
                elapsed.as_secs_f64()
            ),
        }
    }

    fn redraw(&self) {
        self.bar.set_message(self.message());
    }
}

impl State {
    /// The Host that has been running longest, with its elapsed time.
    fn longest_running(&self) -> Option<(&str, Duration)> {
        self.running
            .iter()
            .map(|(host, started)| (host.as_str(), started.elapsed()))
            .max_by_key(|(_, elapsed)| *elapsed)
    }
}
