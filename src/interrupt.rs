//! Ctrl-C: stopping a run in a way the user can predict.
//!
//! The first interrupt stops dispatching new Hosts, then asks the ssh children
//! in flight to exit with `SIGTERM`, and gives them a grace period before
//! `SIGKILL`. A second interrupt skips the grace period. Killing the local ssh
//! does not stop the remote command, so a Host cut short is `cancelled`, never
//! `failed`. See ADR-0008.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::Notify;

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
}

/// The interrupt itself: stop dispatching, then terminate what is in flight.
async fn stop(state: Arc<State>) {
    if tokio::signal::ctrl_c().await.is_err() {
        // No signal handling on this platform, so there is nothing to watch
        // for. The run proceeds normally.
        return;
    }

    let pids = {
        let mut children = state
            .children
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        children.stopped = true;
        // Marked before the signal, not after: a child that dies from the
        // signal is gone by the time this function looks again.
        let running: Vec<(String, i32)> = children
            .running
            .iter()
            .map(|(host, pid)| (host.clone(), *pid))
            .collect();
        for (host, _) in &running {
            children.killed.insert(host.clone());
        }
        running.into_iter().map(|(_, pid)| pid).collect::<Vec<_>>()
    };
    for pid in pids {
        signal_group(pid, libc::SIGTERM);
    }

    // The grace period, cut short by a second interrupt or by every child
    // exiting on its own.
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
pub async fn wait_bounded(
    child: &mut tokio::process::Child,
    pid: i32,
    limit: Option<Duration>,
) -> (std::io::Result<std::process::ExitStatus>, bool) {
    let Some(limit) = limit else {
        return (child.wait().await, false);
    };
    tokio::select! {
        waited = child.wait() => (waited, false),
        _ = tokio::time::sleep(limit) => {
            // `pid` is 0 when the child had no pid to record. `killpg(0, …)`
            // would signal rshx's own process group, so that case kills the
            // child directly instead of by group.
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
                    // `Child::wait` caches the status once the child is
                    // reaped, so this collects what the kill produced.
                    child.wait().await
                }
            };
            (waited, true)
        }
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
