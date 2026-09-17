//! `--privilege`: answering a remote command that asks for a password.
//!
//! rshx authenticates nothing itself. It runs the command under a `sudo` of its
//! own that reads its password from stdin (`-S`) and announces the ask with a
//! marker of rshx's own (`-p rshx-password:`), watches each Host's stderr for
//! that marker, and writes the password to that Host's stdin when it appears.
//! The ask is what triggers the prompt: a Host whose sudo needs no password
//! never prints the marker, so rshx never asks for a password nobody wants.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

/// The prompt sudo is told to print, so rshx can recognise the ask.
///
/// rshx's own string rather than sudo's default, which is translated and would
/// be recognised only on an English machine. It holds no space, so ssh's
/// joining of the command's arguments cannot split it, and no `%`, which sudo
/// would expand.
pub const MARKER: &str = "rshx-password:";

/// The command as it is run: rshx's own sudo, then the command verbatim.
///
/// rshx never reads the command's own sudo options — that would mean
/// reimplementing sudo's option grammar, and being wrong about it some day. So
/// the command is always wrapped, and one that runs sudo itself ends up with a
/// second elevation inside rshx's. `runs_sudo` is what lets the caller say so.
pub fn under_sudo(command: &[String]) -> Vec<String> {
    let mut rewritten = Vec::with_capacity(command.len() + 4);
    rewritten.push("sudo".to_string());
    rewritten.push("-S".to_string());
    rewritten.push("-p".to_string());
    rewritten.push(MARKER.to_string());
    rewritten.extend_from_slice(command);
    rewritten
}

/// Whether the command already runs sudo itself.
///
/// Only ever used to warn: the wrapper is the same either way.
pub fn runs_sudo(command: &[String]) -> bool {
    command.first().is_some_and(|word| is_sudo(word))
}

/// Whether an argument names sudo itself: `sudo`, or a path to it.
fn is_sudo(word: &str) -> bool {
    Path::new(word)
        .file_name()
        .is_some_and(|name| name == "sudo")
}

/// Removes rshx's marker from a Host's stderr, and reports where it was.
///
/// The marker is stripped rather than shown: it is rshx's own plumbing, and
/// sudo prints it once per attempt, so a rejected password would otherwise put
/// two `rshx-password:` lines in the report. Bytes are held back until they can
/// no longer be the start of a marker, so a marker split across two reads is
/// still recognised — and still stripped.
#[derive(Default)]
pub struct MarkerFilter {
    /// The tail of the previous chunk, kept only while it could still be the
    /// beginning of a marker.
    held: Vec<u8>,
}

/// What filtering one chunk of a stream held.
#[derive(Default, Clone, Copy)]
pub struct Filtered {
    /// Whether this chunk carried a marker. sudo prints one per attempt, so a
    /// rejected password produces a second ask: every one of them is a read
    /// that wants a password written to it.
    pub asked: bool,
    /// Whether bytes were dropped, because the stream is past its cap.
    pub dropped: bool,
}

impl MarkerFilter {
    /// Filters one chunk into `kept`.
    pub fn push(&mut self, chunk: &[u8], kept: &mut Vec<u8>, cap: usize) -> Filtered {
        let mut text = std::mem::take(&mut self.held);
        text.extend_from_slice(chunk);

        let mut filtered = Filtered::default();
        let mut start = 0;
        while let Some(at) = find(&text[start..], MARKER.as_bytes()) {
            let at = start + at;
            filtered.dropped |= !self.keep(&text[start..at], kept, cap);
            filtered.asked = true;
            start = at + MARKER.len();
        }

        // Everything from the last marker on could still be the start of
        // another, so only the part that cannot be is written now.
        let tail = &text[start..];
        let safe = safe_prefix(tail);
        filtered.dropped |= !self.keep(&tail[..safe], kept, cap);
        self.held = tail[safe..].to_vec();
        filtered
    }

    /// Writes whatever was held back, once the stream has ended.
    pub fn finish(&mut self, kept: &mut Vec<u8>, cap: usize) -> bool {
        let held = std::mem::take(&mut self.held);
        !self.keep(&held, kept, cap)
    }

    /// Appends up to the cap, and answers whether everything fit.
    fn keep(&self, bytes: &[u8], kept: &mut Vec<u8>, cap: usize) -> bool {
        let room = cap.saturating_sub(kept.len());
        kept.extend_from_slice(&bytes[..room.min(bytes.len())]);
        room >= bytes.len()
    }
}

/// The first occurrence of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// How much of `tail` cannot be the start of a marker, and so is safe to write.
fn safe_prefix(tail: &[u8]) -> usize {
    let marker = MARKER.as_bytes();
    let longest = tail.len().min(marker.len() - 1);
    for len in (1..=longest).rev() {
        if tail[tail.len() - len..] == marker[..len] {
            return tail.len() - len;
        }
    }
    tail.len()
}

/// A Host's stderr reader telling the run that its command asked for a password.
///
/// The ask is noticed by whichever task is reading that Host's stderr, but it is
/// answered by the main task, which owns the terminal and the heartbeat.
pub struct Request {
    pub host: String,
    /// Whether the Host is marked `unique_privilege_pass`, and so asks for a
    /// password of its own rather than the run's.
    pub unique: bool,
    pub reply: tokio::sync::oneshot::Sender<Option<Vec<u8>>>,
}

/// The passwords a run can need.
pub struct Privilege {
    /// The password every Host shares, and the Hosts it has already been given
    /// to.
    shared: Mutex<Shared>,
    /// The passwords of Hosts marked `unique_privilege_pass`.
    unique: Mutex<HashMap<String, Unique>>,
    /// Why the run cannot go on, once it cannot: there was nowhere to ask, or
    /// nothing was typed.
    fatal: Mutex<Option<String>>,
    clock: PromptClock,
}

/// The run's shared password, and who has had it.
///
/// Who has had it is what tells a retry from a first ask: sudo prints the
/// marker again only after rejecting a password, so a Host asking twice has had
/// the one it was given refused, and writing that same password again would
/// only burn sudo's remaining tries.
#[derive(Default)]
struct Shared {
    password: Option<Vec<u8>>,
    given: std::collections::HashSet<String>,
}

/// The password of a Host that has one of its own, and whether it has been
/// given it yet.
#[derive(Default)]
struct Unique {
    password: Option<Vec<u8>>,
    tried: bool,
}

impl Default for Privilege {
    fn default() -> Privilege {
        Privilege {
            shared: Mutex::new(Shared::default()),
            unique: Mutex::new(HashMap::new()),
            fatal: Mutex::new(None),
            clock: PromptClock::default(),
        }
    }
}

impl Privilege {
    /// The password this Host's command should be given, asking the user for one
    /// if the run has none this Host has not already tried.
    ///
    /// The ask happens on a thread of its own: reading a password is a person
    /// typing, which must not hold up the runtime.
    pub async fn answer(self: &Arc<Self>, host: &str, unique: bool) -> Answer {
        if let Some(password) = self.untried(host, unique) {
            return Answer::Password(password);
        }

        let this = Arc::clone(self);
        let host = host.to_string();
        match tokio::task::spawn_blocking(move || this.ask(&host, unique)).await {
            Ok(answer) => answer,
            // The prompt thread is gone, so the run cannot answer. Nothing was
            // written, and this Host's sudo will fail on its own.
            Err(err) => {
                self.fail(format!("could not ask for a password: {err}"));
                Answer::Refused
            }
        }
    }

    /// Asks for a password on the terminal. Blocking: it is a person typing.
    fn ask(&self, host: &str, unique: bool) -> Answer {
        // A Host asking a second time had its password refused, so the prompt
        // says which Host it is for: with a run of Hosts with passwords of
        // their own, the next prompt may well be about another one.
        let text = format!("privilege password for {host}: ");
        match prompt(&text) {
            Typed::Password(password) => {
                self.remember(host, unique, &password);
                Answer::Password(password)
            }
            // Ctrl-C on the terminal. Not a failure: the run stops the way any
            // interrupt stops it, and the Hosts in flight are cancelled.
            Typed::Interrupted => Answer::Interrupted,
            Typed::Empty => {
                self.fail("the password was empty; nothing was attempted");
                Answer::Refused
            }
            Typed::NoTerminal(err) => {
                self.fail(format!(
                    "--privilege needs a terminal to ask for a password: {err}"
                ));
                Answer::Refused
            }
        }
    }

    /// Why the run cannot go on, if it cannot.
    pub fn fatal(&self) -> Option<String> {
        lock(&self.fatal).clone()
    }

    /// What prompting has cost so far, for a Host's `--timeout`.
    pub fn clock(&self) -> PromptClock {
        self.clock.clone()
    }

    /// A password this Host has not been given yet, if the run has one.
    fn untried(&self, host: &str, unique: bool) -> Option<Vec<u8>> {
        if unique {
            let mut passwords = lock(&self.unique);
            let entry = passwords.entry(host.to_string()).or_default();
            if entry.tried {
                return None;
            }
            let password = entry.password.clone()?;
            entry.tried = true;
            Some(password)
        } else {
            let mut shared = lock(&self.shared);
            if shared.given.contains(host) {
                return None;
            }
            let password = shared.password.clone()?;
            shared.given.insert(host.to_string());
            Some(password)
        }
    }

    fn remember(&self, host: &str, unique: bool, password: &[u8]) {
        if unique {
            lock(&self.unique).insert(
                host.to_string(),
                Unique {
                    password: Some(password.to_vec()),
                    tried: true,
                },
            );
        } else {
            let mut shared = lock(&self.shared);
            // A new password replaces the old one, and every Host is due to be
            // given it again: the ones that took the old one are exactly the
            // ones whose sudo has just refused it.
            shared.password = Some(password.to_vec());
            shared.given.clear();
            shared.given.insert(host.to_string());
        }
    }

    fn fail(&self, reason: impl Into<String>) {
        let mut fatal = lock(&self.fatal);
        // The first failure is the one worth reporting: a Host asking after it
        // would only restate it.
        if fatal.is_none() {
            *fatal = Some(reason.into());
        }
    }
}

/// A lock, with poisoning ignored: a panic while a password was being handled
/// must not take the rest of the run down with it.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// How long each Host has spent blocked on rshx for a password.
///
/// Per Host, not per run. Only the Hosts whose ask is out are stopped at a
/// prompt: the rest carry on running, and a `--timeout` is a bound on what each
/// Host's own command does. Charging one Host's ask to the whole run would let a
/// slow typist hold a healthy Host open past its limit — or, worse, let a
/// prompt keep a wedged Host from ever timing out.
#[derive(Clone, Default)]
pub struct PromptClock {
    state: Arc<ClockState>,
}

#[derive(Default)]
struct ClockState {
    hosts: Mutex<HashMap<String, Waiting>>,
    /// Fires when any Host's ask goes out or comes back, so a deadline can be
    /// recomputed rather than expiring on time that was not the Host's.
    changed: Notify,
}

/// What one Host has spent waiting for a password, and whether it is waiting
/// now.
#[derive(Default)]
struct Waiting {
    /// When the ask went out, while it is still out.
    since: Option<Instant>,
    /// What this Host has already spent waiting.
    spent: Duration,
}

impl PromptClock {
    /// How long this Host has been waiting for a password, including a wait in
    /// progress.
    pub fn spent_by(&self, host: &str) -> Duration {
        match lock(&self.state.hosts).get(host) {
            Some(waiting) => waiting.total(),
            None => Duration::ZERO,
        }
    }

    /// Whether this Host is waiting for a password right now.
    fn asking(&self, host: &str) -> bool {
        lock(&self.state.hosts)
            .get(host)
            .is_some_and(|waiting| waiting.since.is_some())
    }

    /// Resolves once this Host is not waiting for a password, including right
    /// now.
    pub async fn wait_out_prompt(&self, host: &str) {
        loop {
            let changed = self.state.changed.notified();
            tokio::pin!(changed);
            // Registered before the check, so an ask that comes back in between
            // still wakes the wait below rather than being missed.
            changed.as_mut().enable();
            if !self.asking(host) {
                return;
            }
            changed.await;
        }
    }

    /// Resolves when any Host's ask goes out or comes back.
    pub async fn changed(&self) {
        self.state.changed.notified().await;
    }

    /// Times this Host's ask, and wakes every waiting deadline when it goes out
    /// and when it comes back.
    ///
    /// Called by the Host's own stderr reader rather than by the run's main
    /// loop: the Host starts waiting the moment its ask goes out, whether or
    /// not the loop has got to it.
    pub fn asking_now<'a>(&'a self, host: &'a str) -> Asking<'a> {
        lock(&self.state.hosts)
            .entry(host.to_string())
            .or_default()
            .since = Some(Instant::now());
        self.state.changed.notify_waiters();
        Asking { clock: self, host }
    }
}

impl Waiting {
    /// What this Host has spent waiting, so far.
    fn total(&self) -> Duration {
        match self.since {
            Some(at) => self.spent + at.elapsed(),
            None => self.spent,
        }
    }
}

/// The mark that one Host's ask is out, cleared when the ask comes back.
pub struct Asking<'a> {
    clock: &'a PromptClock,
    host: &'a str,
}

impl Drop for Asking<'_> {
    fn drop(&mut self) {
        let mut hosts = lock(&self.clock.state.hosts);
        if let Some(waiting) = hosts.get_mut(self.host)
            && let Some(at) = waiting.since.take()
        {
            waiting.spent += at.elapsed();
        }
        drop(hosts);
        self.clock.state.changed.notify_waiters();
    }
}

/// What asking the user for a password produced.
pub enum Answer {
    /// They typed one.
    Password(Vec<u8>),
    /// They pressed Ctrl-C: the run stops, the way any interrupt stops it.
    Interrupted,
    /// Nothing was typed, or there was nowhere to ask. `Privilege::fatal` says
    /// which; either way the run cannot answer, so it stops.
    Refused,
}

/// What the terminal gave back.
enum Typed {
    /// A password, as typed.
    Password(Vec<u8>),
    /// They pressed Ctrl-C.
    Interrupted,
    /// They pressed Enter on an empty line.
    Empty,
    /// There is nowhere to ask.
    NoTerminal(String),
}

/// Asks for a password on the terminal, without echoing it.
///
/// The terminal rather than stdin: the report's own streams are often pipes —
/// `rshx … | jq` — and a redirected stdin has to stay free for whatever put it
/// there.
fn prompt(text: &str) -> Typed {
    let mut tty = match OpenOptions::new().read(true).write(true).open("/dev/tty") {
        Ok(tty) => tty,
        Err(err) => return Typed::NoTerminal(format!("/dev/tty: {err}")),
    };
    let raw = match Raw::new(&tty) {
        Ok(raw) => raw,
        Err(err) => return Typed::NoTerminal(format!("/dev/tty: {err}")),
    };

    let _ = tty.write_all(text.as_bytes());
    let _ = tty.flush();

    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match tty.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => match byte[0] {
                b'\n' | b'\r' => break,
                // Ctrl-C, delivered as a byte because the terminal's signals
                // are off: rshx has to put the terminal back before it stops.
                0x03 => return Typed::Interrupted,
                // Ctrl-D, and backspace, which is unseen and so has to work.
                0x04 => break,
                0x7f | 0x08 => {
                    line.pop();
                }
                byte => line.push(byte),
            },
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    drop(raw);

    // The user's Enter was not echoed, so the next line would otherwise be
    // written over the prompt.
    let _ = tty.write_all(b"\n");
    let _ = tty.flush();

    if line.is_empty() {
        Typed::Empty
    } else {
        Typed::Password(line)
    }
}

/// The terminal in the mode a password is typed in: no echo, a byte at a time,
/// and Ctrl-C delivered as a byte rather than a signal.
///
/// Signals off is what lets rshx restore the terminal before it stops the run:
/// a SIGINT arriving mid-prompt would leave the user's terminal with no echo
/// and no line editing.
struct Raw {
    fd: RawFd,
    saved: libc::termios,
}

impl Raw {
    fn new(tty: &std::fs::File) -> std::io::Result<Raw> {
        let fd = tty.as_raw_fd();
        // SAFETY: `fd` is an open terminal, and tcgetattr writes only `saved`.
        unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut saved) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // copy
            let mut raw = saved;
            // No display for inputed chars, no buffer for read, no normal signals for tty
            raw.c_lflag &= !(libc::ECHO | libc::ICANON | libc::ISIG);
            // Read every one byte
            raw.c_cc[libc::VMIN] = 1;
            // No timeout
            raw.c_cc[libc::VTIME] = 0;
            // Flushed on the way in, so a keystroke typed before the prompt
            // cannot become part of the password.
            // Discard anything unread before setattr
            if libc::tcsetattr(fd, libc::TCSAFLUSH, &raw) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Raw { fd, saved })
        }
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        // SAFETY: `fd` is the terminal this was set on, and `saved` is what it
        // was set from.
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSAFLUSH, &self.saved);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Filtered, MARKER, MarkerFilter, runs_sudo, under_sudo};

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    #[test]
    fn the_command_runs_under_a_sudo_that_reads_stdin() {
        assert_eq!(
            under_sudo(&argv(&["systemctl", "restart", "kubelet"])),
            argv(&[
                "sudo",
                "-S",
                "-p",
                MARKER,
                "systemctl",
                "restart",
                "kubelet"
            ])
        );
    }

    #[test]
    fn a_command_that_runs_sudo_is_wrapped_like_any_other() {
        // No option of the command's is read, moved or dropped: it is wrapped
        // whole, so its own `-p` and `-u` still mean what they say, one
        // elevation further in.
        for command in [
            argv(&["sudo", "-u", "postgres", "psql"]),
            argv(&["sudo", "-p", "Password: ", "uptime"]),
            argv(&["sudo", "-A", "uptime"]),
            argv(&["sudo", "--", "true"]),
        ] {
            let mut expected = argv(&["sudo", "-S", "-p", MARKER]);
            expected.extend(command.iter().cloned());
            assert_eq!(under_sudo(&command), expected, "{command:?}");
        }
    }

    #[test]
    fn a_command_that_runs_sudo_is_noticed() {
        // What the warning is built on: sudo, or a path to it, as the command
        // word.
        for command in [
            argv(&["sudo", "uptime"]),
            argv(&["/usr/bin/sudo", "uptime"]),
        ] {
            assert!(runs_sudo(&command), "{command:?}");
        }
        for command in [
            argv(&["uptime"]),
            argv(&["systemctl", "restart", "sudo"]),
            argv(&["sh", "-c", "sudo uptime"]),
        ] {
            assert!(!runs_sudo(&command), "{command:?}");
        }
    }

    fn filtered(chunks: &[&[u8]]) -> (Vec<u8>, Filtered) {
        let mut filter = MarkerFilter::default();
        let mut kept = Vec::new();
        let mut asked = false;
        let mut dropped = false;
        for chunk in chunks {
            let filtered = filter.push(chunk, &mut kept, 1 << 20);
            asked |= filtered.asked;
            dropped |= filtered.dropped;
        }
        dropped |= filter.finish(&mut kept, 1 << 20);
        (kept, Filtered { asked, dropped })
    }

    #[test]
    fn the_marker_is_stripped_and_every_one_is_an_ask() {
        let (kept, filtered) = filtered(&[b"rshx-password:Sorry, try again.\nrshx-password:\n"]);
        assert_eq!(kept, b"Sorry, try again.\n\n");
        assert!(filtered.asked);
    }

    #[test]
    fn a_marker_split_across_reads_is_still_one_ask() {
        // sudo writes the prompt in one go, but nothing promises the read
        // returns it in one go.
        let (kept, filtered) = filtered(&[b"rshx-", b"pass", b"word:x"]);
        assert_eq!(kept, b"x");
        assert!(filtered.asked);
    }

    #[test]
    fn text_that_only_looks_like_a_marker_is_kept() {
        let (kept, filtered) = filtered(&[b"rshx-passthrough", b" and rshx"]);
        assert_eq!(kept, b"rshx-passthrough and rshx");
        assert!(!filtered.asked);
    }

    #[test]
    fn a_stream_past_the_cap_is_reported_as_dropped() {
        let mut filter = MarkerFilter::default();
        let mut kept = Vec::new();
        let filtered = filter.push(&vec![b'x'; 4096], &mut kept, 16);
        assert!(filtered.dropped);
        assert_eq!(kept.len(), 16);
    }
}
