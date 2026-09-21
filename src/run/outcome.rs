//! What a Host's command did: the vocabulary a run reports in.
use std::time::Duration;

use crate::cause::Cause;
use crate::{EXIT_FAILED, EXIT_INTERRUPTED, EXIT_OK, EXIT_UNREACHABLE};

/// The terminal outcome of one Host's command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Failed,
    /// The command was started and left running on the Host. Only a detached
    /// run reports it: every other run waits for its command, and a Host rshx
    /// cut short is `cancelled`.
    Running,
    Unreachable,
    Timeout,
    Cancelled,
}

impl Status {
    /// Every status, in the order the summary lists them.
    pub const ALL: [Status; 6] = [
        Status::Ok,
        Status::Failed,
        Status::Running,
        Status::Unreachable,
        Status::Timeout,
        Status::Cancelled,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Failed => "failed",
            Status::Running => "running",
            Status::Unreachable => "unreachable",
            Status::Timeout => "timeout",
            Status::Cancelled => "cancelled",
        }
    }

    /// Whether rshx stopped waiting for this Host, not how its command ended.
    /// A `running` Host is not one rshx gave up on: rshx left it to run.
    pub fn is_unfinished(self) -> bool {
        matches!(self, Status::Timeout | Status::Cancelled)
    }

    /// The status ssh's exit status implies: rshx never parses output text.
    pub(super) fn from_exit_code(code: i32) -> Status {
        match code {
            0 => Status::Ok,
            // ssh's own "I could not connect" status.
            255 => Status::Unreachable,
            _ => Status::Failed,
        }
    }
}

/// What one Host's command did.
#[derive(Debug)]
pub struct Outcome {
    pub host: String,
    pub status: Status,
    /// Absent when rshx killed the child, since then there is no exit status.
    pub exit_code: Option<i32>,
    /// Inferred from ssh's stderr; it never affects `status` or the exit code.
    pub cause: Option<Cause>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Whether rshx dropped bytes past its cap.
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    /// Whether rshx stopped the Host's remote command. `false` on a Host rshx
    /// cut short means the command may still be running there: the report says
    /// so rather than claiming work that was not done.
    pub remote_stopped: bool,
    pub duration: Duration,
    /// The pid of the shell running the Host's command, when rshx knows it:
    /// a detached launch is told it, and nothing else is.
    pub pid: Option<u32>,
}

impl Outcome {
    /// A Host rshx itself could not get as far as running anything on, with no
    /// exit status to report: its own ssh never said anything about a command.
    /// The message is rshx's, so it is prefixed as such.
    pub fn local_failure(host: &str, duration: Duration, message: String) -> Outcome {
        Outcome {
            host: host.to_string(),
            status: Status::Unreachable,
            exit_code: None,
            cause: None,
            stdout: Vec::new(),
            stderr: format!("rshx: {message}\n").into_bytes(),
            stdout_truncated: false,
            stderr_truncated: false,
            // Nothing ran remotely, so there is nothing to have stopped.
            remote_stopped: false,
            duration,
            pid: None,
        }
    }
}

/// The run's exit code: `failed` and `unreachable` are separate bits, and a
/// `timeout` counts as `unreachable`. A `cancelled` Host is not a failure —
/// rshx stopped waiting — so an interrupted run exits 99 and nothing else.
pub fn exit_code(outcomes: &[Outcome]) -> u8 {
    if outcomes
        .iter()
        .any(|outcome| outcome.status == Status::Cancelled)
    {
        return EXIT_INTERRUPTED;
    }
    let mut code = EXIT_OK;
    for outcome in outcomes {
        match outcome.status {
            Status::Failed => code |= EXIT_FAILED,
            Status::Unreachable | Status::Timeout => code |= EXIT_UNREACHABLE,
            // A detached run's Hosts are `running` when rshx hands them over,
            // which is exactly what was asked for.
            Status::Ok | Status::Running | Status::Cancelled => {}
        }
    }
    code
}
