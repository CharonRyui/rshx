//! The first complete path — host file in, per-host report and exit
//! code out.

mod support;

use support::{Response, Typed, run, run_on_tty_answering, with_harness};

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
fn the_command_starts_at_its_first_word_and_takes_the_rest() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", THREE);
        harness.respond_default(Response::ok().stdout("42G /data\n"));

        // `-q` and `-p` are rshx's own options, but they come after the
        // command, so they belong to the command: rshx must not read them.
        let out = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "run", "du", "-hs", "-q", "-p"]);
            cmd
        });

        assert_eq!(out.code, 0, "{}", out.stderr);
        assert_eq!(
            harness.command_for("node01"),
            "du -hs -q -p",
            "an option after the command reaches the Host, never rshx"
        );
        assert!(
            out.stdout.contains("42G"),
            "-q after the command is not rshx's quiet: {:?}",
            out.stdout
        );
    });
}

#[test]
fn options_read_the_same_on_either_side_of_the_subcommand() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", THREE);
        harness.respond_default(Response::ok().stdout("42G /data\n"));

        // Every option after the subcommand, before the command: the host file
        // is found (or the run would not happen), and `-q` takes effect.
        let out = run({
            let mut cmd = harness.rshx();
            cmd.args([
                "r",
                "-H",
                file.to_str().unwrap(),
                "-f",
                "1",
                "-q",
                "du",
                "-hs",
                "/data",
            ]);
            cmd
        });

        assert_eq!(out.code, 0, "{}", out.stderr);
        assert_eq!(
            harness.command_for("node01"),
            "du -hs /data",
            "the command is what follows the options"
        );
        assert!(
            !out.stdout.contains("42G"),
            "-q before the command is rshx's quiet: {:?}",
            out.stdout
        );
    });
}

#[test]
fn a_command_whose_first_word_starts_with_a_hyphen_needs_the_separator() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", THREE);
        harness.respond_default(Response::ok());

        // Nothing sensible begins with a hyphen, so this is refused loudly
        // rather than handed to the Host as a command of rshx's options.
        let refused = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "run", "-la", "/tmp"]);
            cmd
        });
        assert_eq!(refused.code, 5, "{}", refused.stderr);
        assert!(
            harness.invocations().is_empty(),
            "nothing runs when the command is refused"
        );

        // `--` is what says the next word is the command, whatever it looks
        // like; it stays optional everywhere else.
        let forwarded = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "run", "--", "-la", "/tmp"]);
            cmd
        });
        assert_eq!(forwarded.code, 0, "{}", forwarded.stderr);
        assert_eq!(harness.command_for("node01"), "-la /tmp");
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

#[test]
fn the_short_spellings_are_the_same_run() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", THREE);
        harness.respond_default(Response::ok());

        // The long spelling, to compare against.
        let long = run_on_tty_answering(
            {
                let mut cmd = harness.rshx();
                cmd.args([
                    "-H",
                    file.to_str().unwrap(),
                    "--privilege",
                    "--timeout",
                    "30s",
                    "--json",
                    "run",
                    "--",
                    "du",
                    "-hs",
                    "/data",
                ]);
                cmd
            },
            &[Typed::now("hunter2\n")],
        );
        assert_eq!(long.code, 0, "the long spelling runs: {}", long.stderr);
        let long_command = harness.command_for("node03");

        // The same run, in the spellings the help advertises: `r` for `run`,
        // and a letter for each option.
        let short = run_on_tty_answering(
            {
                let mut cmd = harness.rshx();
                cmd.args([
                    "-H",
                    file.to_str().unwrap(),
                    "-p",
                    "-t",
                    "30s",
                    "-j",
                    "r",
                    "--",
                    "du",
                    "-hs",
                    "/data",
                ]);
                cmd
            },
            &[Typed::now("hunter2\n")],
        );

        assert_eq!(short.code, long.code, "stderr: {}", short.stderr);
        assert_eq!(
            harness.commands().last().map(|(_, text)| text.clone()),
            Some(long_command),
            "the alias ran the same command on the Host"
        );
        assert_eq!(
            short.stdout_lines().len(),
            long.stdout_lines().len(),
            "the alias reported one line per Host: {:?}",
            short.stdout
        );
        assert!(
            short.stdout.contains("\"status\":\"ok\""),
            "-j is the json report: {:?}",
            short.stdout
        );
    });
}

#[test]
fn the_list_and_ping_aliases_answer_the_same_thing() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", THREE);
        harness.respond_default(Response::ok());

        let named = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "list"]);
            cmd
        });
        let aliased = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "ls"]);
            cmd
        });
        assert_eq!(aliased.code, 0, "stderr: {}", aliased.stderr);
        assert_eq!(
            aliased.stdout, named.stdout,
            "`ls` is `list`: a listing contacts nothing, so it is the same output"
        );

        let pinged = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "p"]);
            cmd
        });
        assert_eq!(pinged.code, 0, "stderr: {}", pinged.stderr);
        assert_eq!(
            harness.command_for("node01"),
            "echo pong",
            "`p` is `ping`: it runs the ping command on the Host"
        );
    });
}
