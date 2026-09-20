//! The first complete path — host file in, per-host report and exit
//! code out.

mod support;

use support::{Response, run, with_harness};

const THREE: &str = r#"
[[hosts]]
name = "node01"

[[hosts]]
name = "node02"

[[hosts]]
name = "node03"
"#;

#[test]
fn prints_one_line_per_host() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", THREE);
        harness.respond_default(Response::ok().stdout("hi\n"));

        // A fanout of one makes the report follow the host file; with a wider
        // fanout hosts are reported as they settle, which tests/fanout.rs
        // covers.
        let out = run({
            let mut cmd = harness.rshx();
            cmd.args([
                "-H",
                file.to_str().unwrap(),
                "-f",
                "1",
                "run",
                "--",
                "hostname",
            ]);
            cmd
        });

        assert_eq!(out.code, 0, "stderr: {}", out.stderr);
        let lines = out.stdout_lines();
        assert_eq!(lines.len(), 3, "one line per host: {:?}", out.stdout);
        assert!(lines[0].starts_with("node01 "), "{:?}", lines[0]);
        assert!(lines[1].starts_with("node02 "), "{:?}", lines[1]);
        assert!(lines[2].starts_with("node03 "), "{:?}", lines[2]);
        assert!(
            lines[0].ends_with("hi"),
            "a Host's output folds onto its own line: {:?}",
            out.stdout
        );
        assert!(out.stderr.contains("3 hosts"), "{:?}", out.stderr);
    });
}

#[test]
fn forwards_the_command_verbatim_including_hyphen_arguments() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", THREE);
        harness.respond_default(Response::ok());

        run({
            let mut cmd = harness.rshx();
            cmd.args([
                "-H",
                file.to_str().unwrap(),
                "run",
                "--",
                "uptime",
                "-p",
                "--since",
                "1 day ago",
            ]);
            cmd
        });

        let invocations = harness.invocations();
        assert_eq!(invocations.len(), 3);
        assert_eq!(
            harness.command_for("node01"),
            "uptime -p --since 1 day ago",
            "the command reaches the Host as it was written, never split by rshx"
        );
        assert_eq!(
            invocations
                .iter()
                .filter(|argv| argv.get(1).map(String::as_str) == Some("node02"))
                .count(),
            1,
            "every host is invoked exactly once: {invocations:?}"
        );
    });
}

#[test]
fn rejects_names_that_are_not_on_the_whitelist() {
    with_harness(|harness| {
        for (name, bad) in [
            ("root@prod", "@"),
            ("node/01", "/"),
            ("node 01", " "),
            ("-node01", "`-`"),
        ] {
            let file = harness.write("bad.toml", &format!("[[hosts]]\nname = {name:?}\n"));
            let out = run({
                let mut cmd = harness.rshx();
                cmd.args(["-H", file.to_str().unwrap(), "run", "--", "hostname"]);
                cmd
            });
            assert_eq!(out.code, 1, "{name:?} should be rejected: {}", out.stderr);
            assert!(
                out.stderr.contains(bad),
                "{name:?} should be rejected with a reason mentioning {bad}: {}",
                out.stderr
            );
        }
        assert!(
            harness.invocations().is_empty(),
            "a rejected host file runs nothing"
        );
    });
}

#[test]
fn rejects_two_entries_declaring_the_same_name() {
    with_harness(|harness| {
        let file = harness.write(
            "dup.toml",
            "[[hosts]]\nname = \"node01\"\n\n[[hosts]]\nname = \"node01\"\n",
        );
        let out = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "run", "--", "hostname"]);
            cmd
        });

        assert_eq!(out.code, 1);
        assert!(
            out.stderr.contains("node01") && out.stderr.contains('1') && out.stderr.contains('2'),
            "the error names both entries: {}",
            out.stderr
        );
    });
}

#[test]
fn a_missing_host_file_is_a_local_error() {
    with_harness(|harness| {
        let out = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", "nope.toml", "run", "--", "hostname"]);
            cmd
        });
        assert_eq!(out.code, 1, "{}", out.stderr);
        assert!(out.stderr.contains("nope.toml"), "{}", out.stderr);
    });
}

#[test]
fn a_toml_syntax_error_keeps_the_parser_line_and_column() {
    with_harness(|harness| {
        let file = harness.write("broken.toml", "[[hosts]\nname = \"node01\"\n");
        let out = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "run", "--", "hostname"]);
            cmd
        });

        assert_eq!(out.code, 1);
        assert!(
            out.stderr.contains("line 1") && out.stderr.contains("column"),
            "the parser's position survives into the message: {}",
            out.stderr
        );
    });
}

#[test]
fn a_usage_error_exits_five_not_two() {
    with_harness(|harness| {
        let out = run({
            let mut cmd = harness.rshx();
            cmd.arg("--no-such-flag");
            cmd
        });
        assert_eq!(
            out.code, 5,
            "2 means a Host failed, so usage errors take clap's code: {}",
            out.stderr
        );
        assert!(!out.stderr.is_empty(), "the usage error is explained");
    });
}

#[test]
fn a_missing_command_is_a_usage_error() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", THREE);
        let out = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap()]);
            cmd
        });
        assert_eq!(out.code, 5, "{}", out.stderr);
    });
}

#[test]
fn exit_codes_follow_the_bit_flag_scheme() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", THREE);
        harness.respond("node01", Response::ok());
        harness.respond("node02", Response::failed(3));
        harness.respond("node03", Response::unreachable());

        let both = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "run", "--", "hostname"]);
            cmd
        });
        assert_eq!(
            both.code, 6,
            "failed and unreachable together: {}",
            both.stderr
        );
        assert!(
            both.stdout.contains("node02 failed"),
            "a non-zero remote status is `failed`: {:?}",
            both.stdout
        );
        assert!(
            both.stdout.contains("node03 unreachable"),
            "255 is `unreachable`: {:?}",
            both.stdout
        );
        assert!(
            both.stderr.contains("1 ok, 1 failed, 1 unreachable"),
            "and the summary says the same thing: {}",
            both.stderr
        );

        harness.respond("node02", Response::ok());
        harness.respond("node03", Response::ok());
        let ok = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "run", "--", "hostname"]);
            cmd
        });
        assert_eq!(ok.code, 0, "{}", ok.stderr);
        assert!(
            ok.stderr.contains("3 hosts: 3 ok"),
            "an all-ok run says so: {}",
            ok.stderr
        );

        harness.respond("node02", Response::failed(1));
        let failed = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "run", "--", "hostname"]);
            cmd
        });
        assert_eq!(failed.code, 2, "{}", failed.stderr);
        assert!(
            failed.stderr.contains("2 ok, 1 failed"),
            "{}",
            failed.stderr
        );

        harness.respond("node02", Response::ok());
        harness.respond("node03", Response::unreachable());
        let unreachable = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "run", "--", "hostname"]);
            cmd
        });
        assert_eq!(unreachable.code, 4, "{}", unreachable.stderr);
        assert!(
            unreachable.stderr.contains("2 ok, 1 unreachable"),
            "{}",
            unreachable.stderr
        );
    });
}

#[test]
fn status_never_comes_from_output_text() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", THREE);
        // Succeeds, but its stderr reads exactly like a connection failure.
        harness.respond(
            "node01",
            Response::ok().stderr("ssh: connect to host node01 port 22: Connection refused\n"),
        );
        // Fails with 255, but its stdout looks like a perfectly good answer.
        harness.respond("node02", Response::unreachable().stdout("node02\n"));
        harness.respond("node03", Response::ok());

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "run", "--", "hostname"]);
            cmd
        });

        assert!(
            out.stdout.contains("node01 ok"),
            "text that looks like a failure cannot demote an exit code of 0: {:?}",
            out.stdout
        );
        assert!(
            out.stdout.contains("node02 unreachable"),
            "text that looks like success cannot promote an exit code of 255: {:?}",
            out.stdout
        );
        assert_eq!(out.code, 4);
    });
}

#[test]
fn the_host_file_is_found_by_default() {
    with_harness(|harness| {
        harness.respond_default(Response::ok());

        harness.write("rshx.toml", THREE);
        let local = run({
            let mut cmd = harness.rshx();
            cmd.args(["run", "--", "hostname"]);
            cmd
        });
        assert_eq!(local.code, 0, "{}", local.stderr);
        assert_eq!(local.stdout_lines().len(), 3);

        // A host file in the current directory wins over the config directory.
        harness.write("xdg/rshx/hosts.toml", "[[hosts]]\nname = \"elsewhere\"\n");
        let still_local = run({
            let mut cmd = harness.rshx();
            cmd.args(["run", "--", "hostname"]);
            cmd
        });
        assert!(
            still_local.stdout.contains("node01"),
            "./rshx.toml is tried before the config directory: {:?}",
            still_local.stdout
        );

        // With no local file, the config directory is used.
        std::fs::remove_file(harness.path().join("rshx.toml")).unwrap();
        let xdg = run({
            let mut cmd = harness.rshx();
            cmd.args(["run", "--", "hostname"]);
            cmd
        });
        assert_eq!(xdg.code, 0, "{}", xdg.stderr);
        assert!(
            xdg.stdout.contains("elsewhere"),
            "the config directory is the fallback: {:?}",
            xdg.stdout
        );
    });
}

#[test]
fn the_summary_agrees_with_the_exit_code() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", THREE);
        harness.respond("node01", Response::ok());
        harness.respond("node02", Response::failed(3));
        harness.respond("node03", Response::failed(4));

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "run", "--", "hostname"]);
            cmd
        });

        assert_eq!(out.code, 2);
        assert!(
            out.stderr.contains("3 hosts: 1 ok, 2 failed"),
            "the summary counts what the exit code reflects: {}",
            out.stderr
        );
        assert!(
            !out.stdout.contains("3 hosts"),
            "the summary is not on stdout, which is one line per Host: {:?}",
            out.stdout
        );
    });
}

#[test]
fn a_failed_host_shows_its_stderr() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", "[[hosts]]\nname = \"node01\"\n");
        harness.respond(
            "node01",
            Response::failed(1).stderr("bash: nope: command not found\n"),
        );

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "run", "--", "nope"]);
            cmd
        });

        assert_eq!(out.code, 2);
        assert!(
            out.stdout.contains("bash: nope: command not found"),
            "a host that is not ok always shows why: {:?}",
            out.stdout
        );
    });
}
