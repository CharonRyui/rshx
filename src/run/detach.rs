//! What a detached run does differently on rshx's side.
//!
//! The remote half of a detached run — the launcher, the marker, the token that
//! names the file rshx leaves on a Host — is `crate::remote`'s, because it is
//! shell text the Host runs. This module is the local half: which command goes
//! inside the launcher, and what the launcher's output means once it returns.

use crate::privilege;
use crate::run::outcome::{Outcome, Status};

/// The command each Host is handed.
///
/// `--privilege` puts rshx's sudo in front of it, so the remote sudo reads its
/// password from stdin. A detached run is the exception: its launcher carries
/// the elevation itself, and what it runs is the command as it was typed —
/// wrapping it here as well would elevate twice.
///
/// The command as it was typed is what decides this, because rshx reads none of
/// the command's own options: that would mean knowing sudo's grammar.
pub(super) fn command(typed: &[String], elevated: bool, detach: bool) -> Vec<String> {
    match (elevated, detach) {
        (true, false) => privilege::under_sudo(typed),
        _ => typed.to_vec(),
    }
}

/// The outcome of a detached launch: ssh's status stands, and what the launcher
/// printed is the pid of the shell running the command — the same shell the
/// marker names, so a stop of a Host cut short reaches what was started here.
///
/// The pid is plumbing, not the Host's output: it is taken out of the stdout
/// the report would otherwise show. A launch that printed no pid started
/// nothing rshx can name, which is what `unreachable` says — the Host never got
/// as far as rshx asked.
pub(super) fn launched(mut outcome: Outcome) -> Outcome {
    if outcome.status != Status::Ok {
        return outcome;
    }
    let text = String::from_utf8_lossy(&outcome.stdout);
    let pid = text
        .lines()
        .rfind(|line| !line.is_empty())
        .and_then(|line| line.trim().parse::<u32>().ok());
    match pid {
        Some(pid) => {
            outcome.status = Status::Running;
            outcome.pid = Some(pid);
            outcome.stdout.clear();
            // The status ssh ended with is the launcher's, not the command's:
            // the command has not ended, so it has none to report.
            outcome.exit_code = None;
        }
        None => {
            let mut stderr =
                b"rshx: the Host printed no pid, so the command was not started\n".to_vec();
            stderr.extend_from_slice(&outcome.stderr);
            outcome.status = Status::Unreachable;
            outcome.stderr = stderr;
        }
    }
    outcome
}
