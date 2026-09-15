//! Test harness: a scripted stand-in for `ssh`.
//!
//! Every integration test runs rshx against a fake `ssh` placed first on
//! `PATH`. The fake records the argv of each invocation and the wall-clock
//! interval it occupied, and replays a response scripted per destination. That
//! makes the run's contract — how ssh is invoked, what the report says, what
//! the process exits with — checkable with no network and no sshd.

#![allow(dead_code)]

use std::fs;
use std::os::fd::FromRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

/// What the fake ssh does for one destination.
#[derive(Clone, Debug)]
pub struct Response {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    code: i32,
    delay_ms: u64,
}

impl Response {
    pub fn ok() -> Response {
        Response {
            stdout: Vec::new(),
            stderr: Vec::new(),
            code: 0,
            delay_ms: 0,
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
        fs::write(&stub, STUB).unwrap();
        let mut perms = fs::metadata(&stub).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        fs::set_permissions(&stub, perms).unwrap();

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

    /// Writes a file into the harness directory and returns its path.
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

    /// Scripts what the fake ssh does for a destination with no script of its own.
    pub fn respond_default(&self, response: Response) -> &Harness {
        self.script_response(&self.dir.join("resp").join("default"), response);
        self
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

/// A pseudo-terminal, for testing what the report does when a stream really is
/// a terminal. A captured pipe is not a terminal, so colour and the heartbeat
/// are invisible without one.
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
/// sequences the report actually emits are understood — erase-in-line, and
/// SGR, which changes nothing about the text. This answers "what does the user
/// end up looking at", which raw bytes cannot.
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

/// A summary of the run a test just performed, for assertions that read better
/// than raw bytes.
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
    let output = cmd.output().expect("spawn");
    Run {
        code: output.status.code().expect("process exited normally"),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
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

/// Runs a command with the chosen streams attached to a terminal. Every other
/// stream is captured in a pipe, never inherited, so a test cannot leak output
/// into the harness's own terminal.
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
    // The Command owns the parent's copies of the terminal fd, and the read
    // below only ends once every copy is closed. So the Command must be gone
    // before the read starts, not at the end of the function.
    let mut child = cmd.spawn().expect("spawn");
    drop(cmd);

    // The terminal is drained on its own thread: reading it to EOF needs every
    // copy of the fd closed, which only happens once the child exits, and
    // waiting for the child first would deadlock as soon as it filled the pty
    // buffer.
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

pub fn ms(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

/// Wall-clock time for a closure, for assertions about how long a run took.
pub fn timed<T>(body: impl FnOnce() -> T) -> (T, Duration) {
    let start = Instant::now();
    let value = body();
    (value, start.elapsed())
}

/// The fake ssh. It records what it was asked to do, then plays the response
/// scripted for its destination.
const STUB: &str = r#"#!/bin/sh
# Fake ssh for rshx's integration tests. See tests/support/mod.rs.
dir="$RSHX_STUB_DIR"

# The argv of this invocation: one line, fields separated by US (0x1f).
{
    for arg in "$@"; do printf '%s\037' "$arg"; done
    printf '\n'
} >> "$dir/argv"

# The destination is the first argument after `--`, as ssh itself parses it.
while [ "$#" -gt 0 ]; do
    if [ "$1" = "--" ]; then shift; break; fi
    shift
done
dest="${1:-}"

resp="$dir/resp/$dest"
[ -d "$resp" ] || resp="$dir/resp/default"

delay=$(cat "$resp/delay" 2>/dev/null) || delay=0
code=$(cat "$resp/code" 2>/dev/null) || code=0

printf 'start %s\n' "$(date +%s%N)" >> "$dir/timeline"
# The pid, its process group and its session, so a test can tell whether rshx
# gave the child a group of its own.
printf 'proc %s %s %s\n' "$$" "$(ps -o pgid= -p $$ | tr -d ' ')" "$(ps -o sid= -p $$ | tr -d ' ')" >> "$dir/procs"
if [ "$delay" != "0" ]; then sleep "$delay"; fi
[ -f "$resp/out" ] && cat "$resp/out"
[ -f "$resp/err" ] && cat "$resp/err" >&2
printf 'end %s\n' "$(date +%s%N)" >> "$dir/timeline"

exit "$code"
"#;
