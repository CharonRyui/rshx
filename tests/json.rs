//! Machine-readable output: one JSON object per Host, one per line.

mod support;

use support::{Response, run, spawn, with_harness};

const ONE: &str = "[[hosts]]\nname = \"node01\"\n";
const TWO: &str = "[[hosts]]\nname = \"node01\"\n\n[[hosts]]\nname = \"node02\"\n";

fn rshx(
    harness: &support::Harness,
    file: &std::path::Path,
    flags: &[&str],
) -> std::process::Command {
    let mut cmd = harness.rshx();
    cmd.args(["-H", file.to_str().unwrap(), "-f", "1", "--json"]);
    cmd.args(flags);
    cmd
}

/// One JSON object per line, parsed.
fn objects(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .lines()
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|err| panic!("{line:?} is not JSON: {err}"))
        })
        .collect()
}

#[test]
fn each_host_settles_into_one_json_object_with_no_enclosing_array() {
    with_harness(|harness| {
        harness.respond_default(Response::ok().stdout("node01\n"));
        let file = harness.write("hosts.toml", TWO);

        let out = run(rshx(harness, &file, &["run", "--", "hostname"]));

        assert_eq!(out.code, 0, "{}", out.stderr);
        assert!(!out.stdout.contains('['), "no array: {:?}", out.stdout);
        let parsed = objects(&out.stdout);
        assert_eq!(parsed.len(), 2, "one object per Host: {:?}", out.stdout);
    });
}

#[test]
fn an_object_carries_every_documented_field() {
    with_harness(|harness| {
        harness.respond_default(Response::ok().stdout("node01\n").stderr("a warning\n"));
        let file = harness.write("hosts.toml", ONE);

        let out = run(rshx(harness, &file, &["run", "--", "hostname"]));
        let object = &objects(&out.stdout)[0];

        assert_eq!(object["host"], "node01");
        assert_eq!(object["status"], "ok");
        assert_eq!(object["exit_code"], 0);
        assert_eq!(object["stdout"], "node01\n");
        assert_eq!(object["stderr"], "a warning\n");
        assert_eq!(object["truncated"], false);
        assert_eq!(
            object["remote_stopped"], false,
            "an ok Host was never cut short, so nothing was stopped: {object}"
        );
        assert!(
            object["duration_ms"].is_u64(),
            "duration is a number of milliseconds: {object}"
        );
        assert!(
            object.get("cause").is_none(),
            "an ok Host has no cause: {object}"
        );
    });
}

#[test]
fn exit_code_is_absent_for_a_host_rshx_killed() {
    with_harness(|harness| {
        // A Host rshx itself terminated has no exit code of its own to report:
        // ssh exits 255 on SIGTERM, which would read as `unreachable` if it
        // were passed through.
        harness.respond_default(Response::ok().delay_ms(30_000));
        let file = harness.write("hosts.toml", ONE);

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args([
                "-H",
                file.to_str().unwrap(),
                "--timeout",
                "1s",
                "--json",
                "run",
                "--",
                "sleep 30",
            ]);
            cmd
        });

        assert_eq!(out.code, 4, "{}", out.stderr);
        let objects = objects(&out.stdout);
        assert_eq!(objects.len(), 1, "{:?}", out.stdout);
        assert_eq!(objects[0]["status"], "timeout", "{}", objects[0]);
        assert!(
            objects[0].get("exit_code").is_none(),
            "rshx ended the child, so there is no exit code: {}",
            objects[0]
        );
    });
}

#[test]
fn a_host_rshx_stopped_says_so_in_json() {
    with_harness(|harness| {
        harness.respond_default(Response::ok().delay_ms(30_000));
        let file = harness.write("hosts.toml", ONE);

        let out = run(rshx(
            harness,
            &file,
            &["--timeout", "1s", "run", "--", "sleep 30"],
        ));

        assert_eq!(out.code, 4, "{}", out.stderr);
        let object = &objects(&out.stdout)[0];
        assert_eq!(object["status"], "timeout", "{object}");
        assert_eq!(
            object["remote_stopped"], true,
            "rshx stopped the command it gave up on, and says so: {object}"
        );
    });
}

#[test]
fn a_host_rshx_could_not_stop_does_not_claim_it_was_stopped() {
    with_harness(|harness| {
        harness.respond_default(Response::ok().delay_ms(30_000));
        harness.respond(
            "stop",
            Response::failed(255)
                .stderr("ssh: connect to host node01 port 22: Connection refused\n"),
        );
        let file = harness.write("hosts.toml", ONE);

        let out = run(rshx(
            harness,
            &file,
            &["--timeout", "1s", "run", "--", "sleep 30"],
        ));

        let object = &objects(&out.stdout)[0];
        assert_eq!(
            object["remote_stopped"], false,
            "a stop that did not happen is not claimed: {object}"
        );
    });
}

#[test]
fn a_cause_is_present_only_when_there_is_one() {
    with_harness(|harness| {
        harness.respond(
            "node01",
            Response::unreachable()
                .stderr("ssh: connect to host node01 port 22: Connection timed out\n"),
        );
        harness.respond("node02", Response::failed(3).stderr("bash: nope\n"));
        let file = harness.write("hosts.toml", TWO);

        let out = run(rshx(harness, &file, &["run", "--", "x"]));
        let parsed = objects(&out.stdout);
        let by_host = |name: &str| {
            parsed
                .iter()
                .find(|o| o["host"] == name)
                .unwrap_or_else(|| panic!("no object for {name}: {parsed:?}"))
                .clone()
        };

        assert_eq!(by_host("node01")["cause"], "connect");
        assert_eq!(by_host("node01")["status"], "unreachable");
        assert_eq!(by_host("node01")["exit_code"], 255);
        assert_eq!(by_host("node02")["status"], "failed");
        assert_eq!(by_host("node02")["exit_code"], 3);
        assert!(
            by_host("node02").get("cause").is_none(),
            "an unrecognised failure has no cause"
        );
    });
}

#[test]
fn invalid_utf8_is_replaced_and_the_line_still_parses() {
    with_harness(|harness| {
        harness.respond_default(Response::ok().stdout_bytes(b"caf\xe9 \xff\xfe\n"));
        let file = harness.write("hosts.toml", ONE);

        let out = run(rshx(harness, &file, &["run", "--", "cat"]));

        assert_eq!(out.code, 0, "the run does not fail: {}", out.stderr);
        let object = &objects(&out.stdout)[0];
        let stdout = object["stdout"].as_str().expect("a JSON string");
        assert!(stdout.starts_with("caf"), "{stdout:?}");
        assert!(
            stdout.contains('\u{fffd}'),
            "the bad bytes became U+FFFD: {stdout:?}"
        );
    });
}

#[test]
fn a_stream_past_the_cap_is_cut_and_says_so() {
    with_harness(|harness| {
        // Just over the 1 MiB cap, with no newlines, so the whole stream is
        // one long line.
        let big = vec![b'x'; 1024 * 1024 + 4096];
        harness.respond_default(Response::ok().stdout_bytes(&big));
        let file = harness.write("hosts.toml", ONE);

        let out = run(rshx(harness, &file, &["run", "--", "cat"]));

        assert_eq!(out.code, 0, "{}", out.stderr);
        let object = &objects(&out.stdout)[0];
        assert_eq!(object["truncated"], true, "the object says so");
        assert_eq!(
            object["stdout"].as_str().unwrap().len(),
            1024 * 1024,
            "exactly the cap is kept"
        );
    });
}

#[test]
fn a_stream_under_the_cap_is_not_flagged() {
    with_harness(|harness| {
        harness.respond_default(Response::ok().stdout_bytes(&vec![b'x'; 1024 * 1024]));
        let file = harness.write("hosts.toml", ONE);

        let out = run(rshx(harness, &file, &["run", "--", "cat"]));

        let object = &objects(&out.stdout)[0];
        assert_eq!(
            object["truncated"], false,
            "a stream exactly at the cap lost nothing"
        );
        assert_eq!(object["stdout"].as_str().unwrap().len(), 1024 * 1024);
    });
}

#[test]
fn stdout_carries_only_json_lines_while_the_summary_stays_on_stderr() {
    with_harness(|harness| {
        harness.respond("node01", Response::ok());
        harness.respond("node02", Response::failed(1).stderr("boom\n"));
        let file = harness.write("hosts.toml", TWO);

        let out = run(rshx(harness, &file, &["run", "--", "x"]));

        assert_eq!(out.code, 2, "{}", out.stderr);
        for line in out.stdout.lines() {
            serde_json::from_str::<serde_json::Value>(line)
                .unwrap_or_else(|err| panic!("stdout has a non-JSON line {line:?}: {err}"));
        }
        assert!(
            out.stderr.contains("2 hosts: 1 ok, 1 failed"),
            "the summary is still chrome on stderr: {}",
            out.stderr
        );
    });
}

#[test]
fn json_is_never_coloured_even_when_asked_and_even_on_a_terminal() {
    with_harness(|harness| {
        harness.respond_default(Response::ok());
        let file = harness.write("hosts.toml", ONE);

        let forced = run(rshx(
            harness,
            &file,
            &["--color", "always", "run", "--", "hostname"],
        ));
        assert!(
            !forced.stdout.contains('\u{1b}'),
            "--color always must not corrupt JSON: {:?}",
            forced.stdout
        );

        let terminal = support::run_on_tty(
            rshx(
                harness,
                &file,
                &["--color", "always", "run", "--", "hostname"],
            ),
            support::Attach::BOTH,
        );
        let lines: Vec<&str> = terminal
            .stdout
            .lines()
            .filter(|line| line.contains("node01"))
            .collect();
        assert_eq!(lines.len(), 1, "{:?}", terminal.stdout);
        serde_json::from_str::<serde_json::Value>(lines[0])
            .unwrap_or_else(|err| panic!("the result line is still valid JSON: {err}"));
    });
}

#[test]
fn json_and_the_plain_report_agree_on_status_and_exit_code() {
    with_harness(|harness| {
        harness.respond("node01", Response::ok());
        harness.respond("node02", Response::failed(7).stderr("boom\n"));
        harness.respond(
            "node03",
            Response::unreachable()
                .stderr("ssh: connect to host node03 port 22: Connection refused\n"),
        );
        let file = harness.write("hosts.toml", "[[hosts]]\nname = \"node01\"\n\n[[hosts]]\nname = \"node02\"\n\n[[hosts]]\nname = \"node03\"\n");

        let json = run(rshx(harness, &file, &["run", "--", "x"]));
        let plain = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "-f", "1", "run", "--", "x"]);
            cmd
        });

        assert_eq!(json.code, plain.code, "same process exit code");
        for object in objects(&json.stdout) {
            let host = object["host"].as_str().unwrap();
            let line = plain
                .stdout
                .lines()
                .find(|line| line.starts_with(host))
                .unwrap_or_else(|| panic!("no plain line for {host}"));
            assert!(
                line.contains(object["status"].as_str().unwrap()),
                "plain {line:?} and json {object} agree on status"
            );
        }

        // The per-Host exit codes too: the plain report shows one for a Host
        // that ran, and the JSON says the same number.
        let node02 = objects(&json.stdout)
            .into_iter()
            .find(|object| object["host"] == "node02")
            .expect("node02");
        assert_eq!(
            node02["exit_code"], 7,
            "the remote status is reported, not rshx's: {node02}"
        );
        assert!(
            objects(&json.stdout)
                .iter()
                .all(|object| object.get("exit_code").is_some()),
            "every Host that ran has one: {:?}",
            json.stdout
        );
    });
}

#[test]
fn a_line_oriented_consumer_sees_results_as_they_settle() {
    with_harness(|harness| {
        use std::io::BufRead;
        use std::time::{Duration, Instant};

        harness.respond("node01", Response::ok());
        // Still running while the first result is already on the pipe.
        harness.respond("node02", Response::ok().delay_ms(1500));
        let file = harness.write("hosts.toml", TWO);

        // Two at once, so the fast Host's result is not waiting on the slow
        // Host for a slot.
        let mut cmd = harness.rshx();
        cmd.args([
            "-H",
            file.to_str().unwrap(),
            "-f",
            "2",
            "--json",
            "run",
            "--",
            "x",
        ]);
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        let mut child = spawn(&mut cmd);
        let mut lines = std::io::BufReader::new(child.stdout.take().unwrap()).lines();

        let started = Instant::now();
        let first = lines.next().expect("a first line").expect("readable");
        let elapsed = started.elapsed();

        assert!(first.contains("node01"), "{first}");
        assert!(
            elapsed < Duration::from_millis(1000),
            "the result was written before the slow Host settled, not buffered until the end: {elapsed:?}"
        );

        let rest: Vec<String> = lines.map(|line| line.expect("readable")).collect();
        assert_eq!(rest.len(), 1, "{rest:?}");
        assert!(rest[0].contains("node02"), "{rest:?}");
        assert_eq!(child.wait().expect("wait").code(), Some(0));
    });
}

#[test]
fn a_host_that_produces_nothing_still_gets_an_object() {
    with_harness(|harness| {
        harness.respond_default(Response::ok());
        let file = harness.write("hosts.toml", ONE);

        let out = run(rshx(harness, &file, &["run", "--", "true"]));

        let object = &objects(&out.stdout)[0];
        assert_eq!(object["stdout"], "");
        assert_eq!(object["stderr"], "");
        assert_eq!(object["status"], "ok");
    });
}

#[test]
fn newlines_inside_a_stream_do_not_break_the_one_line_per_host_rule() {
    with_harness(|harness| {
        harness.respond_default(Response::ok().stdout("a\nb\nc\n").stderr("x\ny\n"));
        let file = harness.write("hosts.toml", TWO);

        let out = run(rshx(harness, &file, &["run", "--", "cat"]));

        assert_eq!(
            out.stdout_lines().len(),
            2,
            "JSON escapes its newlines: {:?}",
            out.stdout
        );
        let parsed = objects(&out.stdout);
        assert_eq!(parsed[0]["stdout"], "a\nb\nc\n");
        assert_eq!(parsed[0]["stderr"], "x\ny\n");
    });
}
