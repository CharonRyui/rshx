//! Running the command on Hosts, and deciding what the run's outcome means.

use std::process::Stdio;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::Result;
use futures_util::stream::{self, StreamExt};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::cause::Cause;
use crate::cli::Cli;
use crate::heartbeat::{self, Heartbeat};
use crate::host::{self, Host};
use crate::interrupt::{self, Interrupt};
use crate::report;
use crate::{EXIT_FAILED, EXIT_INTERRUPTED, EXIT_OK, EXIT_UNREACHABLE};

/// The terminal outcome of one Host's command. See CONTEXT.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Failed,
    Unreachable,
    Timeout,
    Cancelled,
}

impl Status {
    /// Every status, in the order the summary lists them.
    pub const ALL: [Status; 5] = [
        Status::Ok,
        Status::Failed,
        Status::Unreachable,
        Status::Timeout,
        Status::Cancelled,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Failed => "failed",
            Status::Unreachable => "unreachable",
            Status::Timeout => "timeout",
            Status::Cancelled => "cancelled",
        }
    }

    /// Whether rshx stopped waiting for this Host, rather than learning how
    /// its command ended. See ADR-0008.
    pub fn is_unfinished(self) -> bool {
        matches!(self, Status::Timeout | Status::Cancelled)
    }

    /// The status ssh's exit status implies, and nothing else: rshx never
    /// reads output text to decide this. See ADR-0005.
    fn from_exit_code(code: i32) -> Status {
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
    /// A best-effort explanation, inferred from ssh's stderr. It never affects
    /// `status` or the run's exit code. See ADR-0005.
    pub cause: Option<Cause>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Whether rshx dropped bytes past its cap. See ADR-0010.
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub duration: Duration,
}

/// Runs the command on every selected Host and reports as they settle.
pub async fn execute(cli: &Cli) -> Result<u8> {
    let path = host::resolve_path(cli.host_file.as_deref())?;
    let file = host::load(&path)?;
    let selected = file.select(&cli.groups)?;
    let total = selected.len();
    let started = Instant::now();

    // Chrome — the heading and the heartbeat — belongs on stderr, and only
    // when a terminal is watching it: a redirected stderr is a file that
    // neither should be written into. `--json` exists to be parsed, so it gets
    // none either.
    let chrome = !cli.json && std::io::IsTerminal::is_terminal(&std::io::stderr());

    let mut reporter = report::Reporter::new(
        cli.color,
        report::Detail {
            stdout: !cli.quiet,
            stderr: cli.stderr,
        },
        if cli.json {
            report::Format::Json
        } else {
            report::Format::Plain
        },
        chrome,
    );
    // Before the heartbeat's first draw, so the heading sits above it rather
    // than being written over by it.
    reporter.heading(&cli.command, total, cli.fanout);
    let heartbeat = Rc::new(Heartbeat::new(total, chrome));
    let interrupt = Interrupt::install();

    // The pdsh sliding window: at most `fanout` remote commands in flight, and
    // a pending Host takes the place of each one that finishes. Hosts settle
    // out of order, so each is reported the moment it does.
    let command = &cli.command;
    let mut settling = stream::iter(selected)
        .map(|host| {
            let interrupt = interrupt.clone();
            let heartbeat = Rc::clone(&heartbeat);
            async move {
                // Checked here, not when the Host entered the window, so a
                // Host waiting for a slot does not start after an interrupt.
                // Hosts that never start are not reported: their command
                // never ran, and `cancelled` would claim it did.
                if interrupt.is_stopped() {
                    return None;
                }
                heartbeat.start(&host.name);
                Some(run_host(host, command, cli.timeout, &interrupt).await)
            }
        })
        .buffer_unordered(cli.fanout as usize);

    let mut ticker = tokio::time::interval(heartbeat::TICK);
    // A missed tick means the run was busy, not that it owes several redraws.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut outcomes = Vec::with_capacity(total);
    let mut not_started = 0;
    loop {
        tokio::select! {
            next = settling.next() => match next {
                Some(Some(outcome)) => {
                    heartbeat.finish(&outcome);
                    heartbeat.suspend(|| reporter.outcome(&outcome));
                    outcomes.push(outcome);
                }
                Some(None) => not_started += 1,
                None => break,
            },
            _ = ticker.tick() => heartbeat.tick(),
        }
    }
    drop(settling);
    heartbeat.clear();

    reporter.summary(&outcomes, not_started, started.elapsed());
    Ok(exit_code(&outcomes))
}

/// Runs the command on one Host, waiting at most `limit` for it.
async fn run_host(
    host: &Host,
    command: &[String],
    limit: Option<Duration>,
    interrupt: &Interrupt,
) -> Outcome {
    let started = Instant::now();
    let mut child = Command::new("ssh");
    // Target overrides become `-o` options rather than a rewritten destination,
    // so `~/.ssh/config` stays the single source of connection configuration
    // and everything else in it still applies. See ADR-0001 and ADR-0002.
    if let Some(user) = &host.user {
        child.arg("-o").arg(format!("User={user}"));
    }
    if let Some(port) = host.port {
        child.arg("-o").arg(format!("Port={port}"));
    }
    if let Some(ip) = host.ip {
        child.arg("-o").arg(format!("HostName={ip}"));
    }
    // The command is forwarded verbatim as ssh arguments: ssh does its own
    // joining into a remote command string. `--` ends option parsing so a
    // destination is never read as an option. See ADR-0001.
    child.arg("--").arg(&host.name).args(command);
    // No terminal and no stdin: ssh must not stop to prompt, since many Hosts
    // run at once and there is nobody to answer.
    child
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Its own process group, so a terminal's interrupt reaches rshx alone and
    // rshx decides when its children die. See ADR-0008.
    child.process_group(0);

    match child.spawn() {
        Ok(mut running) => {
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
            // Both streams are drained at once, on their own tasks: reading
            // them in turn would deadlock as soon as the child filled the one
            // not being read. Tasks rather than `join!` so that terminating
            // the child below cannot cancel a half-read stream.
            let stdout_task = tokio::spawn(read_capped(stdout, STREAM_CAP));
            let stderr_task = tokio::spawn(read_capped(stderr, STREAM_CAP));

            // The child's lifetime is what the limit bounds. Once it is gone
            // its pipes close, so the reads finish too: a wedged Host cannot
            // hold the run open.
            let (waited, timed_out) = interrupt::wait_bounded(&mut running, pid, limit).await;
            if pid != 0 {
                interrupt.deregister(&host.name);
            }
            let (stdout, stdout_truncated) = stdout_task.await.unwrap_or_default();
            let (stderr, stderr_truncated) = stderr_task.await.unwrap_or_default();

            // A Host rshx killed is `cancelled` whatever exit status its death
            // produced: ssh exits 255 on SIGTERM, which would otherwise read
            // as `unreachable`. See ADR-0008. An interrupt outranks a timeout:
            // the run is ending, and `cancelled` is what happened to it.
            let killed = interrupt.was_killed(&host.name);
            let exit_code = waited.as_ref().ok().and_then(|status| status.code());
            let status = match (killed, timed_out, &waited, exit_code) {
                (true, _, _, _) => Status::Cancelled,
                (false, true, _, _) => Status::Timeout,
                (false, false, Ok(_), Some(code)) => Status::from_exit_code(code),
                // No exit status: the child died from a signal.
                (false, false, Ok(_), None) => Status::Cancelled,
                // The child is gone but its status is unreadable, so the Host
                // has no status to report.
                (false, false, Err(_), _) => Status::Unreachable,
            };
            // Computed after the status, and never allowed to change it.
            let cause = match status {
                Status::Failed | Status::Unreachable => Cause::infer(&stderr),
                _ => None,
            };
            let stderr = match waited {
                Ok(_) => stderr,
                Err(err) => format!("rshx: could not read ssh's exit status: {err}\n").into_bytes(),
            };
            Outcome {
                host: host.name.clone(),
                status,
                // Absent when rshx ended the child: its exit status says
                // nothing about the command, and per ADR-0008 the remote
                // command may still be running.
                exit_code: if killed || timed_out { None } else { exit_code },
                cause,
                stdout,
                stderr,
                stdout_truncated,
                stderr_truncated,
                duration: started.elapsed(),
            }
        }
        Err(err) => Outcome {
            host: host.name.clone(),
            status: Status::Unreachable,
            exit_code: None,
            // rshx's own failure to spawn ssh, not a diagnosis of the remote.
            cause: None,
            stdout: Vec::new(),
            stderr: format!("rshx: could not run ssh: {err}\n").into_bytes(),
            stdout_truncated: false,
            stderr_truncated: false,
            duration: started.elapsed(),
        },
    }
}

/// The most of each stream that is kept in memory. Past this the bytes are
/// dropped, and the Outcome records that they were. See ADR-0010.
const STREAM_CAP: usize = 1024 * 1024;

/// Reads a stream, keeping at most `cap` bytes.
///
/// The rest is still drained and thrown away: a Host that prints more than the
/// cap must not grow rshx without bound, and must not block on a full pipe
/// either, since ssh would then never exit.
async fn read_capped<R>(mut stream: R, cap: usize) -> (Vec<u8>, bool)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut kept = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    let mut truncated = false;
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                let room = cap.saturating_sub(kept.len());
                let keep = room.min(n);
                kept.extend_from_slice(&buf[..keep]);
                truncated |= keep < n;
            }
            // The stream ended as far as the report is concerned. ssh's exit
            // status is what decides the Host's status, never a read error.
            Err(_) => break,
        }
    }
    (kept, truncated)
}

/// The run's exit code, per ADR-0004: `failed` and `unreachable` are separate
/// bits, and a `timeout` counts as `unreachable`. A `cancelled` Host is not a
/// failure — rshx stopped waiting, the command did not fail — so a run that
/// was interrupted exits 99 and nothing else. See ADR-0008.
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
            Status::Ok | Status::Cancelled => {}
        }
    }
    code
}
