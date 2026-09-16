//! The per-host timeout: `--timeout` bounds how long rshx waits for any one
//! Host, and only that Host.

mod support;

use std::time::{Duration, Instant};

use support::{Response, run, with_harness};

const THREE: &str = "[[hosts]]\nname = \"node01\"\n\n[[hosts]]\nname = \"node02\"\n\n[[hosts]]\nname = \"node03\"\n";

fn rshx(
    harness: &support::Harness,
    file: &std::path::Path,
    flags: &[&str],
) -> std::process::Command {
    let mut cmd = harness.rshx();
    cmd.args(["-H", file.to_str().unwrap(), "-f", "3"]);
    cmd.args(flags);
    cmd
}

#[test]
fn a_host_that_outlives_the_limit_times_out_and_the_run_moves_on() {
    with_harness(|harness| {
        // node01 never finishes inside the limit; the others are instant.
        harness.respond("node01", Response::ok().delay_ms(30_000));
        harness.respond("node02", Response::ok().stdout("quick\n"));
        harness.respond("node03", Response::ok().stdout("quick\n"));
        let file = harness.write("hosts.toml", THREE);

        let started = Instant::now();
        let out = run(rshx(harness, &file, &["--timeout", "1s", "--", "sleep 30"]));
        let elapsed = started.elapsed();

        assert!(
            out.stdout.contains("node01 timeout"),
            "the slow Host is reported as a timeout: {:?}",
            out.stdout
        );
        assert!(
            out.stdout.contains("node02 ok") && out.stdout.contains("node03 ok"),
            "the others are unaffected: {:?}",
            out.stdout
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "the run did not wait for the slow Host: {elapsed:?}"
        );
    });
}

#[test]
fn hosts_that_finish_inside_the_limit_are_untouched() {
    with_harness(|harness| {
        harness.respond_default(Response::ok().stdout("done\n").delay_ms(100));
        let file = harness.write("hosts.toml", THREE);

        let out = run(rshx(harness, &file, &["--timeout", "10s", "--", "true"]));

        assert_eq!(out.code, 0, "{}", out.stderr);
        assert_eq!(
            out.stdout
                .lines()
                .filter(|line| line.contains(" ok "))
                .count(),
            3,
            "all three are ok: {:?}",
            out.stdout
        );
        assert!(out.stderr.contains("3 hosts: 3 ok"), "{}", out.stderr);
    });
}

#[test]
fn a_run_where_every_host_times_out_exits_4() {
    with_harness(|harness| {
        harness.respond_default(Response::ok().delay_ms(30_000));
        let file = harness.write("hosts.toml", THREE);

        let out = run(rshx(harness, &file, &["--timeout", "1s", "--", "sleep 30"]));

        assert_eq!(
            out.code, 4,
            "a timeout counts as unreachable for the exit code: {}",
            out.stderr
        );
        assert!(
            out.stderr.contains("3 timeout"),
            "and the summary says timeout, not unreachable: {}",
            out.stderr
        );
    });
}

#[test]
fn timeout_and_unreachable_are_told_apart_in_the_report() {
    with_harness(|harness| {
        harness.respond("node01", Response::ok().delay_ms(30_000));
        harness.respond(
            "node02",
            Response::unreachable()
                .stderr("ssh: connect to host node02 port 22: Connection refused\n"),
        );
        harness.respond("node03", Response::ok());
        let file = harness.write("hosts.toml", THREE);

        let out = run(rshx(harness, &file, &["--timeout", "1s", "--", "x"]));

        assert!(out.stdout.contains("node01 timeout"), "{:?}", out.stdout);
        assert!(
            out.stdout.contains("node02 unreachable"),
            "a connection failure is still unreachable, not a timeout: {:?}",
            out.stdout
        );
        assert!(
            out.stdout.contains("(connect)"),
            "and it still has its cause: {:?}",
            out.stdout
        );
        assert!(
            !out.stdout.contains("node01 unreachable") && !out.stdout.contains("node02 timeout"),
            "neither is mislabelled: {:?}",
            out.stdout
        );
        assert_eq!(out.code, 4, "both share exit code 4: {}", out.stderr);
        assert!(
            out.stderr.contains("1 unreachable") && out.stderr.contains("1 timeout"),
            "the summary counts them separately: {}",
            out.stderr
        );
    });
}

#[test]
fn a_timed_out_host_reports_no_exit_code() {
    with_harness(|harness| {
        harness.respond_default(Response::ok().delay_ms(30_000));
        let file = harness.write("hosts.toml", THREE);

        let out = run(rshx(
            harness,
            &file,
            &["--timeout", "1s", "--json", "--", "sleep 30"],
        ));

        assert_eq!(out.code, 4, "{}", out.stderr);
        for line in out.stdout.lines() {
            let object: serde_json::Value = serde_json::from_str(line).expect(line);
            assert_eq!(object["status"], "timeout", "{line}");
            assert!(
                object.get("exit_code").is_none(),
                "rshx ended the child, so there is no exit code to report: {line}"
            );
        }
    });
}

#[test]
fn durations_are_read_the_way_humantime_writes_them() {
    with_harness(|harness| {
        harness.respond_default(Response::ok());
        let file = harness.write("hosts.toml", THREE);

        for duration in ["30s", "5m", "1h", "500ms", "2m30s"] {
            let out = run(rshx(harness, &file, &["--timeout", duration, "--", "true"]));
            assert_eq!(out.code, 0, "--timeout {duration}: {}", out.stderr);
        }
    });
}

#[test]
fn a_malformed_duration_is_a_usage_error() {
    with_harness(|harness| {
        harness.respond_default(Response::ok());
        let file = harness.write("hosts.toml", THREE);

        for duration in ["soon", "30", "-5s", ""] {
            let out = run(rshx(harness, &file, &["--timeout", duration, "--", "true"]));
            assert_eq!(
                out.code, 5,
                "--timeout {duration:?} is a usage error: {}",
                out.stderr
            );
        }
    });
}

#[test]
fn no_limit_applies_when_the_flag_is_absent() {
    with_harness(|harness| {
        // Slower than the limits the other tests use, but with no flag it must
        // still be waited for.
        harness.respond_default(Response::ok().delay_ms(1500));
        let file = harness.write("hosts.toml", THREE);

        let out = run(rshx(harness, &file, &["--", "sleep 1.5"]));

        assert_eq!(out.code, 0, "{}", out.stderr);
        assert!(out.stderr.contains("3 hosts: 3 ok"), "{}", out.stderr);
        assert!(
            !out.stdout.contains("timeout"),
            "nothing timed out: {:?}",
            out.stdout
        );
    });
}

#[test]
fn a_timed_out_child_is_terminated_rather_than_left_running() {
    with_harness(|harness| {
        // The fake ssh's own `sleep` outlives the SIGTERM that reaches the
        // script, so it takes the SIGKILL to end it. If rshx only sent
        // SIGTERM and walked away, the child would linger past the run.
        harness.respond_default(Response::ok().delay_ms(30_000));
        let file = harness.write("hosts.toml", THREE);

        let out = run(rshx(harness, &file, &["--timeout", "1s", "--", "sleep 30"]));

        assert_eq!(out.code, 4, "{}", out.stderr);
        // The run returned only once the children were gone: `run` waits for
        // rshx to exit, and rshx waits for its children.
        let pids: Vec<i32> = harness.processes().iter().map(|p| p.pid).collect();
        assert_eq!(pids.len(), 3, "{pids:?}");
        for pid in pids {
            // SAFETY: signal 0 checks for the process's existence.
            let alive = unsafe { libc::kill(pid, 0) } == 0;
            assert!(
                !alive,
                "the timed-out child {pid} was terminated, not orphaned"
            );
        }
    });
}

#[test]
fn the_report_says_a_timed_out_remote_command_may_still_be_running() {
    with_harness(|harness| {
        harness.respond_default(Response::ok().delay_ms(30_000));
        let file = harness.write("hosts.toml", THREE);

        let out = run(rshx(harness, &file, &["--timeout", "1s", "--", "sleep 30"]));

        assert!(
            out.stderr.contains("stopped waiting"),
            "the report does not claim the remote work stopped: {}",
            out.stderr
        );
    });
}

#[test]
fn one_slow_host_does_not_delay_the_others() {
    with_harness(|harness| {
        harness.respond("node01", Response::ok().delay_ms(30_000));
        harness.respond("node02", Response::ok().delay_ms(300));
        harness.respond("node03", Response::ok().delay_ms(300));
        let file = harness.write("hosts.toml", THREE);

        let started = Instant::now();
        let out = run(rshx(harness, &file, &["--timeout", "2s", "--", "x"]));
        let elapsed = started.elapsed();

        assert_eq!(out.code, 4, "{}", out.stderr);
        assert!(
            out.stderr.contains("2 ok, 1 timeout"),
            "the quick Hosts still succeeded: {}",
            out.stderr
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the run was bounded by the limit, not by the slow Host: {elapsed:?}"
        );
    });
}

#[test]
fn a_timeout_does_not_set_the_failed_bit() {
    with_harness(|harness| {
        harness.respond_default(Response::ok().delay_ms(30_000));
        let file = harness.write("hosts.toml", THREE);

        let out = run(rshx(harness, &file, &["--timeout", "1s", "--", "sleep 30"]));

        assert_ne!(out.code, 2, "a timeout is not a failure");
        assert_ne!(out.code, 99, "nor a cancellation: it is exit 4");
        assert_eq!(out.code, 4);
    });
}

#[test]
fn the_limit_applies_per_host_not_to_the_run() {
    with_harness(|harness| {
        // Each Host takes 400ms and the limit is 1s: a run-wide limit would
        // cut the last Hosts off, a per-host one leaves them alone.
        harness.respond_default(Response::ok().delay_ms(400));
        let file = harness.write("hosts.toml", THREE);

        let out = run(rshx(
            harness,
            &file,
            &["--timeout", "1s", "--", "sleep 0.4"],
        ));

        assert_eq!(out.code, 0, "{}", out.stderr);
        assert!(out.stderr.contains("3 hosts: 3 ok"), "{}", out.stderr);
    });
}

#[test]
fn a_host_that_times_out_is_not_reported_as_cancelled() {
    with_harness(|harness| {
        harness.respond_default(Response::ok().delay_ms(30_000));
        let file = harness.write("hosts.toml", THREE);

        let out = run(rshx(
            harness,
            &file,
            &["--timeout", "1s", "--json", "--", "sleep 30"],
        ));

        assert!(
            !out.stdout.contains("cancelled"),
            "a timeout is its own status: {:?}",
            out.stdout
        );
        assert_eq!(out.code, 4, "and its own exit code, not 99: {}", out.stderr);
    });
}
