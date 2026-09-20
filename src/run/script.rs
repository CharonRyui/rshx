//! Running a local script on Hosts: the file is copied to a temporary file on
//! the Host, made executable, run there, and removed again.
use std::fs::{self, File};
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::{
    cli::CliOptions,
    host::Host,
    interrupt::Interrupt,
    privilege, remote,
    report::Reporter,
    run::{Outcome, Prompts, Status, execute_on_hosts, run_remote_command},
};

/// Runs a local script on every selected Host.
pub(super) async fn execute_local_script(
    selected: &Vec<&Host>,
    script: &Path,
    options: &CliOptions,
    reporter: &mut Reporter,
) -> Result<u8> {
    // The script is local, so one that cannot be read is the same failure on
    // every Host: it is reported once, before anything runs, as a local error
    // rather than as one failure per Host.
    let metadata =
        fs::metadata(script).with_context(|| format!("could not read {}", script.display()))?;
    if !metadata.is_file() {
        bail!("{} is not a file", script.display());
    }

    // Before the heartbeat's first draw, so the heading is not written over.
    // It names the script, which is what the run is about; the copy is
    // plumbing, and the same for every Host.
    reporter.heading(
        &[format!("{} (script)", script.display())],
        selected.len(),
        options.fanout,
    );

    execute_on_hosts(
        selected,
        options,
        reporter,
        async |host, interrupt, prompts| {
            run_script(host, script, options, interrupt, prompts).await
        },
    )
    .await
}

/// Copies the script to a temporary file on one Host, and runs it there.
async fn run_script(
    host: &Host,
    script: &Path,
    options: &CliOptions,
    interrupt: &Interrupt,
    prompts: Option<Prompts>,
) -> Outcome {
    let started = Instant::now();
    let copied = copy_to_host(host, script, options.timeout, interrupt).await;
    if copied.status != Status::Ok {
        return copied;
    }
    // What the Host printed is where the script landed. Without a path there
    // is nothing to run, and the Host's own output is the only clue as to why.
    let Some(path) = temporary_path(&copied.stdout) else {
        return unreadable_path(copied);
    };
    // `--timeout` bounds the Host, not one of its two steps: what the copy
    // spent is taken off what the run has left.
    let limit = options
        .timeout
        .map(|limit| limit.saturating_sub(started.elapsed()));
    run_on_host(host, &path, options.privilege, limit, interrupt, prompts).await
}

/// The remote side of the copy: a temporary file, the script written into it,
/// and the path of that file printed. `mktemp` failing leaves nothing behind,
/// and a `cat` that fails removes what it wrote. The path is printed without a
/// newline, so a Host's stdout is the path and nothing else.
const COPY: &str =
    r#"tmp=$(mktemp) || exit 1; cat > "$tmp" || { rm -f -- "$tmp"; exit 1; }; printf '%s' "$tmp""#;

/// Copies the script into a temporary file on one Host. The outcome's stdout is
/// that file's path; nothing runs yet.
async fn copy_to_host(
    host: &Host,
    script: &Path,
    limit: Option<Duration>,
    interrupt: &Interrupt,
) -> Outcome {
    let started = Instant::now();
    let file = match File::open(script) {
        Ok(file) => file,
        // Read before the run began, so this is a race with something that
        // changed the file under rshx, not the usual missing-script case.
        Err(err) => {
            return Outcome::local_failure(
                &host.name,
                started.elapsed(),
                format!("could not read {}: {err}", script.display()),
            );
        }
    };
    let marker = remote::marker(&host.name);
    let mut child = remote::ssh(host, &[]);
    child.arg(remote::marked(&marker, &[COPY.to_string()]));
    // The script is ssh's stdin, which carries it to the Host's `cat`. Nothing
    // on this side elevates, so no password is asked for here.
    child
        .stdin(Stdio::from(file))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    run_remote_command(child, host, interrupt, limit, None, Some(&marker)).await
}

/// The command a Host runs: the copied script made executable, run, and
/// removed whatever it exits with, with its own status kept. The removal sits
/// outside the elevated part, so a sudo that refuses a password still leaves
/// nothing behind — and it needs no privilege, since the file belongs to the
/// Host's user, who wrote it.
///
/// Returned as separate words, which ssh joins into the one string the remote
/// shell parses; the separator rides at the end of the word before it.
fn run_argv(path: &str, privilege: bool) -> Vec<String> {
    let chain = format!(r#"chmod +x -- "{path}" && "{path}""#);
    // One shell word, so `--privilege` puts a single sudo in front of all of
    // it. Left to the remote shell's own parsing, sudo would elevate the chmod
    // alone and the script would run as the Host's user.
    let mut command = vec!["sh".to_string(), "-c".to_string(), format!("'{}'", chain)];
    if privilege {
        command = privilege::under_sudo(&command);
    }
    // ssh joins these words into the one string the remote shell parses, so
    // the separator rides at the end of the word before the removal.
    command.last_mut().expect("the chain").push(';');
    command.push(format!(r#"rc=$?; rm -f -- "{path}"; exit $rc"#));
    command
}

/// Runs the copied script on one Host, as the Host's user or, under
/// `--privilege`, as root.
async fn run_on_host(
    host: &Host,
    path: &str,
    privilege: bool,
    limit: Option<Duration>,
    interrupt: &Interrupt,
    prompts: Option<Prompts>,
) -> Outcome {
    // The script runs under a marker, so that a Host cut short can be asked to
    // stop it: killing its ssh leaves the script running on the Host. The
    // cleanup that removes the file is part of the marked command, so a stop
    // that kills the script leaves nothing behind either.
    let marker = remote::marker(&host.name);
    let mut child = remote::ssh(host, &[]);
    child.arg(remote::marked(&marker, &run_argv(path, privilege)));
    // Without `--privilege`, stdin is null so ssh cannot stop to prompt with
    // nobody there to answer; with it, stdin carries the password to sudo.
    child.stdin(if prompts.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    child.stdout(Stdio::piped()).stderr(Stdio::piped());

    run_remote_command(child, host, interrupt, limit, prompts, Some(&marker)).await
}

/// The path a Host printed, if that is what it printed. The last line is the
/// one rshx asked for — a Host's ssh may print a banner of its own before it —
/// and it is an absolute path of characters that mean nothing to a shell,
/// since rshx builds a command out of it.
fn temporary_path(stdout: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(stdout).ok()?;
    let path = text.lines().rfind(|line| !line.is_empty())?;
    let plain = path.starts_with('/')
        && path
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-'));
    plain.then(|| path.to_string())
}

/// The copy's outcome, when what came back is not a path. ssh's exit status
/// stands — rshx does not invent one — but a Host rshx cannot name a file on
/// is one the script never ran on, which is what `unreachable` says. What the
/// Host printed is kept, so the report shows it.
fn unreadable_path(mut copied: Outcome) -> Outcome {
    let mut stderr =
        b"rshx: the Host printed no temporary path, so the script was not run\n".to_vec();
    stderr.extend_from_slice(&copied.stderr);
    copied.status = Status::Unreachable;
    copied.stderr = stderr;
    copied
}
