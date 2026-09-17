//! Running the command on Hosts, and deciding what the run's outcome means.

use std::process::Stdio;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use futures_util::stream::{self, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::cause::Cause;
use crate::cli::Cli;
use crate::heartbeat::{self, Heartbeat};
use crate::host::{self, Host};
use crate::interrupt::{self, Interrupt};
use crate::privilege::{self, Privilege, PromptClock};
use crate::report;
use crate::{EXIT_FAILED, EXIT_INTERRUPTED, EXIT_LOCAL, EXIT_OK, EXIT_UNREACHABLE};

/// The terminal outcome of one Host's command.
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

    /// Whether rshx stopped waiting for this Host, not how its command ended.
    pub fn is_unfinished(self) -> bool {
        matches!(self, Status::Timeout | Status::Cancelled)
    }

    /// The status ssh's exit status implies: rshx never parses output text.
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
    /// Inferred from ssh's stderr; it never affects `status` or the exit code.
    pub cause: Option<Cause>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Whether rshx dropped bytes past its cap.
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

    // `--privilege` wraps the command once, before anything runs, so every Host
    // gets that same command: a remote sudo reads its password from stdin.
    let command = if cli.privilege {
        privilege::under_sudo(&cli.command)
    } else {
        cli.command.clone()
    };
    // A command that runs sudo itself gets rshx's sudo in front of it and
    // elevates a second time: rshx reads none of the command's own options —
    // that would mean knowing sudo's grammar — so it warns rather than guesses.
    let nested_sudo = cli.privilege && privilege::runs_sudo(&cli.command);
    // Borrowed from here on: every Host's task reads the same command.
    let command = &command;
    let prompts = if cli.privilege {
        let privilege = Arc::new(Privilege::default());
        let (requests, asks) = mpsc::unbounded_channel();
        (
            Some(Prompts {
                clock: privilege.clock(),
                privilege,
                requests,
            }),
            Some(asks),
        )
    } else {
        (None, None)
    };
    let (prompts, mut asks) = prompts;

    // Chrome goes on stderr, and only when a terminal is watching it: a
    // redirected stderr must stay free of it, and `--json` must be parseable.
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
    // Before the heading, so what rshx says about the command is read first.
    if nested_sudo {
        reporter
            .warning("the command runs sudo itself; --privilege puts its own sudo in front of it");
    }
    // Before the heartbeat's first draw, so the heading is not written over.
    reporter.heading(command, total, cli.fanout);
    let heartbeat = Rc::new(Heartbeat::new(total, chrome));
    let interrupt = Interrupt::install();

    // The pdsh sliding window: at most `fanout` remote commands in flight, each
    // one that finishes replaced by a pending Host.
    let mut settling = stream::iter(selected)
        .map(|host| {
            let interrupt = interrupt.clone();
            let heartbeat = Rc::clone(&heartbeat);
            let prompts = prompts.clone();
            async move {
                // Checked here, not on entry, so a Host waiting for a slot
                // does not start after an interrupt — and is not reported.
                if interrupt.is_stopped() {
                    return None;
                }
                heartbeat.start(&host.name);
                Some(run_host(host, command, cli.timeout, prompts, &interrupt).await)
            }
        })
        .buffer_unordered(cli.fanout as usize);

    let mut ticker = tokio::time::interval(heartbeat::TICK);
    // A missed tick means the run was busy, not that it owes several redraws.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut outcomes = Vec::with_capacity(total);
    let mut not_started = 0;
    // Why the run could not carry on: nowhere to ask, or nothing typed.
    let mut fatal = None;
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
            // A Host is asking for a password. Answered in this task, which
            // owns the terminal and the heartbeat: one prompt for the run.
            request = next_request(&mut asks) => match request {
                Some(request) => {
                    // A stopping run has nobody to type for it, and dropping
                    // the receiver releases the Hosts still waiting.
                    if interrupt.is_stopped() {
                        asks = None;
                        continue;
                    }
                    let privilege = &prompts.as_ref().expect("--privilege built one").privilege;
                    // Off the terminal while the user types: the prompt is
                    // written where the progress line is drawn.
                    heartbeat.pause();
                    let answer = privilege.answer(&request.host, request.unique).await;
                    heartbeat.resume();
                    let password = match answer {
                        privilege::Answer::Password(password) => Some(password),
                        // Nothing to write: the Host's sudo fails on its own,
                        // and the run stops rather than repeating the failure.
                        privilege::Answer::Refused => {
                            fatal = privilege.fatal();
                            interrupt.trigger();
                            asks = None;
                            None
                        }
                        privilege::Answer::Interrupted => {
                            interrupt.trigger();
                            asks = None;
                            None
                        }
                    };
                    let _ = request.reply.send(password);
                }
                // Every Host that could ask is gone.
                None => asks = None,
            },
            _ = ticker.tick() => heartbeat.tick(),
        }
    }
    drop(settling);
    heartbeat.clear();

    if let Some(reason) = &fatal {
        eprintln!("rshx: {reason}");
    }
    reporter.summary(&outcomes, not_started, started.elapsed());
    Ok(match fatal {
        // A run rshx could not ask for a password is a local failure, not a
        // Host's: the Hosts it cancelled never got to fail.
        Some(_) => EXIT_LOCAL,
        None => exit_code(&outcomes),
    })
}

/// The passwords a run asks for, and where a Host's stderr reader asks. Held
/// by every Host's task, so the clock and the ask channel reach whoever asks.
#[derive(Clone)]
struct Prompts {
    /// What a Host spends waiting for its password, outside its `--timeout`.
    clock: PromptClock,
    privilege: Arc<Privilege>,
    requests: mpsc::UnboundedSender<privilege::Request>,
}

/// The next Host asking for a password.
async fn next_request(
    asks: &mut Option<mpsc::UnboundedReceiver<privilege::Request>>,
) -> Option<privilege::Request> {
    match asks {
        Some(asks) => asks.recv().await,
        // No `--privilege`, so no Host can ask.
        None => std::future::pending().await,
    }
}

/// One Host's stderr reader, and its means of answering a password prompt.
struct Ask {
    prompts: Prompts,
    host: String,
    /// Whether this Host wants a password of its own rather than the run's.
    unique: bool,
    /// The Host's ssh stdin, which reaches the remote sudo.
    stdin: tokio::process::ChildStdin,
}

impl Ask {
    /// Asks the run for this Host's password, and writes it to the Host.
    /// Answers whether the run can still answer; asking again never returns.
    async fn answer(&mut self) -> bool {
        let (reply, answer) = tokio::sync::oneshot::channel();
        let request = privilege::Request {
            host: self.host.clone(),
            unique: self.unique,
            reply,
        };
        // Timed from here, not from when the user starts typing: the Host is
        // blocked on rshx until its answer comes back. Marked before the ask
        // goes out, so one queued behind another Host's prompt still counts.
        let waiting = self.prompts.clock.asking_now(&self.host);
        if self.prompts.requests.send(request).is_err() {
            return false;
        }
        let Ok(Some(password)) = answer.await else {
            return false;
        };
        // The writing is the Host's own time again.
        drop(waiting);
        // The password is written to the Host's ssh, which forwards it to the
        // remote sudo reading its stdin. A newline ends it: sudo reads a line.
        self.stdin.write_all(&password).await.is_ok()
            && self.stdin.write_all(b"\n").await.is_ok()
            && self.stdin.flush().await.is_ok()
    }
}

/// Reads a Host's stderr, answering password prompts as they appear. The
/// markers are stripped on the way into the report: they are rshx's own
/// plumbing, not something the remote command printed.
async fn read_stderr<R>(stream: R, cap: usize, ask: Option<Ask>) -> (Vec<u8>, bool)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let Some(mut ask) = ask else {
        return read_capped(stream, cap).await;
    };
    let mut stream = stream;
    let mut kept = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    let mut filter = privilege::MarkerFilter::default();
    let mut truncated = false;
    let mut answerable = true;
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                let filtered = filter.push(&buf[..n], &mut kept, cap);
                truncated |= filtered.dropped;
                if filtered.asked && answerable {
                    answerable = ask.answer().await;
                }
            }
            // The stream ended as far as the report is concerned. ssh's exit
            // status is what decides the Host's status, never a read error.
            Err(_) => break,
        }
    }
    truncated |= filter.finish(&mut kept, cap);
    (kept, truncated)
}

/// Runs the command on one Host, waiting at most `limit` for it.
async fn run_host(
    host: &Host,
    command: &[String],
    limit: Option<Duration>,
    prompts: Option<Prompts>,
    interrupt: &Interrupt,
) -> Outcome {
    let started = Instant::now();
    let mut child = Command::new("ssh");
    // Overrides become `-o` options rather than a rewritten destination, so
    // `~/.ssh/config` stays the single source and the rest still applies.
    if let Some(user) = &host.user {
        child.arg("-o").arg(format!("User={user}"));
    }
    if let Some(port) = host.port {
        child.arg("-o").arg(format!("Port={port}"));
    }
    if let Some(ip) = host.ip {
        child.arg("-o").arg(format!("HostName={ip}"));
    }
    // Forwarded verbatim as ssh arguments, since ssh does its own joining.
    // `--` ends option parsing, so a destination is never read as an option.
    child.arg("--").arg(&host.name).args(command);
    // Without `--privilege`, stdin is null so ssh cannot stop to prompt with
    // nobody there to answer; with it, stdin carries the password to sudo.
    child.stdin(if prompts.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    child.stdout(Stdio::piped()).stderr(Stdio::piped());
    // Its own process group, so a terminal's interrupt reaches rshx alone and
    // rshx decides when its children die.
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
            // The Host's stdin reaches the remote sudo, so the reader that
            // notices the prompt is the one that can answer it.
            let ask = match (prompts.as_ref(), running.stdin.take()) {
                (Some(prompts), Some(stdin)) => Some(Ask {
                    prompts: prompts.clone(),
                    host: host.name.clone(),
                    unique: host.unique_privilege_pass,
                    stdin,
                }),
                _ => None,
            };
            // Both streams are drained at once: read in turn, they deadlock
            // once the child fills one. Tasks, not `join!`, so terminating
            // the child cannot cancel a half-read stream.
            let stdout_task = tokio::spawn(read_capped(stdout, STREAM_CAP));
            let stderr_task = tokio::spawn(read_stderr(stderr, STREAM_CAP, ask));

            // The limit bounds the child's lifetime: once it is gone its
            // pipes close, so the reads finish and a wedged Host cannot hold
            // the run open. Only this Host's own ask is excluded from the
            // limit, so a slow typist does not turn it into a `timeout`.
            let (waited, timed_out) = interrupt::wait_bounded(
                &mut running,
                pid,
                limit,
                &host.name,
                prompts.as_ref().map(|p| &p.clock),
            )
            .await;
            if pid != 0 {
                interrupt.deregister(&host.name);
            }
            let (stdout, stdout_truncated) = stdout_task.await.unwrap_or_default();
            let (stderr, stderr_truncated) = stderr_task.await.unwrap_or_default();

            // A Host rshx killed is `cancelled` whatever exit status its death
            // produced: ssh exits 255 on SIGTERM, which would otherwise read
            // as `unreachable`. An interrupt outranks a timeout.
            let killed = interrupt.was_killed(&host.name);
            let exit_code = waited.as_ref().ok().and_then(|status| status.code());
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
            let stderr = match waited {
                Ok(_) => stderr,
                Err(err) => format!("rshx: could not read ssh's exit status: {err}\n").into_bytes(),
            };
            Outcome {
                host: host.name.clone(),
                status,
                // Absent when rshx ended the child: its exit status says
                // nothing about the command, which may still be running.
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

/// The most of each stream kept in memory; past this the bytes are dropped.
const STREAM_CAP: usize = 1024 * 1024;

/// Reads a stream, keeping at most `cap` bytes. The rest is drained and
/// thrown away: a Host that prints more than the cap must not grow rshx
/// without bound, nor block on a full pipe, since ssh would then never exit.
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
            // The status comes from ssh's exit status, never a read error.
            Err(_) => break,
        }
    }
    (kept, truncated)
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
            Status::Ok | Status::Cancelled => {}
        }
    }
    code
}
