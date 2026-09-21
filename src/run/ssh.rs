//! One Host's ssh: spawned, bounded, read, and turned into an Outcome.
use std::time::{Duration, Instant};

use tokio::process::Command;

use crate::cause::Cause;
use crate::host::Host;
use crate::interrupt::{self, Interrupt};
use crate::remote;
use crate::run::outcome::{Outcome, Status};
use crate::run::prompt::{self, Ask, Prompts};
use crate::run::stream;

/// What one Host's ssh left behind: everything rshx read from it, before
/// anything is made of it.
struct Settled {
    /// The child's own end. `Err` means it is gone but its status is
    /// unreadable.
    waited: std::io::Result<std::process::ExitStatus>,
    /// Whether rshx's own limit ended the child.
    timed_out: bool,
    /// Whether an interrupt killed it.
    killed: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_truncated: bool,
    stderr_truncated: bool,
    /// How long the Host took, as rshx measured it.
    duration: Duration,
}

impl Settled {
    /// The child's exit status, when it exited rather than being killed.
    fn exit_code(&self) -> Option<i32> {
        self.waited.as_ref().ok().and_then(|status| status.code())
    }
}

/// Runs one Host's ssh to its end — or until an interrupt or that Host's own
/// limit ends it — and reads both of its streams. `Err` is rshx's own failure
/// to start ssh, not a diagnosis of the remote.
///
/// Shared by every connection rshx makes: a run waits for its command, and the
/// stop a Host cut short is asked for waits for the Host's answer. What the
/// streams *mean* is the caller's, which is the whole difference between them.
async fn settle(
    mut child: Command,
    host: &Host,
    interrupt: &Interrupt,
    limit: Option<Duration>,
    prompts: Option<Prompts>,
) -> std::io::Result<Settled> {
    // Its own process group, so a terminal's interrupt reaches rshx alone and
    // rshx decides when its children die.
    child.process_group(0);

    let started = Instant::now();
    let mut running = child.spawn()?;
    let pid = match running.id() {
        Some(pid) => pid as i32,
        // A spawned child always has a pid; without one it cannot be
        // tracked, so it is left to run rather than killed blind.
        None => 0,
    };
    if pid != 0 && !interrupt.register(&host.name, pid) {
        // The run was interrupted while this ssh was starting. It is
        // already marked killed, so its result is `cancelled`.
        interrupt::signal_group(pid, libc::SIGTERM);
    }

    let stdout = running.stdout.take().expect("stdout was piped");
    let stderr = running.stderr.take().expect("stderr was piped");
    // The Host's stdin reaches the remote sudo, so the reader that
    // notices the prompt is the one that can answer it.
    let ask = match (prompts.as_ref(), running.stdin.take()) {
        (Some(prompts), Some(stdin)) => Some(Ask::new(prompts.clone(), host, stdin)),
        _ => None,
    };
    // Both streams are drained at once: read in turn, they deadlock
    // once the child fills one. Tasks, not `join!`, so terminating
    // the child cannot cancel a half-read stream.
    let stdout_task = tokio::spawn(stream::read_capped(stdout, stream::CAP));
    let stderr_task = tokio::spawn(prompt::read_stderr(stderr, stream::CAP, ask));

    // The limit bounds the child's lifetime: once it is gone its
    // pipes close, so the reads finish and a wedged Host cannot hold
    // the run open. Only this Host's own ask is excluded from the
    // limit, so a slow typist does not turn it into a `timeout`.
    let (waited, timed_out) = interrupt::wait_bounded(
        &mut running,
        pid,
        limit,
        &host.name,
        prompts.as_ref().map(Prompts::clock),
    )
    .await;
    if pid != 0 {
        interrupt.deregister(&host.name);
    }
    let (stdout, stdout_truncated) = stdout_task.await.unwrap_or_default();
    let (stderr, stderr_truncated) = stderr_task.await.unwrap_or_default();

    Ok(Settled {
        waited,
        timed_out,
        // A Host rshx killed is `cancelled` whatever exit status its death
        // produced: ssh exits 255 on SIGTERM, which would otherwise read as
        // `unreachable`. An interrupt outranks a timeout.
        killed: interrupt.was_killed(&host.name),
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
        duration: started.elapsed(),
    })
}

/// Runs one Host's command over ssh, and makes an Outcome of it.
///
/// `marker` is the file the run left on the Host naming its command: a Host
/// rshx cut short is asked to stop its own command by that marker, because
/// killing its ssh does not stop what it started.
pub(super) async fn run_remote_command(
    child: Command,
    host: &Host,
    interrupt: &Interrupt,
    limit: Option<Duration>,
    prompts: Option<Prompts>,
    marker: Option<&str>,
) -> Outcome {
    let started = Instant::now();
    let settled = match settle(child, host, interrupt, limit, prompts.clone()).await {
        Ok(settled) => settled,
        // rshx's own failure to spawn ssh, not a diagnosis of the remote.
        Err(err) => {
            return Outcome::local_failure(
                &host.name,
                started.elapsed(),
                format!("could not run ssh: {err}"),
            );
        }
    };
    let exit_code = settled.exit_code();
    let Settled {
        waited,
        timed_out,
        killed,
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
        duration,
    } = settled;

    let status = match (killed, timed_out, &waited, exit_code) {
        (true, _, _, _) => Status::Cancelled,
        (false, true, _, _) => Status::Timeout,
        (false, false, Ok(_), Some(code)) => Status::from_exit_code(code),
        // No exit status: the child died from a signal.
        (false, false, Ok(_), None) => Status::Cancelled,
        // The child is gone but its status is unreadable.
        (false, false, Err(_), _) => Status::Unreachable,
    };
    // Computed after the status, and never allowed to change it.
    let cause = match status {
        Status::Failed | Status::Unreachable => Cause::infer(&stderr),
        _ => None,
    };
    let mut stderr = match waited {
        Ok(_) => stderr,
        Err(err) => format!("rshx: could not read ssh's exit status: {err}\n").into_bytes(),
    };
    // Killing a Host's ssh does not stop what it started, so a Host rshx cut
    // short is asked to stop its own command, by the marker the run left on it.
    let mut remote_stopped = false;
    if (killed || timed_out)
        && let Some(marker) = marker
    {
        let password = prompts
            .as_ref()
            .and_then(|prompts| prompts.password_for(host));
        match remote::stop(host, marker, prompts.is_some(), password.as_deref()).await {
            Ok(()) => remote_stopped = true,
            // The Host's status is settled; this says what rshx could not
            // finish, which its own output cannot.
            Err(reason) => stderr.extend_from_slice(
                format!("rshx: could not stop the remote command: {reason}\n").as_bytes(),
            ),
        }
    }
    Outcome {
        host: host.name.clone(),
        status,
        // Absent when rshx ended the child: its exit status says nothing about
        // the command, which rshx stops on its own.
        exit_code: if killed || timed_out { None } else { exit_code },
        cause,
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
        remote_stopped,
        duration,
        // rshx waited for this command rather than leaving it to run, so no
        // Host ever told it a pid.
        pid: None,
    }
}
