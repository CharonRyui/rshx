//! Test harness: a scripted stand-in for `ssh`.
//!
//! Every integration test runs rshx against a fake `ssh` first on `PATH`,
//! which records each invocation's argv and interval and replays a response
//! scripted per destination, so no network or sshd is needed.

#![allow(dead_code)]

use std::fs;
use std::os::fd::FromRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

/// Held while an executable is written, and while a process is spawned.
///
/// A forked child holds a copy of every descriptor this process has open until
/// it reaches `exec`, including one another thread is writing the fake ssh
/// with; `exec` of a file with an open write descriptor fails with `ETXTBSY`.
/// The lock is held only across the fork and the write, never while a child
/// runs.
static EXEC_LOCK: Mutex<()> = Mutex::new(());

/// The exec lock, ignoring poisoning: a test that panicked mid-spawn must not
/// take every other test down with it.
fn exec_lock() -> MutexGuard<'static, ()> {
    EXEC_LOCK.lock().unwrap_or_else(|err| err.into_inner())
}

/// Spawns a command, holding the exec lock across the fork. Use this instead
/// of `Command::spawn` so the fork cannot overlap a write to an executable.
pub fn spawn(cmd: &mut Command) -> Child {
    let _guard = exec_lock();
    cmd.spawn().expect("spawn")
}

/// Like `Command::output`, but the exec lock is not held while it runs.
pub fn output(cmd: &mut Command) -> Output {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = spawn(cmd);
    child.wait_with_output().expect("wait for the child")
}

/// What the fake ssh does for one destination.
#[derive(Clone, Debug)]
pub struct Response {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    code: i32,
    delay_ms: u64,
    /// When set, the fake ssh asks for a password the way `sudo -S` does: it
    /// prints rshx's `-p` prompt, reads a line, and fails unless it matches.
    password: Option<String>,
    /// When set, the fake ssh keeps every byte it reads on stdin, so a test
    /// can assert what rshx sent the Host. Never on with a password prompt:
    /// that path reads stdin a line at a time.
    capture_stdin: bool,
}

impl Response {
    pub fn ok() -> Response {
        Response {
            stdout: Vec::new(),
            stderr: Vec::new(),
            code: 0,
            delay_ms: 0,
            password: None,
            capture_stdin: false,
        }
    }

    /// Exits non-zero, the way a remote command that fails does.
    pub fn failed(code: i32) -> Response {
        Response {
            code,
            ..Response::ok()
        }
    }

    /// Exits 255, the way ssh reports that it could not connect.
    pub fn unreachable() -> Response {
        Response {
            code: 255,
            stderr: b"ssh: connect to host example port 22: Connection refused\n".to_vec(),
            ..Response::ok()
        }
    }

    pub fn stdout(mut self, text: &str) -> Response {
        self.stdout = text.as_bytes().to_vec();
        self
    }

    pub fn stderr(mut self, text: &str) -> Response {
        self.stderr = text.as_bytes().to_vec();
        self
    }

    /// A stream that is not valid UTF-8, or is simply large.
    pub fn stdout_bytes(mut self, bytes: &[u8]) -> Response {
        self.stdout = bytes.to_vec();
        self
    }

    pub fn code(mut self, code: i32) -> Response {
        self.code = code;
        self
    }

    pub fn delay_ms(mut self, ms: u64) -> Response {
        self.delay_ms = ms;
        self
    }

    /// Asks for a password, the way a remote `sudo -S` does, and fails when
    /// the password is not `password`. The prompt printed is the one rshx set
    /// with `-p`, so a run that set none prompts nothing and reads no stdin.
    pub fn prompt(mut self, password: &str) -> Response {
        self.password = Some(password.to_string());
        self
    }

    /// Keeps every byte that arrives on stdin, so a test can assert what rshx
    /// sent the Host. rshx sends a script this way.
    pub fn captures_stdin(mut self) -> Response {
        self.capture_stdin = true;
        self
    }
}

/// A temporary directory holding a fake `ssh` and everything it records.
pub struct Harness {
    dir: PathBuf,
}

impl Harness {
    pub fn new() -> Harness {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("rshx-it-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::create_dir_all(dir.join("resp")).unwrap();
        fs::create_dir_all(dir.join("xdg")).unwrap();

        let stub = dir.join("bin/ssh");
        {
            // Held against a concurrent spawn: see `EXEC_LOCK`.
            let _guard = exec_lock();
            fs::write(&stub, STUB).unwrap();
            let mut perms = fs::metadata(&stub).unwrap().permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
            fs::set_permissions(&stub, perms).unwrap();
        }

        Harness { dir }
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// The fake ssh executable, for driving it directly.
    pub fn stub(&self) -> PathBuf {
        self.dir.join("bin/ssh")
    }

    fn stub_env(&self) -> String {
        self.dir.display().to_string()
    }

    pub fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, contents).unwrap();
        path
    }

    /// Scripts what the fake ssh does when the destination is `dest`.
    pub fn respond(&self, dest: &str, response: Response) -> &Harness {
        self.script_response(&self.dir.join("resp").join(dest), response);
        self
    }

    /// Scripts the fallback for a destination with no script of its own.
    pub fn respond_default(&self, response: Response) -> &Harness {
        self.script_response(&self.dir.join("resp").join("default"), response);
        self
    }

    /// Scripts a destination whose answer changes with each invocation: the
    /// first entry answers the first invocation, the second the second, and the
    /// last answers every one after it. A command rshx sends twice — a script
    /// copied to a Host, then run on it — is scripted this way.
    pub fn respond_sequence(&self, dest: &str, responses: &[Response]) -> &Harness {
        let base = self.dir.join("resp").join(dest);
        for (index, response) in responses.iter().enumerate() {
            self.script_response(
                &base.join("seq").join((index + 1).to_string()),
                response.clone(),
            );
        }
        self
    }

    /// Every byte a Host's ssh read on stdin, for a response that asked to keep
    /// them with `Response::captures_stdin`. Empty when nothing arrived, or
    /// when no response for that destination asked.
    pub fn raw_stdin(&self, dest: &str) -> Vec<u8> {
        fs::read(self.dir.join("stdin_raw").join(dest)).unwrap_or_default()
    }

    fn script_response(&self, dir: &Path, response: Response) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("out"), &response.stdout).unwrap();
        fs::write(dir.join("err"), &response.stderr).unwrap();
        fs::write(dir.join("code"), response.code.to_string()).unwrap();
        fs::write(
            dir.join("delay"),
            format!(
                "{}.{:03}",
                response.delay_ms / 1000,
                response.delay_ms % 1000
            ),
        )
        .unwrap();
        match &response.password {
            Some(password) => {
                // `prompt` makes the stub ask; `password` is what it accepts.
                fs::write(dir.join("prompt"), b"1").unwrap();
                fs::write(dir.join("password"), password).unwrap();
            }
            None => {
                let _ = fs::remove_file(dir.join("prompt"));
            }
        }
        match response.capture_stdin {
            true => {
                fs::write(dir.join("capture_stdin"), b"1").unwrap();
            }
            false => {
                let _ = fs::remove_file(dir.join("capture_stdin"));
            }
        }
    }

    /// A command that runs the rshx binary against this harness.
    pub fn rshx(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_rshx"));
        let path = std::env::var("PATH").unwrap_or_default();
        cmd.current_dir(&self.dir)
            .env("PATH", format!("{}:{path}", self.dir.join("bin").display()))
            .env("RSHX_STUB_DIR", self.stub_env())
            .env("XDG_CONFIG_HOME", self.dir.join("xdg"))
            .env_remove("NO_COLOR");
        cmd
    }

    /// Every invocation the fake ssh saw, as its argv.
    pub fn invocations(&self) -> Vec<Vec<String>> {
        self.records("argv", '\u{1f}')
    }

    /// Every password the fake ssh read from its stdin, as `(destination,
    /// password)`. rshx writes it to the Host's ssh, which forwards it to the
    /// remote command's stdin; one entry per read, so a Host that asked twice
    /// appears twice, and one never asked appears not at all.
    pub fn passwords(&self) -> Vec<(String, String)> {
        self.read_records("stdin")
            .iter()
            .filter_map(|line| {
                let (dest, password) = line.split_once(' ')?;
                Some((dest.to_string(), password.to_string()))
            })
            .collect()
    }

    /// One entry per fake ssh run: its pid, its process group and its session.
    pub fn processes(&self) -> Vec<Process> {
        self.read_records("procs")
            .iter()
            .filter_map(|line| {
                let fields: Vec<&str> = line.split_whitespace().collect();
                match fields.as_slice() {
                    ["proc", pid, pgid, sid] => Some(Process {
                        pid: pid.parse().ok()?,
                        pgid: pgid.parse().ok()?,
                        sid: sid.parse().ok()?,
                    }),
                    _ => None,
                }
            })
            .collect()
    }

    /// Every fake ssh start and end, oldest first.
    pub fn timeline(&self) -> Vec<Tick> {
        let mut ticks: Vec<Tick> = self
            .read_records("timeline")
            .iter()
            .filter_map(|line| {
                let mut fields = line.split(' ');
                let (Some(kind), Some(at), Some(dest)) =
                    (fields.next(), fields.next(), fields.next())
                else {
                    return None;
                };
                let started = match kind {
                    "start" => true,
                    "end" => false,
                    _ => return None,
                };
                Some(Tick {
                    at: at.parse().ok()?,
                    started,
                    dest: dest.to_string(),
                })
            })
            .collect();
        ticks.sort();
        ticks
    }

    /// The destinations the fake ssh was started for, in the order it started.
    pub fn starts(&self) -> Vec<String> {
        self.timeline()
            .into_iter()
            .filter(|tick| tick.started)
            .map(|tick| tick.dest)
            .collect()
    }

    /// The highest number of fake ssh processes that were running at once.
    pub fn peak_concurrency(&self) -> usize {
        let (peak, _) = self.concurrency();
        peak
    }

    /// How many fake ssh processes were still running when the last one ended.
    pub fn leftover_concurrency(&self) -> usize {
        let (_, leftover) = self.concurrency();
        leftover
    }

    fn concurrency(&self) -> (usize, usize) {
        let mut events: Vec<(u128, i32)> = Vec::new();
        for line in self.read_records("timeline") {
            let mut fields = line.split(' ');
            let (Some(kind), Some(nanos)) = (fields.next(), fields.next()) else {
                continue;
            };
            let Ok(nanos) = nanos.parse::<u128>() else {
                continue;
            };
            match kind {
                "start" => events.push((nanos, 1)),
                "end" => events.push((nanos, -1)),
                _ => {}
            }
        }
        // Ends before starts at the same instant, so a window boundary is never
        // counted as overlap.
        events.sort();
        let (mut running, mut peak) = (0i32, 0i32);
        for (_, delta) in events {
            running += delta;
            peak = peak.max(running);
        }
        (peak.max(0) as usize, running.max(0) as usize)
    }

    fn records(&self, file: &str, separator: char) -> Vec<Vec<String>> {
        self.read_records(file)
            .iter()
            .map(|line| {
                line.split(separator)
                    .filter(|field| !field.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .collect()
    }

    fn read_records(&self, file: &str) -> Vec<String> {
        fs::read_to_string(self.dir.join(file))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Runs `body` with a harness, and fails with the recorded invocations attached
/// when it panics — the fastest way to see how ssh was actually invoked.
pub fn with_harness(body: impl FnOnce(&Harness)) {
    let harness = Harness::new();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(&harness)));
    if result.is_err() {
        eprintln!("--- fake ssh invocations ---");
        for argv in harness.invocations() {
            eprintln!("{argv:?}");
        }
        eprintln!("--- timeline ---");
        for line in harness.read_records("timeline") {
            eprintln!("{line}");
        }
        eprintln!("--- harness dir {} ---", harness.path().display());
        std::mem::forget(harness);
        panic!("test body failed");
    }
}

/// A pseudo-terminal, for testing the report on a stream that really is a
/// terminal: colour and the heartbeat are invisible on a pipe.
pub struct Tty {
    master: std::fs::File,
    slave: std::fs::File,
}

impl Tty {
    /// Opens a pty. `stdio` hands the child an fd on the terminal end; the
    /// parent reads what the child wrote from the master end.
    pub fn new() -> Tty {
        let mut master = 0;
        let mut slave = 0;
        // SAFETY: openpty writes two owned fds into the pointers it is given.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        assert_eq!(rc, 0, "openpty failed");
        // openpty does not set close-on-exec, and the harness must not leak a
        // terminal into the fake ssh it spawns.
        for fd in [master, slave] {
            // SAFETY: fd is open, so fcntl only reads and writes its flags.
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
        // SAFETY: both fds came from openpty and are owned by this process.
        unsafe {
            Tty {
                master: std::fs::File::from_raw_fd(master),
                slave: std::fs::File::from_raw_fd(slave),
            }
        }
    }

    /// A `Stdio` attached to the terminal, for one of the child's streams.
    /// Each call duplicates the terminal fd, so several streams can share it.
    pub fn stdio(&self) -> std::process::Stdio {
        std::process::Stdio::from(self.slave.try_clone().expect("clone the pty"))
    }

    /// Reads everything the child wrote to the terminal, until every copy of
    /// the terminal fd is closed.
    pub fn read_to_end(&mut self) -> String {
        use std::io::Read;
        let mut text = String::new();
        let _ = self.master.read_to_string(&mut text);
        text
    }

    /// Takes the master end, for reading on another thread. Dropping the slave
    /// is what lets the read finish once the child exits.
    pub fn into_master(self) -> std::fs::File {
        drop(self.slave);
        self.master
    }
}

/// What a terminal shows once every byte has been written: the visible text
/// with carriage returns, overwrites and line erases applied. Only the escape
/// sequences the report emits are understood — erase-in-line, and SGR.
pub fn screen(bytes: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut line: Vec<char> = Vec::new();
    let mut cursor = 0usize;
    let mut chars = bytes.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => cursor = 0,
            '\n' => {
                lines.push(line.iter().collect::<String>().trim_end().to_string());
                line.clear();
                cursor = 0;
            }
            '\u{1b}' => {
                if chars.peek() != Some(&'[') {
                    continue;
                }
                chars.next();
                let mut params = String::new();
                while let Some(&next) = chars.peek() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                    params.push(next);
                    chars.next();
                }
                match chars.next() {
                    // Erase in line: `2` clears the whole line, the default
                    // clears from the cursor onwards.
                    Some('K') if params == "2" => {
                        line.clear();
                        cursor = 0;
                    }
                    Some('K') => line.truncate(cursor),
                    // Everything else is styling or a cursor move the report
                    // does not rely on.
                    _ => {}
                }
            }
            _ => {
                // Overwrite at the cursor, the way a terminal does.
                if cursor < line.len() {
                    line[cursor] = c;
                } else {
                    line.push(c);
                }
                cursor += 1;
            }
        }
    }
    if !line.is_empty() {
        lines.push(line.iter().collect::<String>().trim_end().to_string());
    }
    lines.join("\n")
}

/// One fake ssh process, as the harness observed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Process {
    pub pid: i32,
    /// The process group the child ran in. Equal to `pid` when rshx gave the
    /// child a group of its own.
    pub pgid: i32,
    /// The session the child ran in. Equal to rshx's own session, since rshx
    /// does not call `setsid`.
    pub sid: i32,
}

/// One fake ssh start or end, as the harness observed it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Tick {
    /// Nanoseconds since the epoch, on the same clock as `SystemTime::now`.
    pub at: u128,
    /// True for a start, false for an end. At the same instant an end sorts
    /// first, so a window boundary is never counted as overlap.
    pub started: bool,
    pub dest: String,
}

/// A summary of the run a test just performed.
#[derive(Debug)]
pub struct Run {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    pub fn stdout_lines(&self) -> Vec<&str> {
        self.stdout.lines().collect()
    }
}

/// Runs a command to completion and captures everything.
pub fn run(mut cmd: Command) -> Run {
    let captured = output(&mut cmd);
    Run {
        code: captured.status.code().expect("process exited normally"),
        stdout: String::from_utf8_lossy(&captured.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&captured.stderr).into_owned(),
    }
}

/// Which of the child's streams get a terminal, and which get a pipe.
#[derive(Debug, Clone, Copy)]
pub struct Attach {
    pub stdout: bool,
    pub stderr: bool,
}

impl Attach {
    pub const NONE: Attach = Attach {
        stdout: false,
        stderr: false,
    };
    pub const BOTH: Attach = Attach {
        stdout: true,
        stderr: true,
    };
    /// stdout piped, stderr on a terminal.
    pub const STDERR_ONLY: Attach = Attach {
        stdout: false,
        stderr: true,
    };
    /// stdout on a terminal, stderr piped.
    pub const STDOUT_ONLY: Attach = Attach {
        stdout: true,
        stderr: false,
    };
}

/// Runs a command with the chosen streams attached to a terminal; every other
/// stream is captured in a pipe, so no output leaks into the harness's own.
pub fn run_on_tty(mut cmd: Command, attach: Attach) -> Run {
    let tty = Tty::new();
    if attach.stdout {
        cmd.stdout(tty.stdio());
    } else {
        cmd.stdout(std::process::Stdio::piped());
    }
    if attach.stderr {
        cmd.stderr(tty.stdio());
    } else {
        cmd.stderr(std::process::Stdio::piped());
    }
    // The Command holds the parent's copies of the terminal fd, and the read
    // only ends once every copy is closed, so it must be dropped here.
    let mut child = spawn(&mut cmd);
    drop(cmd);

    // The terminal is drained on its own thread: reading it to EOF needs every
    // copy of the fd closed, which only happens once the child exits, and
    // waiting first would deadlock as soon as it filled the pty buffer.
    let mut master = tty.into_master();
    let reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut text = String::new();
        let _ = master.read_to_string(&mut text);
        text
    });

    // The pipes are drained before waiting, for the same reason.
    let piped_stdout = read_pipe(child.stdout.take());
    let piped_stderr = read_pipe(child.stderr.take());
    let code = child.wait().expect("wait").code().expect("exited normally");
    let terminal = reader.join().expect("pty reader");

    Run {
        code,
        stdout: if attach.stdout {
            terminal.clone()
        } else {
            piped_stdout
        },
        stderr: if attach.stderr {
            terminal
        } else {
            piped_stderr
        },
    }
}

fn read_pipe<R: std::io::Read>(pipe: Option<R>) -> String {
    let Some(mut pipe) = pipe else {
        return String::new();
    };
    let mut text = String::new();
    let _ = pipe.read_to_string(&mut text);
    text
}

/// One answer to a terminal prompt: what to type, and how long to wait after
/// the prompt appears before typing it.
#[derive(Debug, Clone, Copy)]
pub struct Typed<'a> {
    pub text: &'a str,
    pub after_ms: u64,
}

impl<'a> Typed<'a> {
    /// Typed the moment the prompt appears.
    pub fn now(text: &'a str) -> Typed<'a> {
        Typed { text, after_ms: 0 }
    }

    /// Typed `ms` after the prompt appears, for what a slow typist does.
    pub fn after(text: &'a str, ms: u64) -> Typed<'a> {
        Typed { text, after_ms: ms }
    }
}

/// The text of every password prompt rshx writes. A test waits for this before
/// typing, the way a person does — and must: rshx flushes the terminal's input
/// when it starts reading, so a password typed before the prompt is discarded.
const PROMPT: &str = "privilege password";

/// Runs a command on a terminal of its own, answering its password prompts.
///
/// The command gets a session and a controlling terminal of its own, with the
/// terminal on stdin and stderr — the shape an interactive run has. stdout
/// stays a pipe, so the report can be read as text rather than off a screen.
/// Each answer is typed after the next prompt appears, so `answers[i]` answers
/// the `i`-th prompt.
pub fn run_on_tty_answering(mut cmd: Command, answers: &[Typed]) -> Run {
    let tty = Tty::new();
    cmd.stdin(tty.stdio()).stderr(tty.stdio());
    // Every stream is set explicitly, so none inherits the harness's own.
    cmd.stdout(std::process::Stdio::piped());
    controlling_terminal(&mut cmd);

    let mut child = spawn(&mut cmd);
    drop(cmd);

    let mut master = tty.into_master();
    let mut writer = master.try_clone().expect("clone the pty");
    let (chunks, arrivals) = std::sync::mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut all = String::new();
        let mut buf = [0u8; 4096];
        loop {
            match master.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = String::from_utf8_lossy(&buf[..n]).into_owned();
                    all.push_str(&chunk);
                    if chunks.send(chunk).is_err() {
                        break;
                    }
                }
            }
        }
        all
    });

    let mut seen = String::new();
    let mut answered = 0;
    while answered < answers.len() {
        // Bounded, so a prompt that never comes fails the test rather than
        // hanging it. Long enough for a deliberately slow answer.
        let Ok(chunk) = arrivals.recv_timeout(Duration::from_secs(30)) else {
            break;
        };
        seen.push_str(&chunk);
        if seen.matches(PROMPT).count() > answered {
            let answer = answers[answered];
            if answer.after_ms > 0 {
                std::thread::sleep(ms(answer.after_ms));
            }
            use std::io::Write;
            let _ = writer.write_all(answer.text.as_bytes());
            let _ = writer.write_all(b"\n");
            let _ = writer.flush();
            answered += 1;
        }
    }
    // Drained to the end, so the child is never blocked on a full pty buffer.
    while let Ok(chunk) = arrivals.recv_timeout(Duration::from_secs(30)) {
        seen.push_str(&chunk);
    }

    let piped_stdout = read_pipe(child.stdout.take());
    let code = child.wait().expect("wait").code().expect("exited normally");
    let terminal = reader.join().expect("pty reader");

    Run {
        code,
        stdout: piped_stdout,
        stderr: terminal,
    }
}

/// Runs a command with no terminal at all: a session of its own, and nothing
/// that could be a controlling terminal — the shape a run has from a script or
/// a CI job, where there is no `/dev/tty` to ask on. A child left in the
/// harness's session would inherit whatever terminal the test runs under.
pub fn run_detached(mut cmd: Command) -> Run {
    cmd.stdin(std::process::Stdio::null());
    // `setsid` alone leaves no controlling terminal, so `/dev/tty` fails.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    run(cmd)
}

/// Gives the child a session and `tty` as its controlling terminal.
fn controlling_terminal(cmd: &mut Command) {
    // SAFETY: `setsid` and `TIOCSCTTY` are async-signal-safe, which is what a
    // `pre_exec` closure is allowed to call.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // fd 0 is the terminal this child was handed; a pty without
            // `TIOCSCTTY` is not a controlling terminal, so `/dev/tty` fails.
            if libc::ioctl(0, libc::TIOCSCTTY, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

pub fn ms(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

/// Wall-clock time for a closure, for assertions about how long a run took.
pub fn timed<T>(body: impl FnOnce() -> T) -> (T, Duration) {
    let start = Instant::now();
    let value = body();
    (value, start.elapsed())
}

/// The fake ssh: records what it was asked, then plays its scripted response.
const STUB: &str = r#"#!/bin/sh
# Fake ssh for rshx's integration tests. See tests/support/mod.rs.
dir="$RSHX_STUB_DIR"

# The argv of this invocation: one line, fields separated by US (0x1f).
# Built as one string and written once. A printf per field would be a write
# per field, and concurrent invocations would then interleave inside a record:
# each write appends atomically, but the record as a whole would not be.
us=$(printf '\037')
line=
for arg in "$@"; do line="$line$arg$us"; done
printf '%s\n' "$line" >> "$dir/argv"

# The destination is the first argument after `--`, as ssh itself parses it.
while [ "$#" -gt 0 ]; do
    if [ "$1" = "--" ]; then shift; break; fi
    shift
done
dest="${1:-}"

resp="$dir/resp/$dest"
[ -d "$resp" ] || resp="$dir/resp/default"

# A destination may script one response per invocation, for the commands rshx
# sends it twice: `resp/<dest>/seq/<n>` answers the n-th, and the last answers
# every one after it. The count is a file per destination, written before the
# response is read, so two invocations never share an answer.
if [ -d "$resp/seq" ]; then
    mkdir -p "$dir/count"
    n=$(cat "$dir/count/$dest" 2>/dev/null) || n=0
    n=$((n + 1))
    printf '%s\n' "$n" > "$dir/count/$dest"
    last=$(ls "$resp/seq" | sort -n | tail -n 1)
    [ "$n" -le "$last" ] || n="$last"
    resp="$resp/seq/$n"
fi

delay=$(cat "$resp/delay" 2>/dev/null) || delay=0
code=$(cat "$resp/code" 2>/dev/null) || code=0

# When the response asks for it, keep every byte that arrives on stdin: rshx
# sends a script this way. The prompt below reads stdin a line at a time, so no
# response asks for both.
if [ -f "$resp/capture_stdin" ]; then
    mkdir -p "$dir/stdin_raw"
    cat > "$dir/stdin_raw/$dest"
fi

# When this invocation ran, and for which destination. One write per event, so
# concurrent invocations never corrupt each other's records. The destination
# lets a test assert on the order Hosts ran in, which is a structural property:
# unlike a wall-clock bound, it does not bend when the machine is loaded.
printf 'start %s %s\n' "$(date +%s%N)" "$dest" >> "$dir/timeline"
# The pid, its process group and its session, so a test can tell whether rshx
# gave the child a group of its own.
printf 'proc %s %s %s\n' "$$" "$(ps -o pgid= -p $$ | tr -d ' ')" "$(ps -o sid= -p $$ | tr -d ' ')" >> "$dir/procs"

# When the response asks for one, do what `sudo -S` does: print the prompt rshx
# set with `-p` to stderr, read the password from stdin, and retry a few times
# before giving up. The prompt comes from rshx's own argument, so a run that
# failed to set one prompts nothing and this reads nothing.
if [ -f "$resp/prompt" ]; then
    prompt=
    prev=
    for arg in "$@"; do
        if [ "$prev" = "-p" ]; then prompt="$arg"; break; fi
        case "$arg" in
            -p?*) prompt=${arg#-p}; break ;;
        esac
        prev="$arg"
    done
    want=$(cat "$resp/password" 2>/dev/null) || want=
    tries=0
    matched=
    while [ "$tries" -lt 3 ]; do
        tries=$((tries + 1))
        printf '%s' "$prompt" >&2
        typed=
        IFS= read -r typed || break
        printf '%s %s\n' "$dest" "$typed" >> "$dir/stdin"
        if [ "$typed" = "$want" ]; then matched=1; break; fi
        printf 'Sorry, try again.\n' >&2
    done
    if [ -z "$matched" ]; then
        printf 'sudo: %s incorrect password attempts\n' "$tries" >&2
        code=1
    fi
fi

if [ "$delay" != "0" ]; then sleep "$delay"; fi
[ -f "$resp/out" ] && cat "$resp/out"
[ -f "$resp/err" ] && cat "$resp/err" >&2
printf 'end %s %s\n' "$(date +%s%N)" "$dest" >> "$dir/timeline"

exit "$code"
"#;
