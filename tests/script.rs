//! `run --script`: a local script copied to each Host, made executable, run
//! there, and removed again.

mod support;

use support::{Attach, Response, run, run_on_tty, screen, with_harness};

const ONE: &str = r#"
[[hosts]]
name = "node01"
"#;

const THREE: &str = r#"
[[hosts]]
name = "node01"

[[hosts]]
name = "node02"

[[hosts]]
name = "node03"
"#;

/// The script as it is written locally, and so as it must arrive.
const SCRIPT: &str = "#!/bin/sh\necho deployed\n";

/// What a Host answers the copy with: the path it wrote the script to.
const PATH: &str = "/tmp/tmp.abc123";

/// The remote side of the copy, asserted verbatim: a temporary file, the
/// script written into it, and that file's path printed — no newline, so a
/// Host's stdout is the path and nothing else. It is the contract with the
/// remote shell, which is why it is spelled out here rather than imported.
const COPY: &str =
    r#"tmp=$(mktemp) || exit 1; cat > "$tmp" || { rm -f -- "$tmp"; exit 1; }; printf '%s' "$tmp""#;

/// The words a Host runs the copied script with, asserted verbatim: the file
/// made executable and run as one shell word, then — outside it, so a sudo
/// that refuses still removes the file — its status kept and the file removed.
/// This is the contract with the remote shell, which is why it is spelled out
/// here rather than imported.
fn run_argv(path: &str, privilege: bool) -> Vec<String> {
    let chain = format!(r#"'chmod +x -- "{path}" && "{path}"';"#);
    let mut argv = match privilege {
        true => vec!["sudo", "-S", "-p", "rshx-password:", "sh", "-c", &chain],
        false => vec!["sh", "-c", &chain],
    }
    .into_iter()
    .map(str::to_string)
    .collect::<Vec<String>>();
    argv.push(format!(r#"rc=$?; rm -f -- "{path}"; exit $rc"#));
    argv
}

/// One Host, a script to copy to it, and the two invocations that takes.
fn copying(harness: &support::Harness) -> std::path::PathBuf {
    harness.write("deploy.sh", SCRIPT)
}

/// The command a Host runs the copied script with, as the one line the Host's
/// shell parses.
fn run_text(path: &str, privilege: bool) -> String {
    run_argv(path, privilege).join(" ")
}

#[test]
fn the_script_is_copied_to_a_temporary_file_and_run_from_it() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", ONE);
        let script = copying(harness);
        harness.respond_sequence(
            "node01",
            &[
                Response::ok().stdout(PATH).captures_stdin(),
                Response::ok().stdout("deployed\n"),
            ],
        );

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args([
                "-H",
                file.to_str().unwrap(),
                "-f",
                "1",
                "run",
                "--script",
                script.to_str().unwrap(),
            ]);
            cmd
        });

        assert_eq!(out.code, 0, "stderr: {}", out.stderr);
        assert_eq!(
            harness.commands(),
            vec![
                ("node01".to_string(), COPY.to_string()),
                ("node01".to_string(), run_text(PATH, false)),
            ],
            "the copy runs first, and the run names the path the copy printed"
        );
        assert_eq!(
            harness.raw_stdin("node01"),
            SCRIPT.as_bytes(),
            "the script is what ssh carried to the Host, byte for byte"
        );
        assert_eq!(
            out.stdout_lines().len(),
            1,
            "one line for the Host: {:?}",
            out.stdout
        );
        assert!(
            out.stdout.contains("node01 ok") && out.stdout.contains("deployed"),
            "the script's own output is the Host's result: {:?}",
            out.stdout
        );
    });
}

#[test]
fn the_scripts_exit_status_is_the_hosts() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", ONE);
        let script = copying(harness);
        harness.respond_sequence(
            "node01",
            &[
                Response::ok().stdout(PATH),
                Response::failed(3).stdout("halfway\n").stderr("boom\n"),
            ],
        );

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args([
                "-H",
                file.to_str().unwrap(),
                "--json",
                "run",
                "--script",
                script.to_str().unwrap(),
            ]);
            cmd
        });

        assert_eq!(out.code, 2, "stderr: {}", out.stderr);
        let object: serde_json::Value =
            serde_json::from_str(out.stdout.trim()).expect("one object per Host");
        assert_eq!(object["status"], "failed");
        assert_eq!(
            object["exit_code"], 3,
            "the script's own status, not ssh's: {object}"
        );
        assert_eq!(object["stdout"], "halfway\n");
        assert_eq!(object["stderr"], "boom\n");
    });
}

#[test]
fn every_host_gets_its_own_copy_and_runs_its_own_path() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", THREE);
        let script = copying(harness);
        for (index, host) in ["node01", "node02", "node03"].iter().enumerate() {
            let path = format!("/tmp/tmp.{index}");
            harness.respond_sequence(
                host,
                &[
                    Response::ok().stdout(&path).captures_stdin(),
                    Response::ok().stdout("done\n"),
                ],
            );
        }

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args([
                "-H",
                file.to_str().unwrap(),
                "-f",
                "3",
                "run",
                "--script",
                script.to_str().unwrap(),
            ]);
            cmd
        });

        assert_eq!(out.code, 0, "stderr: {}", out.stderr);
        let commands = harness.commands();
        assert_eq!(commands.len(), 6, "two per Host: {commands:?}");
        for (index, host) in ["node01", "node02", "node03"].iter().enumerate() {
            let path = format!("/tmp/tmp.{index}");
            assert!(
                commands.contains(&(host.to_string(), run_text(&path, false))),
                "{host} runs the file it was sent, not another Host's: {commands:?}"
            );
            assert_eq!(
                harness.raw_stdin(host),
                SCRIPT.as_bytes(),
                "{host} was sent the script"
            );
        }
    });
}

#[test]
fn a_copy_that_fails_stops_before_anything_is_run() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", ONE);
        let script = copying(harness);
        harness.respond_sequence("node01", &[Response::unreachable()]);

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args([
                "-H",
                file.to_str().unwrap(),
                "run",
                "--script",
                script.to_str().unwrap(),
            ]);
            cmd
        });

        assert_eq!(out.code, 4, "stderr: {}", out.stderr);
        assert_eq!(
            harness.invocations().len(),
            1,
            "nothing is run when the script never arrived: {:?}",
            harness.invocations()
        );
        assert!(
            out.stdout.contains("unreachable"),
            "ssh's own status stands: {:?}",
            out.stdout
        );
    });
}

#[test]
fn a_host_that_prints_no_path_is_never_run_on() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", ONE);
        let script = copying(harness);
        // A copy that exits zero without saying where it put the file: there is
        // nothing to run, and the Host's own output is the only clue.
        harness.respond_sequence(
            "node01",
            &[Response::ok()
                .stdout("who knows\n")
                .stderr("no /tmp here\n")],
        );

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args([
                "-H",
                file.to_str().unwrap(),
                "run",
                "--script",
                script.to_str().unwrap(),
            ]);
            cmd
        });

        assert_eq!(out.code, 4, "stderr: {}", out.stderr);
        assert_eq!(
            harness.invocations().len(),
            1,
            "the script is not run from a path rshx does not have: {:?}",
            harness.invocations()
        );
        assert!(
            out.stdout.contains("no temporary path"),
            "rshx says why nothing ran: {:?}",
            out.stdout
        );
        assert!(
            out.stdout.contains("no /tmp here"),
            "and keeps what the Host printed: {:?}",
            out.stdout
        );
    });
}

#[test]
fn privilege_elevates_the_whole_chain_and_not_the_copy() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", ONE);
        let script = copying(harness);
        harness.respond_sequence(
            "node01",
            &[Response::ok().stdout(PATH), Response::ok().stdout("root\n")],
        );

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args([
                "-H",
                file.to_str().unwrap(),
                "--privilege",
                "run",
                "--script",
                script.to_str().unwrap(),
            ]);
            cmd
        });

        assert_eq!(out.code, 0, "stderr: {}", out.stderr);
        // The copy writes where the Host's user may write, and removing it is
        // theirs to do; only the run itself is elevated.
        assert_eq!(
            harness.commands(),
            vec![
                ("node01".to_string(), COPY.to_string()),
                ("node01".to_string(), run_text(PATH, true)),
            ]
        );
    });
}

#[test]
fn the_limit_covers_the_copy_and_the_run_together() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", ONE);
        let script = copying(harness);
        // Each step fits inside the second on its own; together they do not, so
        // the Host's limit is what the run is bounded by.
        harness.respond_sequence(
            "node01",
            &[
                Response::ok().stdout(PATH).delay_ms(600),
                Response::ok().stdout("done\n").delay_ms(600),
            ],
        );

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args([
                "-H",
                file.to_str().unwrap(),
                "--timeout",
                "1s",
                "run",
                "--script",
                script.to_str().unwrap(),
            ]);
            cmd
        });

        assert_eq!(out.code, 4, "stderr: {}", out.stderr);
        assert!(
            out.stdout.contains("timeout"),
            "the two steps share one limit: {:?}",
            out.stdout
        );
        assert_eq!(
            harness.invocations().len(),
            3,
            "the copy and the run were started, and the Host was stopped: {:?}",
            harness.invocations()
        );
        assert_eq!(
            harness.stops().len(),
            1,
            "the Host that ran out of time had its script stopped there: {:?}",
            harness.stops()
        );
    });
}

#[test]
fn the_heading_names_the_script() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", ONE);
        let script = copying(harness);
        harness.respond_sequence(
            "node01",
            &[Response::ok().stdout(PATH), Response::ok().stdout("done\n")],
        );

        let out = run_on_tty(
            {
                let mut cmd = harness.rshx();
                cmd.args([
                    "-H",
                    file.to_str().unwrap(),
                    "run",
                    "--script",
                    script.to_str().unwrap(),
                ]);
                cmd
            },
            Attach::STDERR_ONLY,
        );

        let chrome = screen(&out.stderr);
        assert!(
            chrome.contains("deploy.sh (script)") && chrome.contains("1 host"),
            "the heading says what runs, and on how many Hosts: {chrome:?}"
        );
    });
}

#[test]
fn a_script_that_cannot_be_read_is_a_local_error() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", ONE);
        harness.respond_default(Response::ok());

        let missing = harness.path().join("nope.sh");
        let out = run({
            let mut cmd = harness.rshx();
            cmd.args([
                "-H",
                file.to_str().unwrap(),
                "run",
                "--script",
                missing.to_str().unwrap(),
            ]);
            cmd
        });

        assert_eq!(out.code, 1, "stderr: {}", out.stderr);
        assert!(
            out.stderr.contains("nope.sh"),
            "the reason names the file: {:?}",
            out.stderr
        );
        assert!(
            harness.invocations().is_empty(),
            "nothing is sent anywhere: {:?}",
            harness.invocations()
        );
    });
}

#[test]
fn a_directory_is_not_a_script() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", ONE);
        harness.respond_default(Response::ok());
        let dir = harness.path().join("scripts");
        std::fs::create_dir_all(&dir).unwrap();

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args([
                "-H",
                file.to_str().unwrap(),
                "run",
                "--script",
                dir.to_str().unwrap(),
            ]);
            cmd
        });

        assert_eq!(out.code, 1, "stderr: {}", out.stderr);
        assert!(
            out.stderr.contains("is not a file"),
            "the reason says what was wrong with it: {:?}",
            out.stderr
        );
        assert!(harness.invocations().is_empty());
    });
}
