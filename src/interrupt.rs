//! Ctrl-C: stopping a run in a way the user can predict.
//!
//! The first interrupt stops dispatching, `SIGTERM`s the ssh children in
//! flight, then `SIGKILL`s them after a grace period a second interrupt skips.
//! Killing the local ssh does not stop the remote command, so a Host cut short
//! is `cancelled`, never `failed`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use crate::privilege::PromptClock;

/// How long a child has to exit after `SIGTERM` before rshx sends `SIGKILL`.
pub const GRACE: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub struct Interrupt {
    state: Arc<State>,
}

#[derive(Default)]
struct State {
    children: Mutex<Children>,
    /// Fires when the in-flight set changes, so the grace period can end early.
    changed: Notify,
}

#[derive(Default)]
struct Children {
    /// Set by the first interrupt, under the same lock as the in-flight set:
    /// that is what makes "stop dispatching" and "kill what is in flight" one
    /// indivisible step, so no Host can slip in between and be missed.
    stopped: bool,
    /// In-flight ssh children, by Host, with the pid that leads their group.
    running: HashMap<String, i32>,
    /// Hosts rshx killed: `cancelled` regardless of exit status.
    killed: HashSet<String>,
}

impl Interrupt {
    pub fn install() -> Interrupt {
        let interrupt = Interrupt {
            state: Arc::new(State::default()),
        };
        let state = Arc::clone(&interrupt.state);
        tokio::spawn(async move { stop(state).await });
        interrupt
    }

    /// Whether the run has been interrupted. Only an optimisation: the
    /// authoritative gate is `register`, which cannot race the interrupt.
    pub fn is_stopped(&self) -> bool {
        self.children().stopped
    }

    pub fn was_killed(&self, host: &str) -> bool {
        self.children().killed.contains(host)
    }

    /// Records that a Host's ssh is running, and answers whether to leave it
    /// alone: `false` means the run is stopping, so the caller must kill it.
    pub fn register(&self, host: &str, pid: i32) -> bool {
        let mut children = self.children();
        children.running.insert(host.to_string(), pid);
        if children.stopped {
            children.killed.insert(host.to_string());
            false
        } else {
            true
        }
    }

    pub fn deregister(&self, host: &str) {
        self.children().running.remove(host);
        self.state.changed.notify_waiters();
    }

    fn children(&self) -> MutexGuard<'_, Children> {
        // Nothing runs under this lock but map operations, so it cannot be
        // poisoned; recovering rather than panicking keeps that true anyway.
        self.state
            .children
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Stops the run the way Ctrl-C stops it, for a failure rshx cannot carry
    /// on past, such as a `--privilege` run with nowhere to ask for a password.
    /// The state is marked here, not in the spawned task, so a Host settling
    /// immediately afterwards is already recorded as `cancelled`.
    pub fn trigger(&self) {
        let state = Arc::clone(&self.state);
        let pids = begin(&state);
        tokio::spawn(async move {
            if let Some(pids) = pids {
                for pid in pids {
                    signal_group(pid, libc::SIGTERM);
                }
                escalate(state).await;
            }
        });
    }
}

async fn stop(state: Arc<State>) {
    if tokio::signal::ctrl_c().await.is_err() {
        // No signal handling on this platform, so the run proceeds normally.
        return;
    }

    let Some(pids) = begin(&state) else {
        return;
    };
    for pid in pids {
        signal_group(pid, libc::SIGTERM);
    }
    escalate(state).await;
}

/// Stops the run, and answers the process groups to terminate. `None` when it
/// was already stopping: the first interrupt owns the escalation.
fn begin(state: &Arc<State>) -> Option<Vec<i32>> {
    let mut children = state
        .children
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if children.stopped {
        return None;
    }
    children.stopped = true;
    // Marked before the signal, not after: a child that dies from the signal is
    // gone by the time this function looks again.
    let running: Vec<(String, i32)> = children
        .running
        .iter()
        .map(|(host, pid)| (host.clone(), *pid))
        .collect();
    for (host, _) in &running {
        children.killed.insert(host.clone());
    }
    Some(running.into_iter().map(|(_, pid)| pid).collect())
}

/// The grace period before `SIGKILL`, cut short by a second interrupt or by
/// every child exiting on its own.
async fn escalate(state: Arc<State>) {
    tokio::select! {
        _ = tokio::time::sleep(GRACE) => {}
        _ = tokio::signal::ctrl_c() => {}
        _ = all_gone(&state) => return,
    }

    let survivors = state
        .children
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .running
        .values()
        .copied()
        .collect::<Vec<_>>();
    for pid in survivors {
        signal_group(pid, libc::SIGKILL);
    }
}

/// Resolves once no ssh child is in flight.
async fn all_gone(state: &Arc<State>) {
    loop {
        let changed = state.changed.notified();
        tokio::pin!(changed);
        // Enabled before the check, so a child exiting in between still wakes
        // this future rather than being missed.
        changed.as_mut().enable();
        let idle = state
            .children
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .running
            .is_empty();
        if idle {
            return;
        }
        changed.await;
    }
}

/// Waits for a child, terminating its process group if `limit` passes first.
/// Returns the wait's result and whether the limit fired.
///
/// Same escalation as an interrupt: `SIGTERM`, a grace period, then `SIGKILL`.
/// A Host that timed out is rshx's kill, not ssh's own exit, so its exit status
/// says nothing and the caller reports no exit code.
///
/// `prompts` keeps a Host's own password ask out of its limit: the Host is
/// blocked on rshx while the ask is out, and only its own ask is excluded.
pub async fn wait_bounded(
    child: &mut tokio::process::Child,
    pid: i32,
    limit: Option<Duration>,
    host: &str,
    prompts: Option<&PromptClock>,
) -> (std::io::Result<std::process::ExitStatus>, bool) {
    let Some(limit) = limit else {
        return (child.wait().await, false);
    };
    let started = Instant::now();
    // Waiting before this Host started is not its time, nor a credit to it:
    // only the increase counts, as an ask can be answered before this call.
    let before = prompts.map_or(Duration::ZERO, |prompts| prompts.spent_by(host));
    loop {
        // Its own ask is not its time: never wait through it.
        if let Some(prompts) = prompts {
            prompts.wait_out_prompt(host).await;
        }
        let remaining = limit.saturating_sub(own_time(started, host, prompts, before));
        if !remaining.is_zero() {
            tokio::select! {
                waited = child.wait() => return (waited, false),
                _ = tokio::time::sleep(remaining) => {}
                // A deadline has to move when an ask goes out or comes back, or
                // the sleep above would expire on time that was not the Host's.
                _ = prompt_changed(prompts) => continue,
            }
        }
        // The limit ran out — or ran out while this Host was at a prompt, which
        // the sleep cannot tell apart — so it is decided again on own time.
        if let Some(prompts) = prompts {
            prompts.wait_out_prompt(host).await;
        }
        if !limit
            .saturating_sub(own_time(started, host, prompts, before))
            .is_zero()
        {
            continue;
        }
        break;
    }

    // `pid` is 0 when the child had no pid to record. `killpg(0, …)` would
    // signal rshx's own process group, so that case kills the child directly
    // instead of by group.
    if pid != 0 {
        signal_group(pid, libc::SIGTERM);
    }
    let waited = match tokio::time::timeout(GRACE, child.wait()).await {
        Ok(waited) => waited,
        Err(_) => {
            if pid != 0 {
                signal_group(pid, libc::SIGKILL);
            } else {
                let _ = child.start_kill();
            }
            // `Child::wait` caches the status once the child is reaped, so
            // this collects what the kill produced.
            child.wait().await
        }
    };
    (waited, true)
}

/// How much of a Host's limit it has used, less its own password waits.
fn own_time(
    started: Instant,
    host: &str,
    prompts: Option<&PromptClock>,
    before: Duration,
) -> Duration {
    let waiting = prompts.map_or(Duration::ZERO, |prompts| {
        prompts.spent_by(host).saturating_sub(before)
    });
    started.elapsed().saturating_sub(waiting)
}

/// Resolves when an ask goes out or comes back, so a deadline can be moved.
async fn prompt_changed(prompts: Option<&PromptClock>) {
    match prompts {
        Some(prompts) => prompts.changed().await,
        // No `--privilege`, so no prompt can move a deadline.
        None => std::future::pending::<()>().await,
    }
}

/// Signals a child's whole process group.
///
/// The group, not just the pid: ssh helpers such as a `ProxyCommand` share it,
/// and each child is spawned with `process_group(0)`, so its pid is its group
/// id.
pub fn signal_group(pid: i32, signal: i32) {
    // SAFETY: `killpg` on a pid that is already gone fails harmlessly with
    // ESRCH, which is the only failure worth expecting here.
    unsafe {
        libc::killpg(pid, signal);
    }
}
