//! Ctrl-C: stopping a run in a way the user can predict.
//!
//! The first interrupt stops dispatching new Hosts, then asks the ssh children
//! in flight to exit with `SIGTERM`, and gives them a grace period before
//! `SIGKILL`. A second interrupt skips the grace period. Killing the local ssh
//! does not stop the remote command, so a Host cut short is `cancelled`, never
//! `failed`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use crate::privilege::PromptClock;

/// How long a child has to exit after `SIGTERM` before rshx sends `SIGKILL`.
pub const GRACE: Duration = Duration::from_secs(2);

/// A handle on the run's interrupt state.
#[derive(Clone)]
pub struct Interrupt {
    state: Arc<State>,
}

#[derive(Default)]
struct State {
    children: Mutex<Children>,
    /// Fires when the in-flight set changes, so the grace period can end early
    /// once every child is gone.
    changed: Notify,
}

#[derive(Default)]
struct Children {
    /// Set by the first interrupt. Read and written under the same lock that
    /// holds the in-flight set, which is what makes "stop dispatching" and
    /// "kill what is in flight" one indivisible step: a Host cannot slip in
    /// between them and be missed.
    stopped: bool,
    /// In-flight ssh children, by Host, with the pid that leads their process
    /// group.
    running: HashMap<String, i32>,
    /// Hosts whose ssh rshx killed, so their result is `cancelled` whatever
    /// exit status their death produced.
    killed: HashSet<String>,
}

impl Interrupt {
    /// Starts listening for interrupts.
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

    /// Whether rshx killed this Host's ssh, which makes it `cancelled`.
    pub fn was_killed(&self, host: &str) -> bool {
        self.children().killed.contains(host)
    }

    /// Records that a Host's ssh is running, and answers whether it should be
    /// left alone. `false` means the run is already stopping, so the caller
    /// must kill it: it will never be dispatched.
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

    /// Records that a Host's ssh is gone.
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

    /// Stops the run the way Ctrl-C stops it.
    ///
    /// For a failure rshx cannot carry on past, such as a `--privilege` run
    /// with nowhere to ask for a password. The state is marked here rather than
    /// in the spawned task, so a Host settling immediately afterwards is
    /// already recorded as `cancelled`.
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

/// The interrupt itself: stop dispatching, then terminate what is in flight.
async fn stop(state: Arc<State>) {
    if tokio::signal::ctrl_c().await.is_err() {
        // No signal handling on this platform, so there is nothing to watch
        // for. The run proceeds normally.
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

/// Stops the run, and answers the process groups to terminate.
///
/// `None` when the run was already stopping: the first interrupt owns the
/// escalation, and a second one only skips the grace period.
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
        // Registered before the check, so a child that exits in between still
        // wakes this future rather than being missed.
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
/// The same escalation an interrupt uses: `SIGTERM`, a grace period, then
/// `SIGKILL`. A Host that timed out is rshx's kill, not ssh's own exit, so its
/// exit status says nothing and the caller reports no exit code.
///
/// `prompts` is what keeps a Host's own password ask out of its limit. The Host
/// is blocked on rshx for as long as its ask is out, and that is not its time:
/// without this a slow typist would turn a healthy Host into a `timeout`. Only
/// this Host's ask counts — another Host's prompt is no reason to let this one
/// run long.
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
    // Waiting that happened before this Host started is not its time, but it is
    // not a credit to it either: only the increase counts. The reader can have
    // asked and been answered in the moment between the spawn and this call.
    let before = prompts.map_or(Duration::ZERO, |prompts| prompts.spent_by(host));
    loop {
        // Never wait through this Host's own ask: it is blocked on rshx, not on
        // the network.
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
        // the sleep above cannot tell apart. Decided again here, so a Host is
        // only given up on for time that was its own.
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

/// How much of a Host's limit it has used: everything since it started, less
/// the time that Host spent waiting for a password of its own.
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
/// The group, not just the pid: ssh may have started helpers of its own (a
/// `ProxyCommand`, say), and they share the group. Each child is spawned with
/// `process_group(0)`, so its pid is also its group id.
pub fn signal_group(pid: i32, signal: i32) {
    // SAFETY: `killpg` on a pid that is already gone fails harmlessly with
    // ESRCH, which is the only failure worth expecting here.
    unsafe {
        libc::killpg(pid, signal);
    }
}
