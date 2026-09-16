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
    /// `chrome` is false when stderr is not a terminal, or under `--json`:
    /// there is nobody watching, and a redirected file must stay free of
    /// escape sequences.
    pub fn new(total: usize, chrome: bool) -> Heartbeat {
        let target = if chrome {
            ProgressDrawTarget::stderr()
        } else {
            ProgressDrawTarget::hidden()
        };
        let bar = ProgressBar::with_draw_target(Some(total as u64), target);
        // A spinner for liveness, then how far the run has got, then a bar for
        // the proportion, then the one detail a count cannot give: what is
        // still in flight. The bar takes the width left over, so the line
        // always fills the terminal and the parts either side of it stay put.
        if let Ok(style) =
            ProgressStyle::with_template("{spinner:.cyan} {pos}/{len} done {wide_bar:.green} {msg}")
        {
            bar.set_style(
                style
                    // The frames of the spinner. Its last character is what
                    // the finished bar shows, and it is never seen: the
                    // heartbeat is cleared before the summary.
                    .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ")
                    // Filled, then the partial cell, then the empty cell, so
                    // the bar reads as one continuous track rather than a
                    // block of colour next to a gap.
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

    /// Records that a Host settled.
    ///
    /// The position is set before the message because each setter takes the
    /// bar's lock, draws, and releases it. The message never mentions the
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
    /// moves rather than looking like a hung run. `tick` also advances the
    /// spinner frame, which is the only part of the line that moves while
    /// nothing settles — without it a long Host would look like a hung run.
    pub fn tick(&self) {
        self.redraw();
        // Advances the spinner, not the position: the bar tracks Hosts that
        // have settled, and nothing else may move it.
        self.bar.tick();
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
    ///
    /// How far the run has got is the template's `{pos}/{len}`, so the message
    /// carries only what a count cannot: how much is in flight, and what is
    /// taking longest. Naming the slowest Host is what tells a reader that a
    /// run is waiting on one machine rather than on the network.
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
