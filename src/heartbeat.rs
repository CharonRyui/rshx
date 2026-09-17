//! The line that says a long run is still alive.
//!
//! Stderr chrome, never part of the report, and off under `--json`. Drawn only
//! when stderr is a terminal, so a redirected file gets no escape sequences.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

use crate::run::Outcome;

/// How often the heartbeat is redrawn while Hosts are in flight: only the
/// elapsed time changes between Hosts settling, so it is a readability choice.
pub const TICK: Duration = Duration::from_millis(200);

/// A one-line summary of a run in progress.
pub struct Heartbeat {
    bar: ProgressBar,
    state: RefCell<State>,
}

#[derive(Default)]
struct State {
    /// When each in-flight Host started, so the longest-running can be named.
    running: BTreeMap<String, Instant>,
    done: usize,
}

impl Heartbeat {
    /// `chrome` is false when stderr is not a terminal, or under `--json`.
    pub fn new(total: usize, chrome: bool) -> Heartbeat {
        let target = if chrome {
            ProgressDrawTarget::stderr()
        } else {
            ProgressDrawTarget::hidden()
        };
        let bar = ProgressBar::with_draw_target(Some(total as u64), target);
        // Spinner, progress count, bar, then what is in flight. The bar takes
        // the leftover width, so the line fills the terminal and the parts
        // beside it stay put.
        if let Ok(style) =
            ProgressStyle::with_template("{spinner:.cyan} {pos}/{len} done {wide_bar:.green} {msg}")
        {
            bar.set_style(
                style
                    // The last character is what the finished bar shows, and it
                    // is never seen: the heartbeat is cleared first.
                    .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ")
                    // Filled, then partial, then empty: the bar reads as one
                    // track rather than a block beside a gap.
                    .progress_chars("━╸─"),
            );
        }

        let heartbeat = Heartbeat {
            bar,
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

    /// Records that a Host settled. The position is set before the message:
    /// each setter draws on its own lock, and the message never mentions the
    /// count, so the frame between the two is stale, never wrong.
    pub fn finish(&self, outcome: &Outcome) {
        let done = {
            let mut state = self.state.borrow_mut();
            state.running.remove(&outcome.host);
            state.done += 1;
            state.done as u64
        };
        self.bar.set_position(done);
        self.redraw();
    }

    /// Redraws between Hosts settling, so a slow Host's elapsed time still
    /// moves.
    pub fn tick(&self) {
        self.redraw();
        // Advances the spinner, not the position: the bar tracks Hosts that
        // have settled, and nothing else may move it.
        self.bar.tick();
    }

    /// Runs `body` with the heartbeat out of the way, then redraws it: a redraw
    /// over a result line corrupts the report. Unlike `ProgressBar::println`
    /// this clears the line, and still writes when the bar is hidden.
    pub fn suspend<R>(&self, body: impl FnOnce() -> R) -> R {
        self.bar.suspend(body)
    }

    /// Removes the line, leaving the terminal to the summary.
    pub fn clear(&self) {
        self.bar.finish_and_clear();
    }

    /// Takes the progress line off the terminal, for something that needs the
    /// line to itself: a password prompt is written where the bar is drawn, and
    /// a redraw under the cursor would corrupt it. The bar is finished, not
    /// hidden: a hidden target stops drawing but leaves the last line standing.
    pub fn pause(&self) {
        self.bar.finish_and_clear();
    }

    /// Puts the line back, at the position it left off. `reset` returns the
    /// bar to `InProgress` and zeroes the position, so it must be followed by
    /// a restore.
    pub fn resume(&self) {
        self.bar.reset();
        self.bar.set_position(self.state.borrow().done as u64);
        self.redraw();
    }

    /// The line's text, as it would be drawn: how far the run has got is the
    /// template's `{pos}/{len}`, so this carries only what a count cannot —
    /// what is in flight, and the slowest Host, which tells a reader the run is
    /// waiting on one machine rather than on the network.
    fn message(&self) -> String {
        let state = self.state.borrow();
        match state.longest_running() {
            None => String::new(),
            Some((host, elapsed)) => format!(
                "{} running, {host} {:.1}s",
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
