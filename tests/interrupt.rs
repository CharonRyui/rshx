//! Interrupting a run.
//!
//! The interrupt is delivered the way a terminal delivers it: `SIGINT` to
//! rshx's process group. Because each ssh child is spawned in its own group,
//! only rshx receives it — which is the property the last test here pins.

mod support;

use std::io::{BufRead, BufReader};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use support::{Response, run, spawn, with_harness};

const THREE: &str = "[[hosts]]\nname = \"node01\"\n\n[[hosts]]\nname = \"node02\"\n\n[[hosts]]\nname = \"node03\"\n";

/// Starts a run whose stdout and stderr are pipes, so it can be interrupted
/// and then read.
///
/// rshx gets a process group of its own, the way a shell puts a foreground job
/// in one. That is what makes `interrupt` a faithful stand-in for a terminal:
/// `SIGINT` to the group reaches rshx and nothing else, and rshx's own
/// children are in groups of their own.
fn start(harness: &support::Harness, flags: &[&str]) -> Child {
    let file = harness.write("hosts.toml", THREE);
    let mut cmd = harness.rshx();
    cmd.args(["-H", file.to_str().unwrap()]);
    cmd.args(flags);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    spawn(&mut cmd)
}

/// Waits until `predicate` holds, or fails. Interrupts are asynchronous, so a
/// test has to wait for the state it is about to act on.
fn wait_until(what: &str, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if predicate() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

/// Sends `SIGINT` to rshx's process group, as a terminal does on Ctrl-C.
fn interrupt(child: &Child) {
    let pid = child.id() as i32;
    // SAFETY: `killpg` on a live pid; ESRCH is the only failure worth
    // expecting, and it means the run already ended.
    unsafe {
        libc::killpg(pid, libc::SIGINT);
    }
}

/// Reads a finished child's streams.
fn collect(mut child: Child) -> (i32, String, String) {
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut out) = child.stdout.take() {
        use std::io::Read;
        let _ = out.read_to_string(&mut stdout);
    }
    if let Some(mut err) = child.stderr.take() {
        use std::io::Read;
        let _ = err.read_to_string(&mut stderr);
    }
    let status = child.wait().expect("wait");
    (status.code().unwrap_or(-1), stdout, stderr)
}

#[test]
fn ctrl_c_cancels_the_hosts_in_flight_and_exits_99() {
    with_harness(|harness| {
        // Slow enough that the interrupt lands while they are all running.
        harness.respond_default(Response::ok().delay_ms(30_000));
        let child = start(harness, &["-f", "3", "run", "--", "sleep 30"]);

        wait_until("all three hosts to be in flight", || {
            harness.processes().len() == 3
        });
        interrupt(&child);
        let (code, stdout, stderr) = collect(child);

        assert_eq!(code, 99, "an interrupted run exits 99: {stderr}");
        for host in ["node01", "node02", "node03"] {
            assert!(
                stdout.contains(&format!("{host} cancelled")),
                "{host} is reported cancelled: {stdout:?}"
            );
        }
        assert!(
            stderr.contains("3 hosts: 0 ok, 3 cancelled"),
            "the summary counts them: {stderr}"
        );
    });
}

#[test]
fn a_cancelled_host_is_not_a_failure() {
    with_harness(|harness| {
        harness.respond_default(Response::ok().delay_ms(30_000));
        let child = start(harness, &["-f", "3", "run", "--", "sleep 30"]);
        wait_until("all three hosts to be in flight", || {
            harness.processes().len() == 3
        });
        interrupt(&child);
        let (code, stdout, stderr) = collect(child);

        assert_ne!(code, 2, "cancelled does not set the failed bit");
        assert_ne!(code, 4, "nor the unreachable bit");
        assert_eq!(code, 99);
        assert!(
            !stdout.contains("failed") && !stdout.contains("unreachable"),
            "no Host is reported as a failure: {stdout:?}"
        );
        assert!(
            !stderr.contains("0 failed") && !stderr.contains("0 unreachable"),
            "the summary does not invent failure counts: {stderr}"
        );
    });
}

#[test]
fn a_second_ctrl_c_does_not_wait_out_the_grace_period() {
    with_harness(|harness| {
        // The fake ssh ignores SIGTERM, since `sleep` in a shell script does
        // not die from it until the shell exits. It is the SIGKILL that ends
        // this run, so the second interrupt is what makes it prompt.
        harness.respond_default(Response::ok().delay_ms(30_000));
        let child = start(harness, &["-f", "3", "run", "--", "sleep 30"]);
        wait_until("all three hosts to be in flight", || {
            harness.processes().len() == 3
        });

        let started = Instant::now();
        interrupt(&child);
        std::thread::sleep(Duration::from_millis(200));
        interrupt(&child);
        let (code, _, stderr) = collect(child);
        let elapsed = started.elapsed();

        assert_eq!(code, 99, "{stderr}");
        assert!(
            elapsed < Duration::from_millis(1500),
            "the grace period was skipped, not waited out: {elapsed:?}"
        );
    });
}

#[test]
fn ctrl_c_stops_new_hosts_from_starting() {
    with_harness(|harness| {
        harness.respond_default(Response::ok().delay_ms(30_000));
        // Two at a time: with three hosts, one is still waiting for a slot
        // when the interrupt arrives.
        let child = start(harness, &["-f", "2", "run", "--", "sleep 30"]);
        wait_until("two hosts to be in flight", || {
            harness.processes().len() == 2
        });
        interrupt(&child);
        let (code, stdout, stderr) = collect(child);

        assert_eq!(code, 99, "{stderr}");
        assert_eq!(
            harness.processes().len(),
            2,
            "the third Host never started: {:?}",
            harness.invocations()
        );
        assert!(
            !stdout.contains("node03"),
            "a Host that never started has no result to report: {stdout:?}"
        );
        assert!(
            stderr.contains("1 not started"),
            "but the summary accounts for it: {stderr}"
        );
    });
}

#[test]
fn hosts_that_already_settled_keep_their_real_status() {
    with_harness(|harness| {
        // node01 finishes at once; the other two are still running when the
        // interrupt arrives.
        harness.respond("node01", Response::ok().stdout("done\n"));
        harness.respond("node02", Response::ok().delay_ms(30_000));
        harness.respond("node03", Response::ok().delay_ms(30_000));

        let child = start(harness, &["-f", "3", "run", "--", "du -hs /data"]);
        wait_until(
            "the settled host's line and the slow hosts to be running",
            || harness.processes().len() == 3,
        );
        // The fast Host's result has to be on stdout before the interrupt, or
        // this test would prove nothing about preserving it.
        std::thread::sleep(Duration::from_millis(300));
        interrupt(&child);
        let (code, stdout, stderr) = collect(child);

        assert_eq!(code, 99, "{stderr}");
        // The duration is formatting, not contract: under load this Host takes
        // a tenth of a second rather than none, and the test must not care.
        let settled = stdout
            .lines()
            .find(|line| line.starts_with("node01"))
            .unwrap_or_else(|| panic!("node01 has a result line: {stdout:?}"));
        assert!(
            settled.contains(" ok "),
            "a settled Host keeps its status: {stdout:?}"
        );
        assert!(
            settled.ends_with("done"),
            "and its output, folded onto the same line: {stdout:?}"
        );
        assert!(
            stdout.contains("node02 cancelled") && stdout.contains("node03 cancelled"),
            "the ones cut short are cancelled: {stdout:?}"
        );
        assert!(
            stderr.contains("3 hosts: 1 ok, 2 cancelled"),
            "the summary has both: {stderr}"
        );
    });
}

#[test]
fn the_report_says_the_remote_command_may_still_be_running() {
    with_harness(|harness| {
        harness.respond_default(Response::ok().delay_ms(30_000));
        let child = start(harness, &["-f", "3", "run", "--", "sleep 30"]);
        wait_until("all three hosts to be in flight", || {
            harness.processes().len() == 3
        });
        interrupt(&child);
        let (_, _, stderr) = collect(child);

        assert!(
            stderr.contains("stopped waiting"),
            "the report does not claim the remote work stopped: {stderr}"
        );
        assert!(
            stderr.contains("may still be running"),
            "and says so plainly: {stderr}"
        );
    });
}

#[test]
fn a_cancelled_host_reports_no_exit_code() {
    with_harness(|harness| {
        harness.respond_default(Response::ok().delay_ms(30_000));
        let child = start(harness, &["-f", "3", "--json", "run", "--", "sleep 30"]);
        wait_until("all three hosts to be in flight", || {
            harness.processes().len() == 3
        });
        interrupt(&child);
        let (code, stdout, _) = collect(child);

        assert_eq!(code, 99);
        for line in stdout.lines() {
            let object: serde_json::Value = serde_json::from_str(line).expect(line);
            assert_eq!(object["status"], "cancelled", "{line}");
            assert!(
                object.get("exit_code").is_none(),
                "there is no exit code to report: {line}"
            );
        }
    });
}

#[test]
fn a_run_that_is_never_interrupted_is_unaffected() {
    with_harness(|harness| {
        harness.respond_default(Response::ok());
        let file = harness.write("hosts.toml", THREE);

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "-f", "3", "run", "--", "true"]);
            cmd
        });

        assert_eq!(out.code, 0, "{}", out.stderr);
        assert!(out.stderr.contains("3 hosts: 3 ok"), "{}", out.stderr);
        assert!(
            !out.stderr.contains("stopped waiting"),
            "no note without a cancellation: {}",
            out.stderr
        );
    });
}

#[test]
fn each_ssh_child_runs_in_its_own_process_group() {
    with_harness(|harness| {
        harness.respond_default(Response::ok());
        let file = harness.write("hosts.toml", THREE);

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "-f", "3", "run", "--", "true"]);
            cmd
        });
        assert_eq!(out.code, 0, "{}", out.stderr);

        let processes = harness.processes();
        assert_eq!(processes.len(), 3, "{processes:?}");
        for process in &processes {
            assert_eq!(
                process.pgid, process.pid,
                "the child leads its own process group, so a terminal interrupt \
                 cannot reach it behind rshx's back: {process:?}"
            );
        }
        let groups: std::collections::BTreeSet<i32> =
            processes.iter().map(|process| process.pgid).collect();
        assert_eq!(
            groups.len(),
            3,
            "one group each, not a shared one: {groups:?}"
        );
    });
}

#[test]
fn the_interrupt_reaches_only_rshx() {
    with_harness(|harness| {
        // If the terminal's interrupt reached the children directly, they
        // would die before rshx could report them as cancelled, and the
        // summary would be missing a Host.
        harness.respond_default(Response::ok().delay_ms(30_000));
        let child = start(harness, &["-f", "3", "run", "--", "sleep 30"]);
        wait_until("all three hosts to be in flight", || {
            harness.processes().len() == 3
        });

        let rshx_pid = child.id() as i32;
        let before = harness.processes();
        for process in &before {
            assert_ne!(
                process.pgid, rshx_pid,
                "no child shares rshx's group: {process:?}"
            );
        }

        interrupt(&child);
        let (code, stdout, _) = collect(child);

        assert_eq!(code, 99);
        assert_eq!(
            stdout
                .lines()
                .filter(|line| line.contains("cancelled"))
                .count(),
            3,
            "all three were still rshx's to report: {stdout:?}"
        );
    });
}

#[test]
fn a_line_oriented_consumer_sees_the_cancellation_arrive() {
    with_harness(|harness| {
        harness.respond_default(Response::ok().delay_ms(30_000));
        let mut child = start(harness, &["-f", "3", "--json", "run", "--", "sleep 30"]);
        let stdout = child.stdout.take().expect("piped");
        let mut lines = BufReader::new(stdout).lines();

        wait_until("all three hosts to be in flight", || {
            harness.processes().len() == 3
        });
        interrupt(&child);

        // The results are written as each Host is cut short, not held back
        // until the process is about to exit.
        let first = lines.next().expect("a first line").expect("readable");
        let object: serde_json::Value = serde_json::from_str(&first).expect(&first);
        assert_eq!(object["status"], "cancelled", "{first}");
        let rest: Vec<String> = lines.map(|line| line.expect("readable")).collect();
        assert_eq!(rest.len(), 2, "{rest:?}");
        assert_eq!(child.wait().expect("wait").code(), Some(99));
    });
}
