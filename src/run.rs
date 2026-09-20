//! Running the command on Hosts, listing the Hosts a run would select, and
//! deciding what a run's outcome means.
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use futures_util::{StreamExt, stream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use crate::cause::Cause;
use crate::cli::{Cli, CliCommand, CliOptions};
use crate::heartbeat::{self, Heartbeat};
use crate::host::{self, Host};
use crate::interrupt::{self, Interrupt};
use crate::privilege::{self, Privilege, PromptClock, Request};
use crate::remote;
use crate::report::{self, Reporter};
use crate::run::command::execute_command;
use crate::run::script::execute_local_script;
use crate::{EXIT_FAILED, EXIT_INTERRUPTED, EXIT_LOCAL, EXIT_OK, EXIT_UNREACHABLE};

mod command;
mod list;
mod script;

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
    /// Whether rshx stopped the Host's remote command. `false` on a Host rshx
    /// cut short means the command may still be running there: the report says
    /// so rather than claiming work that was not done.
    pub remote_stopped: bool,
    pub duration: Duration,
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
        }
    }
}

/// Loads the host file, then answers the subcommand the command line asked for.
pub async fn execute(cli: &Cli) -> Result<u8> {
    let options = &cli.options;
    let path = host::resolve_path(options.host_file.as_deref())?;
    let file = host::load(&path)?;
    let selected = file.select(&options.groups)?;

    match &cli.sub_command {
        // `list` answers from the host file alone: nothing runs, so no Host has
        // an outcome, and a listing is not a report.
        CliCommand::List => list::execute(&selected, options),

        CliCommand::Run(args) => {
            let mut reporter = reporter(options);

            // A script is copied to each Host and run there.
            if let Some(script_path) = &args.script {
                return execute_local_script(&selected, script_path, options, &mut reporter).await;
            }

            // A command is handed to each Host's own ssh to run.
            let command = if options.privilege {
                // A command that runs sudo itself gets rshx's sudo in front of it and
                // elevates a second time: rshx reads none of the command's own options —
                // that would mean knowing sudo's grammar — so it warns rather than guesses.
                privilege::under_sudo(&args.command)
            } else {
                args.command.clone()
            };

            // `--privilege` wraps the command once, before anything runs, so every Host
            // gets that same command: a remote sudo reads its password from stdin.
            // Before the heading, so what rshx says about the command is read first.
            // Asked of the command as it was typed: `command` is already wrapped, and
            // its own leading `sudo` is rshx's.
            if options.privilege && privilege::runs_sudo(&args.command) {
                reporter.warning(
                    "the command runs sudo itself; --privilege puts its own sudo in front of it",
                );
            }

            execute_command(&selected, &command, options, &mut reporter).await
        }

        CliCommand::Ping => {
            let mut reporter = reporter(options);

            execute_command(
                &selected,
                &["echo".to_string(), "pong".to_string()],
                options,
                &mut reporter,
            )
            .await
        }
    }
}

/// The reporter a run writes through. Chrome goes on stderr, and only when a
/// terminal is watching it: a redirected stderr must stay free of it, and
/// `--json` must be parseable.
fn reporter(options: &CliOptions) -> Reporter {
    let chrome = !options.json && std::io::IsTerminal::is_terminal(&std::io::stderr());
    Reporter::new(
        options.color,
        report::Detail {
            stdout: !options.quiet,
            stderr: options.stderr,
        },
        if options.json {
            report::Format::Json
        } else {
            report::Format::Plain
        },
        chrome,
    )
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

/// The most of each stream kept in memory; past this the bytes are dropped.
const STREAM_CAP: usize = 1024 * 1024;

/// Reads a stream, keeping at most `cap` bytes. The rest is drained and
/// thrown away: a Host that prints more than the cap must not grow rshx
/// without bound, nor block on a full pipe, since ssh would then never exit.
pub(crate) async fn read_capped<R>(mut stream: R, cap: usize) -> (Vec<u8>, bool)
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

async fn run_remote_command(
    mut child: Command,
    host: &Host,
    interrupt: &Interrupt,
    limit: Option<Duration>,
    prompts: Option<Prompts>,
    marker: Option<&str>,
) -> Outcome {
    // Its own process group, so a terminal's interrupt reaches rshx alone and
    // rshx decides when its children die.
    child.process_group(0);

    let started = Instant::now();
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
            let mut stderr = match waited {
                Ok(_) => stderr,
                Err(err) => format!("rshx: could not read ssh's exit status: {err}\n").into_bytes(),
            };
            // The Host's own time, taken before the stop below: that is rshx
            // tidying up after the Host, not the Host still working.
            let duration = started.elapsed();
            // Killing a Host's ssh does not stop what it started, so a Host
            // rshx cut short is asked to stop its own command, by the marker
            // written before that command ran.
            let mut remote_stopped = false;
            if (killed || timed_out)
                && let Some(marker) = marker
            {
                let password = prompts.as_ref().and_then(|prompts| {
                    prompts
                        .privilege
                        .password_for(&host.name, host.unique_privilege_pass)
                });
                match remote::stop(host, marker, prompts.is_some(), password.as_deref()).await {
                    Ok(()) => remote_stopped = true,
                    // The Host's status is settled; this says what rshx could
                    // not finish, which its own output cannot.
                    Err(reason) => stderr.extend_from_slice(
                        format!("rshx: could not stop the remote command: {reason}\n").as_bytes(),
                    ),
                }
            }
            Outcome {
                host: host.name.clone(),
                status,
                // Absent when rshx ended the child: its exit status says
                // nothing about the command, which rshx stops on its own.
                exit_code: if killed || timed_out { None } else { exit_code },
                cause,
                stdout,
                stderr,
                stdout_truncated,
                stderr_truncated,
                remote_stopped,
                duration,
            }
        }
        // rshx's own failure to spawn ssh, not a diagnosis of the remote.
        Err(err) => Outcome::local_failure(
            &host.name,
            started.elapsed(),
            format!("could not run ssh: {err}"),
        ),
    }
}

fn init_prompts_and_asks(
    options: &CliOptions,
) -> (Option<Prompts>, Option<UnboundedReceiver<Request>>) {
    if options.privilege {
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
    }
}

async fn execute_on_hosts<F>(
    selected: &Vec<&Host>,
    options: &CliOptions,
    reporter: &mut Reporter,
    per_host: F,
) -> Result<u8>
where
    F: AsyncFn(&Host, &Interrupt, Option<Prompts>) -> Outcome,
{
    let started = Instant::now();
    let total = selected.len();

    let (prompts, mut asks) = init_prompts_and_asks(options);

    let heartbeat = Rc::new(Heartbeat::new(total, reporter.chrome()));
    let interrupt = Interrupt::install();
    let per_host = Rc::new(per_host);

    // The pdsh sliding window: at most `fanout` remote commands in flight, each
    // one that finishes replaced by a pending Host.
    let mut settling = stream::iter(selected)
        .map(|host| {
            let interrupt = interrupt.clone();
            let heartbeat = Rc::clone(&heartbeat);
            let prompts = prompts.clone();
            let per_host = Rc::clone(&per_host);
            async move {
                // Checked here, not on entry, so a Host waiting for a slot
                // does not start after an interrupt — and is not reported.
                if interrupt.is_stopped() {
                    return None;
                }
                heartbeat.start(&host.name);
                Some(per_host(host, &interrupt, prompts).await)
            }
        })
        .buffer_unordered(options.fanout as usize);

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
