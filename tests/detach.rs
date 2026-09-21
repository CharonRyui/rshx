//! `run --detach`: the command is started on each Host and left running there.
//!
//! A detached run keeps nothing of its own on a Host beyond the marker a stop
//! finds its command by: what it started is on its own from the moment rshx
//! returns, and what rshx reports is the pid of the shell running it.

mod support;

use support::{Response, Typed, run, run_on_tty_answering, with_harness};

const ONE: &str = "[[hosts]]\nname = \"node01\"\n";

/// What the fake ssh answers a launch with: the pid of the shell the command
/// runs in, which is the one line a detached run is told.
const PID: &str = "4321";

#[test]
fn a_detached_run_reports_the_pid_and_leaves_the_command_running() {
    with_harness(|harness| {
        harness.respond_default(Response::ok().stdout(PID));
        let file = harness.write("hosts.toml", ONE);

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args([
                "-H",
                file.to_str().unwrap(),
                "run",
                "--detach",
                "--",
                "sleep",
                "300",
            ]);
            cmd
        });

        assert_eq!(out.code, 0, "stderr: {}", out.stderr);
        let line = out.stdout_lines()[0];
        assert!(
            line.starts_with("node01 running "),
            "a command left running is its own status, not a failure: {line:?}"
        );
        assert!(line.ends_with(&format!("pid {PID}")), "{line:?}");
        assert!(
            !line.contains("sleep"),
            "the launcher's own output is plumbing, not the Host's: {line:?}"
        );
    });
}

#[test]
fn a_detached_run_keeps_nothing_of_its_own_on_the_host() {
    with_harness(|harness| {
        harness.respond_default(Response::ok().stdout(PID));
        let file = harness.write("hosts.toml", ONE);

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args([
                "-H",
                file.to_str().unwrap(),
                "run",
                "--detach",
                "--",
                "sleep",
                "300",
            ]);
            cmd
        });
        assert_eq!(out.code, 0, "{}", out.stderr);

        // The one word rshx handed the Host's ssh: the launcher, and nothing
        // else. What the command prints goes nowhere — the connection that
        // would have carried it has returned — so there is no log, no record
        // of how it ends, and nothing to read back later.
        let word = harness.word_for("node01");
        assert!(
            word.contains("nohup") && word.contains(">/dev/null 2>&1 </dev/null &"),
            "the command is let out of the connection, and its output is dropped: {word}"
        );
        assert!(
            word.contains("/tmp/rshx-") && word.contains("-node01.pid"),
            "and is named by the marker a stop finds it by: {word}"
        );
        assert!(
            word.contains("rm -f -- \"/tmp/rshx-"),
            "the marker goes when the command does: {word}"
        );
        for kept in [".out", ".state", "date +%s"] {
            assert!(
                !word.contains(kept),
                "nothing of the run is kept on the Host: {kept} in {word}"
            );
        }
    });
}

#[test]
fn a_detached_run_says_so_in_json_without_an_exit_status() {
    with_harness(|harness| {
        harness.respond_default(Response::ok().stdout(PID));
        let file = harness.write("hosts.toml", ONE);

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "--json"]);
            cmd.args(["run", "--detach", "--", "sleep", "300"]);
            cmd
        });

        let object: serde_json::Value =
            serde_json::from_str(out.stdout.trim()).expect("one JSON object");
        assert_eq!(object["status"], "running");
        assert_eq!(object["pid"], 4321);
        assert!(
            object.get("exit_code").is_none(),
            "the launcher's status is not the command's, which has not ended: {object}"
        );
    });
}

#[test]
fn a_launch_that_printed_no_pid_started_nothing_rshx_can_name() {
    with_harness(|harness| {
        // ssh succeeded, but nothing came back that names the command.
        harness.respond_default(Response::ok());
        let file = harness.write("hosts.toml", ONE);

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args([
                "-H",
                file.to_str().unwrap(),
                "run",
                "--detach",
                "--",
                "sleep",
                "300",
            ]);
            cmd
        });

        assert_ne!(out.code, 0, "{}", out.stdout);
        assert!(
            out.stdout.contains("unreachable"),
            "the Host never got as far as rshx asked: {:?}",
            out.stdout
        );
        assert!(
            out.stdout.contains("printed no pid"),
            "and rshx says why, under the Host's own line: {:?}",
            out.stdout
        );
    });
}

#[test]
fn a_detached_run_under_privilege_answers_its_own_password_prompt() {
    with_harness(|harness| {
        // The Host's sudo asks, the way it does under `--privilege`.
        harness.respond_default(Response::ok().prompt("hunter2").stdout(PID));
        let file = harness.write("hosts.toml", ONE);

        let out = run_on_tty_answering(
            {
                let mut cmd = harness.rshx();
                cmd.args(["-H", file.to_str().unwrap(), "--privilege"]);
                cmd.args(["run", "--detach", "--", "id", "-u"]);
                cmd
            },
            &[Typed::now("hunter2")],
        );

        assert_eq!(out.code, 0, "stderr: {}", out.stderr);
        assert!(
            out.stdout.contains("running") && out.stdout.contains(&format!("pid {PID}")),
            "the launch is reported like any other: {:?}",
            out.stdout
        );

        // The elevation is the launcher's, so sudo's prompt reaches the
        // connection rshx watches; the command inside runs as root already.
        let word = harness.word_for("node01");
        assert!(
            word.starts_with("sh -c 'sudo -S -p rshx-password: sh -c "),
            "the sudo is outside the launcher: {word}"
        );
        let payload = word.split_once("nohup").expect("the payload").1;
        assert!(
            !payload.contains("sudo"),
            "and the payload holds none of its own: {payload}"
        );
    });
}

#[test]
fn a_detached_launch_cut_short_is_stopped_on_the_host() {
    with_harness(|harness| {
        // The launch never answers, so rshx gives up on it.
        harness.respond_default(Response::ok().delay_ms(30_000));
        let file = harness.write("hosts.toml", ONE);

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "--timeout", "1s"]);
            cmd.args(["run", "--detach", "--", "sleep", "300"]);
            cmd
        });

        assert_eq!(
            out.code, 4,
            "a timeout is its own exit code: {}",
            out.stderr
        );
        let stops = harness.stops();
        assert_eq!(stops.len(), 1, "the Host is asked to stop it: {stops:?}");
        assert!(
            stops[0]
                .last()
                .is_some_and(|word| word.starts_with("/tmp/rshx-") && word.ends_with("-node01.pid")),
            "the marker names what the launcher started: {:?}",
            stops[0]
        );
    });
}

#[test]
fn a_detached_script_run_removes_its_own_copy() {
    with_harness(|harness| {
        harness.write("deploy.sh", "#!/bin/sh\necho deployed\n");
        // One destination, two connections: the copy answers with the path it
        // wrote the script to, and the launch then answers with a pid.
        harness.respond_sequence(
            "node01",
            &[
                Response::ok().stdout("/tmp/tmp.abc123"),
                Response::ok().stdout(PID),
            ],
        );
        let file = harness.write("hosts.toml", ONE);

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "run", "--detach", "--script"]);
            cmd.arg(harness.path().join("deploy.sh"));
            cmd
        });

        assert_eq!(out.code, 0, "stderr: {}", out.stderr);
        assert!(
            out.stdout.contains("running") && out.stdout.contains(&format!("pid {PID}")),
            "{:?}",
            out.stdout
        );
        // The launch returns long before the script ends, so only the script's
        // own end can remove the file it was copied to.
        let word = harness.word_for("node01");
        assert!(
            word.contains(r#"rm -f -- "/tmp/tmp.abc123""#),
            "the copy goes with the script, not with the launch: {word}"
        );
    });
}

#[test]
fn detach_rides_along_with_the_command_rather_than_replacing_it() {
    with_harness(|harness| {
        harness.respond_default(Response::ok().stdout(PID));
        let file = harness.write("hosts.toml", ONE);

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "run", "--detach"]);
            cmd
        });

        assert_eq!(
            out.code, 5,
            "a detached run still runs something: {}",
            out.stderr
        );
        assert!(!out.stderr.is_empty(), "and says so");
    });
}
