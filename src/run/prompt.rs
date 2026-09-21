//! Asking the user for a password.
//!
//! `--privilege` runs the command under a remote sudo, which reads its password
//! from stdin and prints a marker on stderr when it wants one. Whichever Host is
//! asking, the prompt goes to the one terminal the run has — and that terminal
//! belongs to the main task, which also owns the heartbeat — so every ask
//! travels there, and the answer travels back.
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::ChildStdin;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use crate::heartbeat::Heartbeat;
use crate::host::Host;
use crate::interrupt::Interrupt;
use crate::privilege::{self, Privilege, PromptClock, Request};
use crate::run::stream::read_capped;

/// The passwords a run asks for, and where a Host's stderr reader asks. Held
/// by every Host's task, so the clock and the ask channel reach whoever asks.
#[derive(Clone)]
pub(super) struct Prompts {
    /// What a Host spends waiting for its password, outside its `--timeout`.
    clock: PromptClock,
    privilege: Arc<Privilege>,
    requests: mpsc::UnboundedSender<Request>,
}

impl Prompts {
    /// What a Host has spent waiting for its password, for its `--timeout`.
    pub(super) fn clock(&self) -> &PromptClock {
        &self.clock
    }

    /// The password this Host's command was given, if the run has one.
    ///
    /// Asked when a Host rshx cut short has to be stopped: a command under
    /// `sudo` belongs to root, and a stop must not prompt for a password the
    /// run already has.
    pub(super) fn password_for(&self, host: &Host) -> Option<Vec<u8>> {
        self.privilege
            .password_for(&host.name, host.unique_privilege_pass)
    }

    /// Answers the Host that asked: the prompt goes where the progress line is
    /// drawn, the password back to the Host. Says whether the run can keep
    /// asking — one that cannot stops, and the Hosts still waiting are released.
    pub(super) async fn serve(
        &self,
        request: Request,
        interrupt: &Interrupt,
        heartbeat: &Heartbeat,
    ) -> Asking {
        // Off the terminal while the user types: the prompt is written where
        // the progress line is drawn.
        heartbeat.pause();
        let answer = self.privilege.answer(&request.host, request.unique).await;
        heartbeat.resume();
        let (password, asking) = match answer {
            privilege::Answer::Password(password) => (Some(password), Asking::On),
            // Nothing to write: the Host's sudo fails on its own, and the run
            // stops rather than repeating the failure.
            privilege::Answer::Refused => (
                None,
                Asking::Done {
                    reason: self.privilege.fatal(),
                },
            ),
            // Ctrl-C on the prompt. Not a failure: the run stops the way any
            // interrupt stops it, and the Hosts in flight are cancelled.
            privilege::Answer::Interrupted => (None, Asking::Done { reason: None }),
        };
        if matches!(asking, Asking::Done { .. }) {
            interrupt.trigger();
        }
        // Whatever the answer, the Host stops waiting for it: `None` is a
        // reader that will not ask again.
        let _ = request.reply.send(password);
        asking
    }
}

/// Whether the run can still answer a Host.
pub(super) enum Asking {
    /// The Host has its answer; the run goes on, and another Host may ask.
    On,
    /// The run cannot ask any more: nothing was typed, or the prompt was
    /// interrupted. `reason` is rshx's own failure, when there is one to report.
    Done { reason: Option<String> },
}

/// The run's end of the ask channel: the Hosts asking for a password, in the
/// order they asked.
pub(super) type Asks = UnboundedReceiver<Request>;

/// The passwords a run asks for, when `--privilege` asks for any.
pub(super) fn init(privilege: bool) -> (Option<Prompts>, Option<Asks>) {
    if privilege {
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

/// The next Host asking for a password.
pub(super) async fn next_request(asks: &mut Option<Asks>) -> Option<Request> {
    match asks {
        Some(asks) => asks.recv().await,
        // No `--privilege`, so no Host can ask.
        None => std::future::pending().await,
    }
}

/// One Host's stderr reader, and its means of answering a password prompt.
pub(super) struct Ask {
    prompts: Prompts,
    host: String,
    /// Whether this Host wants a password of its own rather than the run's.
    unique: bool,
    /// The Host's ssh stdin, which reaches the remote sudo.
    stdin: ChildStdin,
}

impl Ask {
    /// The reader for one Host, when the run has a password to give it: a
    /// Host's stdin is piped only under `--privilege`, and it is what reaches
    /// the remote sudo.
    pub(super) fn new(prompts: Prompts, host: &Host, stdin: ChildStdin) -> Ask {
        Ask {
            prompts,
            host: host.name.clone(),
            unique: host.unique_privilege_pass,
            stdin,
        }
    }

    /// Asks the run for this Host's password, and writes it to the Host.
    /// Answers whether the run can still answer; asking again never returns.
    async fn answer(&mut self) -> bool {
        let Some(password) = ask_for_password(&self.prompts, &self.host, self.unique).await else {
            return false;
        };
        // The password is written to the Host's ssh, which forwards it to the
        // remote sudo reading its stdin. A newline ends it: sudo reads a line.
        self.stdin.write_all(&password).await.is_ok()
            && self.stdin.write_all(b"\n").await.is_ok()
            && self.stdin.flush().await.is_ok()
    }
}

/// Asks the run for one Host's password, and answers it. The ask goes to the
/// main task, which owns the terminal and the heartbeat: one prompt for the
/// run, whoever needs it and whatever they need it for.
///
/// `None` means the run cannot answer — nowhere to ask, or nothing typed —
/// which is already recorded as the run's own failure.
async fn ask_for_password(prompts: &Prompts, host: &str, unique: bool) -> Option<Vec<u8>> {
    let (reply, answer) = tokio::sync::oneshot::channel();
    let request = Request {
        host: host.to_string(),
        unique,
        reply,
    };
    // Timed from here, not from when the user starts typing: the Host is
    // blocked on rshx until its answer comes back. Marked before the ask goes
    // out, so one queued behind another Host's prompt still counts.
    let waiting = prompts.clock.asking_now(host);
    if prompts.requests.send(request).is_err() {
        return None;
    }
    let password = answer.await.ok().flatten();
    // Whatever the password is written to is the Host's own time again.
    drop(waiting);
    password
}

/// Reads a Host's stderr, answering password prompts as they appear. The
/// markers are stripped on the way into the report: they are rshx's own
/// plumbing, not something the remote command printed.
pub(super) async fn read_stderr<R>(stream: R, cap: usize, ask: Option<Ask>) -> (Vec<u8>, bool)
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
