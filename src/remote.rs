//! The remote side of a run: the ssh command for a Host, the marker that names
//! the command rshx starts there, stopping that command again, and the launcher
//! that starts one without waiting for it.
//!
//! Killing a Host's ssh does not stop what it started. The remote command
//! belongs to the sshd session, not to the connection: it outlives the ssh
//! that asked for it, and carries on with nobody watching. So rshx names the
//! command before it runs — the pid of the shell that runs it, in a file of
//! rshx's own on the Host — and a second connection reads that file and stops
//! the tree below it. The second connection is best-effort: a Host rshx cannot
//! reach again is reported as one whose command may still be running.
//!
//! A detached run leaves that same marker and nothing else: rshx starts the
//! command, is told its pid, and returns. What it started is on its own from
//! there, and the marker is gone when the command ends.

use std::process::Stdio;
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::host::Host;
use crate::interrupt;
use crate::run::stream::read_capped;

/// How long a stop connection has to connect, authenticate, and stop a Host's
/// command. Past it the Host is reported as one rshx could not stop: a run
/// already cut short must not hang on a Host that cannot be reached again.
const STOP_LIMIT: Duration = Duration::from_secs(3);

/// What the stop connection keeps of ssh's stderr, which is where the reason
/// a stop failed comes from.
const STOP_STDERR_CAP: usize = 8 * 1024;

/// The ssh command for one Host, with `extra` `-o` options of rshx's own.
///
/// A Host's overrides become `-o` options rather than a rewritten destination,
/// so `~/.ssh/config` stays the single source and the rest still applies.
pub fn ssh(host: &Host, extra: &[(&str, &str)]) -> Command {
    let mut child = Command::new("ssh");
    if let Some(user) = &host.user {
        child.arg("-o").arg(format!("User={user}"));
    }
    if let Some(port) = host.port {
        child.arg("-o").arg(format!("Port={port}"));
    }
    if let Some(ip) = host.ip {
        child.arg("-o").arg(format!("HostName={ip}"));
    }
    for (option, value) in extra {
        child.arg("-o").arg(format!("{option}={value}"));
    }
    // Forwarded verbatim as ssh arguments, since ssh does its own joining.
    // `--` ends option parsing, so a destination is never read as an option.
    child.arg("--").arg(&host.name);
    child
}

/// This run's token, mixed from the pid and the clock so that two runs on the
/// same machine never share one — a marker an earlier run left behind must
/// never be read as this run's.
static RUN_TOKEN: LazyLock<u64> = LazyLock::new(|| {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos());
    (u64::from(std::process::id()) << 32) | u64::from(nanos)
});

/// Where a Host's marker goes: a name no other run can pick, and one a Host's
/// name cannot steer, since it holds only letters, digits, `.`, `_` and `-`.
pub fn marker(host: &str) -> String {
    format!("/tmp/rshx-{:x}-{host}.pid", *RUN_TOKEN)
}

/// A Host's command, wrapped in the marker a stop finds it by.
///
/// ssh hands a command to the Host's *login shell*, which is whatever that
/// Host's user runs — a POSIX sh, bash, fish, csh. rshx cannot know which, so
/// the wrapper is not written in the login shell's language: the login shell
/// only has to parse `sh -c` and the one quoted word after it, and the command
/// itself is parsed by that `sh`. A Host whose user has not changed their
/// shell would have had the same `sh` parse it anyway.
///
/// The pid written is that `sh`'s own, and the command runs in a subshell
/// below it: whatever the command does — exec, fork, a background job — it
/// stays in that shell's tree, and the tree is what a stop walks. Its exit
/// status is carried out through the subshell, and the marker is removed last,
/// so a stop that arrives as the command ends finds either a live pid or
/// nothing at all.
///
/// Returned as the one word ssh is handed. The write is silent, since a Host
/// whose `/tmp` cannot take it is not a Host the run has anything to say
/// about — the command still runs, it just cannot be stopped.
pub fn marked(marker: &str, command: &[String]) -> String {
    // What ssh would have joined the words into, parsed by a shell rshx names
    // rather than by whatever the Host's user happens to run.
    let text = command.join(" ");
    let script = format!(
        "m={marker}; echo $$ > \"$m\" 2>/dev/null; ( {text} ); rc=$?; rm -f -- \"$m\"; exit $rc"
    );
    one_word(&script)
}

/// A shell script as the one word ssh is handed: the script is quoted so that
/// it arrives whole, whatever the login shell that reads the line makes of
/// quotes. Every quote inside is closed and reopened.
fn one_word(script: &str) -> String {
    format!("sh -c '{}'", script.replace('\'', r"'\''"))
}

/// A Host's command, started and left running: ssh returns once the command has
/// started, instead of waiting for it to end.
///
/// A detached command is not rshx's to wait for any more, so nothing of its own
/// comes back over the ssh channel: the channel is what the Host's ssh waits
/// on, and a command still holding it would keep the connection open for as
/// long as it ran. Both its streams therefore go to `/dev/null`, and the run
/// keeps no record of it — the marker beside it is the only thing rshx leaves
/// on the Host, and the command's own end removes that.
///
/// What the launcher prints is the pid of the shell that runs the command — the
/// same shell the marker names, and the root of the tree a stop walks — so a
/// launch cut short is stopped by the same script as an attached run. `nohup`
/// and the redirects are what let the command outlive the connection: the
/// streams are no longer the channel's, and a session teardown that sends
/// `SIGHUP` cannot reach it. `setsid` would buy nothing more, since sshd
/// signals no group of a session it allocated no terminal for.
///
/// Under `privilege` the elevation sits *outside* the launcher, not inside the
/// payload: sudo's prompt has to reach rshx, and the payload's own streams are
/// discarded. The payload only starts once sudo has authenticated, so a Host
/// reported `running` is one whose command really is running as root.
pub fn detached(marker: &str, text: &str, privilege: bool) -> String {
    let payload = format!(
        "echo $$ > \"{marker}\" 2>/dev/null; ( {text} ); rc=$?; rm -f -- \"{marker}\"; exit $rc"
    );
    let launcher = format!(
        "nohup {} >/dev/null 2>&1 </dev/null & echo $!",
        one_word(&payload)
    );
    let launched = match privilege {
        // The launcher as one word, so the sudo elevates all of it rather than
        // the first command in it — and so the login shell never has to parse
        // the `;`, `&` and quotes the launcher is written in.
        true => crate::privilege::under_sudo(&[one_word(&launcher)]).join(" "),
        false => launcher,
    };
    one_word(&launched)
}

/// Stops the command a Host's ssh started, by the marker rshx wrote beside it.
///
/// `privilege` is whether the run elevated: a command started under `sudo`
/// belongs to root, and only root can signal it. `password` is the one the run
/// already has, if it has one — a stop must not ask for another, and with none
/// to give, only a sudo that wants none may run it. A Host with nothing to stop
/// is not a failure: the marker is gone exactly when the command has already
/// ended.
pub async fn stop(
    host: &Host,
    marker: &str,
    privilege: bool,
    password: Option<&[u8]>,
) -> Result<(), String> {
    let words = stop_argv(marker, privilege, password.is_some());
    // Nothing on this connection can prompt: the password, when there is one,
    // is written for the Host's sudo, and an ssh that wants one of its own has
    // to fail rather than read it.
    let mut child = ssh(host, &[("BatchMode", "yes")]);
    child.args(words);
    child.stdin(match password {
        Some(_) => Stdio::piped(),
        None => Stdio::null(),
    });
    child.stdout(Stdio::null());
    child.stderr(Stdio::piped());
    // Its own group, like every other child rshx spawns, so an interrupt
    // reaches rshx alone.
    child.process_group(0);

    let mut running = match child.spawn() {
        Ok(running) => running,
        Err(err) => return Err(format!("could not run ssh: {err}")),
    };
    let pid = running.id().map_or(0, |pid| pid as i32);
    if let (Some(password), Some(mut stdin)) = (password, running.stdin.take()) {
        // A newline ends it: sudo reads a line. A write that fails means the
        // connection is already gone, which its exit status will say.
        let _ = stdin.write_all(password).await;
        let _ = stdin.write_all(b"\n").await;
    }
    let stderr = running.stderr.take().expect("stderr was piped");
    let stderr = tokio::spawn(read_capped(stderr, STOP_STDERR_CAP));

    let (waited, timed_out) =
        interrupt::wait_bounded(&mut running, pid, Some(STOP_LIMIT), &host.name, None).await;
    let (stderr, _) = stderr.await.unwrap_or_default();
    if timed_out {
        return Err("the stop connection did not finish in time".into());
    }
    match waited {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(reason(&stderr, status.code())),
        Err(err) => Err(format!(
            "could not read the stop connection's status: {err}"
        )),
    }
}

/// The words a Host runs to stop its own command: `sh -c` and the script, with
/// the marker as the script's `$0`, which `sh -c` passes without rshx quoting
/// it into the text.
fn stop_argv(marker: &str, privilege: bool, password: bool) -> Vec<String> {
    let words = vec![
        "sh".to_string(),
        "-c".to_string(),
        // One shell word, so the sudo in front of it elevates all of it.
        format!("'{STOP}'"),
        marker.to_string(),
    ];
    match privilege {
        // A password the run already has is written to this connection's stdin;
        // with none to give, `-n` succeeds exactly when the Host's sudo wants
        // none. The prompt is empty: nothing here is recognised, so nothing of
        // rshx's own is printed into the reason a stop failed.
        true => crate::privilege::under_sudo_unprompted(&words, password),
        false => words,
    }
}

/// Why a connection failed, from what the Host's ssh printed. ssh's own status
/// is what rshx reports, so a stop ssh could not make reads as one rshx could
/// not make.
fn reason(stderr: &[u8], code: Option<i32>) -> String {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    if !text.is_empty() {
        return text.to_string();
    }
    match code {
        Some(code) => format!("ssh exited with status {code}"),
        None => "the connection was killed".into(),
    }
}

/// The remote side of a stop: read the pid rshx recorded for the Host's
/// command, and stop everything below it.
///
/// The tree is walked rather than a process group signalled: a command under
/// `sudo` may sit in a session of its own, and no group of rshx's naming can
/// be assumed on the far side. `ps` and `awk` are what every remote has;
/// without them the recorded pid is all that is left to signal.
///
/// A marker that is gone — the command finished, or never started — is not a
/// failure: there is nothing to stop. It is removed first, so a stop that
/// races the command's own end reads the marker once. Reading a missing marker
/// as nothing to stop is only safe because of an ordering neither side can
/// break: the wrapper writes the marker before the command runs, and this
/// connection has to connect, authenticate and start a shell before it can
/// read — strictly more work than the wrapper's first statement, which began
/// as soon as the Host forked it. A missing marker therefore means the Host
/// never got as far as forking the wrapper, or the wrapper has already removed
/// it, and in both cases nothing rshx started is still running.
///
/// The script is handed to `sh -c` as one word quoted in single quotes, so it
/// holds none of its own: `awk`'s `$1` and `$2` are escaped for the `sh` that
/// parses this text, which is what keeps them out of its hands.
const STOP: &str = r#"p=$(cat "$0" 2>/dev/null) || exit 0
rm -f -- "$0"
[ -n "$p" ] || exit 0
case $p in *[!0-9]*) exit 0;; esac
list=$(ps -A -o pid=,ppid= 2>/dev/null | awk -v root="$p" "{ par[\$1] = \$2 }
END {
    want[root] = 1; out = root
    do {
        n = 0
        for (pid in par) if (!(pid in want) && (par[pid] in want)) { want[pid] = 1; out = out OFS pid; n++ }
    } while (n)
    print out
}")
[ -n "$list" ] || list=$p
kill -TERM $list 2>/dev/null
sleep 1
kill -KILL $list 2>/dev/null
exit 0
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::path::{Path, PathBuf};
    use std::process::Command as Std;

    /// A directory of this test's own, removed when it ends.
    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Dir {
            let dir =
                std::env::temp_dir().join(format!("rshx-remote-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create the test directory");
            Dir(dir)
        }

        fn file(&self, name: &str) -> String {
            self.0.join(name).display().to_string()
        }

        /// The one file a run leaves on a Host, as a Host's would be named.
        fn marker(&self) -> String {
            self.file("run.pid")
        }

        /// Every file in here, by name.
        fn files(&self) -> Vec<String> {
            let mut files: Vec<String> = std::fs::read_dir(&self.0)
                .expect("read the directory")
                .map(|entry| {
                    entry
                        .expect("an entry")
                        .file_name()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect();
            files.sort();
            files
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn words(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    /// A remote command line, run the way sshd runs one: handed to a shell as
    /// the login shell would hand it over.
    fn shell(line: &str) -> Std {
        let mut cmd = Std::new("sh");
        cmd.arg("-c").arg(line);
        cmd
    }

    /// Runs the stop script the way the second connection does, with the
    /// marker as the script's `$0`.
    fn stop(marker: &str) -> std::process::Output {
        Std::new("sh")
            .arg("-c")
            .arg(STOP)
            .arg(marker)
            .output()
            .expect("run the stop script")
    }

    /// The pid the marker names, once it names one.
    fn recorded_pid(marker: &str) -> i32 {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if let Ok(text) = std::fs::read_to_string(marker)
                && let Ok(pid) = text.trim().parse()
            {
                return pid;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("{marker} never named a pid");
    }

    /// Waits for something a Host does on its own clock: a launch that has
    /// already returned cannot be asked whether the command got there.
    fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if ready() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("{what} never happened");
    }

    /// Every process below `root`, as the stop script finds them.
    fn descendants(root: i32) -> Vec<i32> {
        let out = Std::new("ps")
            .args(["-A", "-o", "pid=,ppid="])
            .output()
            .expect("ps");
        let text = String::from_utf8_lossy(&out.stdout);
        let pairs: Vec<(i32, i32)> = text
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
            })
            .collect();
        let mut tree = vec![root];
        loop {
            let mut grew = false;
            for (pid, parent) in &pairs {
                if tree.contains(parent) && !tree.contains(pid) {
                    tree.push(*pid);
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }
        tree
    }

    /// Whether a pid is no longer running. One that has exited but not been
    /// reaped is not running either, which is what a Host's init is for.
    fn gone(pid: i32) -> bool {
        let Ok(out) = Std::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
        else {
            return true;
        };
        if !out.status.success() {
            return true;
        }
        String::from_utf8_lossy(&out.stdout).trim().starts_with('Z')
    }

    #[test]
    fn the_stop_script_is_one_shell_word() {
        assert!(
            !STOP.contains('\''),
            "the script is handed over quoted in single quotes, so it holds none"
        );
    }

    #[test]
    fn a_stop_hands_over_the_script_and_the_marker() {
        let plain = stop_argv("/tmp/m.pid", false, false);
        assert_eq!(
            plain,
            words(&["sh", "-c", &format!("'{STOP}'"), "/tmp/m.pid"]),
            "a stop runs as the Host's user unless the run elevated"
        );

        // Under `--privilege` the sudo is in front of the whole script, and the
        // marker rides behind it as `sh -c`'s `$0`.
        let given = stop_argv("/tmp/m.pid", true, true);
        assert_eq!(
            given[..4],
            ["sudo", "-S", "-p", ""],
            "a stop with the run's password writes it, and prompts for nothing: {given:?}"
        );
        assert_eq!(
            given[4..],
            ["--", "sh", "-c", &format!("'{STOP}'"), "/tmp/m.pid"],
            "and the script is still one word: {given:?}"
        );
        let asked = stop_argv("/tmp/m.pid", true, false);
        assert_eq!(
            asked[..3],
            ["sudo", "-n", "--"],
            "with no password in hand, only a sudo that wants none may run it: {asked:?}"
        );
    }

    #[test]
    fn a_marker_is_one_path_a_shell_cannot_read_into() {
        // The marker is written into the payload and into the stop script as
        // text, so it holds nothing either shell could act on.
        let one = marker("node01");
        assert!(
            one.starts_with("/tmp/rshx-") && one.ends_with("-node01.pid"),
            "the run and the Host it is on: {one}"
        );
        assert!(
            one.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-')),
            "and nothing a shell reads: {one}"
        );
        assert_ne!(one, marker("node02"), "one marker per Host");
    }

    #[test]
    fn a_marked_command_is_one_quoted_word_for_the_login_shell() {
        // The Host's login shell is whatever its user runs — fish, csh, bash —
        // so rshx hands it `sh -c` and one word, quoted so that word arrives
        // whole however that shell reads quotes.
        let line = marked("/tmp/m.pid", &words(&["echo", "it's", "mine"]));
        let quoted = line
            .strip_prefix("sh -c '")
            .unwrap_or_else(|| panic!("sh -c and a quoted word: {line}"))
            .strip_suffix('\'')
            .unwrap_or_else(|| panic!("the word ends: {line}"));
        assert!(
            !quoted.replace(r"'\''", "").contains('\''),
            "every quote the command brought is escaped, so none of them closes \
             the word early: {line}"
        );
    }

    #[test]
    fn a_command_with_quotes_of_its_own_reaches_the_shell_whole() {
        let dir = Dir::new("quoting");
        let marker = dir.marker();
        // What `run -- sh -c 'echo hi'` produces: words that quote themselves,
        // which the login shell must hand over untouched.
        let line = marked(&marker, &words(&["sh", "-c", "'echo hi'"]));

        let out = shell(&line).output().expect("run the marked command");

        assert_eq!(
            out.stdout, b"hi\n",
            "the quoted command reached sh as it was written: {line}"
        );
        assert!(!Path::new(&marker).exists(), "and the marker is gone");
    }

    #[test]
    fn a_marked_command_keeps_its_own_status_and_output() {
        let dir = Dir::new("status");
        let marker = dir.marker();
        let line = marked(&marker, &words(&["sh", "-c", "'echo hi; exit 7'"]));

        let out = shell(&line).output().expect("run the marked command");

        assert_eq!(
            out.status.code(),
            Some(7),
            "the command's status is the Host's"
        );
        assert_eq!(out.stdout, b"hi\n", "and its output is its own");
        assert!(
            !Path::new(&marker).exists(),
            "the marker goes with the command"
        );
    }

    #[test]
    fn a_stop_takes_the_commands_children_with_it() {
        let dir = Dir::new("tree");
        let marker = dir.marker();
        // Two commands, so the shell does not exec the last of them: what the
        // marker names is a shell with a child of its own, which is the shape
        // a stop has to walk.
        let line = marked(&marker, &words(&["sh", "-c", "'sleep 300; sleep 300'"]));
        let mut running = shell(&line).spawn().expect("start the marked command");
        let root = recorded_pid(&marker);
        let tree = descendants(root);
        assert!(
            tree.len() >= 3,
            "the command is a tree, not one process: {tree:?}"
        );

        let out = stop(&marker);

        assert!(out.status.success(), "{:?}", out.stderr);
        assert!(
            !Path::new(&marker).exists(),
            "the stop removes the marker it read"
        );
        let status = running.wait().expect("the marked command ends");
        assert!(
            status.signal().is_some(),
            "the recorded shell was signalled, not left running: {status:?}"
        );
        for pid in &tree[1..] {
            assert!(gone(*pid), "pid {pid} outlived the stop");
        }
    }

    #[test]
    fn a_stop_takes_what_a_detached_launch_started() {
        let dir = Dir::new("detached-stop");
        let marker = dir.marker();
        let line = detached(&marker, "sleep 300", false);
        shell(&line).output().expect("run the detached launch");
        let pid = recorded_pid(&marker);

        let out = stop(&marker);

        assert!(out.status.success(), "{:?}", out.stderr);
        assert!(gone(pid), "the command a detached launch started is gone");
        assert!(!Path::new(&marker).exists(), "and the marker with it");
    }

    #[test]
    fn a_stop_with_nothing_to_stop_is_not_a_failure() {
        let dir = Dir::new("missing");
        let marker = dir.marker();
        let out = stop(&marker);
        assert!(
            out.status.success(),
            "a run with no marker at all is nothing to stop: {:?}",
            out.stderr
        );

        // And one that holds something that is not a pid is not one either.
        std::fs::write(&marker, "not a pid\n").expect("write the marker");
        let out = stop(&marker);

        assert!(out.status.success(), "{:?}", out.stderr);
        assert!(
            !Path::new(&marker).exists(),
            "a marker with no pid in it names nothing, and goes"
        );
    }

    #[test]
    fn a_detached_command_runs_on_with_nothing_left_beside_it() {
        let dir = Dir::new("detached");
        let marker = dir.marker();
        let ran = dir.file("ran");
        // The command's own output has nowhere to go: the connection that
        // started it has already returned.
        let line = detached(
            &marker,
            &format!("echo hi; echo bad >&2; touch {ran}; exit 7"),
            false,
        );

        let out = shell(&line).output().expect("run the detached launch");

        let launched: i32 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("the launcher prints the pid it started");
        assert!(launched > 0, "and the pid is the shell's: {launched}");
        wait_until("the command to run", || Path::new(&ran).exists());
        // The marker is written while the command runs and removed when it
        // ends: a command that fast leaves nothing at all behind.
        wait_until("the marker to go", || !Path::new(&marker).exists());
        assert_eq!(
            dir.files(),
            ["ran"],
            "the run leaves the command's own doings, and no record of its own"
        );
    }

    #[test]
    fn a_detached_launch_reports_a_pid_while_the_command_is_still_running() {
        let dir = Dir::new("detached-alive");
        let marker = dir.marker();
        let line = detached(&marker, "sleep 300", false);

        let out = shell(&line).output().expect("run the detached launch");

        let launched: i32 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("the launcher prints the pid it started");
        assert_eq!(
            recorded_pid(&marker),
            launched,
            "the marker names the shell the launch reported"
        );
        assert!(!gone(launched), "which is still running");
        stop(&marker);
    }

    #[test]
    fn a_detached_launch_under_privilege_elevates_the_launcher_alone() {
        let dir = Dir::new("privileged");
        let marker = dir.marker();
        let line = detached(&marker, "id -u", true);

        // The sudo is outside the launcher, so its prompt is written to the
        // connection — where rshx watches for it — rather than to the run's
        // own streams, which go nowhere.
        assert!(
            line.starts_with("sh -c 'sudo -S -p rshx-password: sh -c "),
            "the elevation is the launcher's: {line}"
        );
        // And only outside: the payload runs as root already, so a second
        // elevation would be rshx's own sudo asking again.
        let payload = line
            .split_once("nohup")
            .expect("the launcher backgrounds the payload")
            .1;
        assert!(
            !payload.contains("sudo"),
            "the payload holds no elevation of its own: {payload}"
        );
    }
}
