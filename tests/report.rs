//! The readable per-host report: one line per Host, with the chrome on stderr.

mod support;

use support::{Attach, Response, run, run_on_tty, with_harness};

const ONE: &str = "[[hosts]]\nname = \"node01\"\n";
const TWO: &str = "[[hosts]]\nname = \"node01\"\n\n[[hosts]]\nname = \"node02\"\n";
const THREE: &str = "[[hosts]]\nname = \"node01\"\n\n[[hosts]]\nname = \"node02\"\n\n[[hosts]]\nname = \"node03\"\n";

/// A command line with the given extra flags.
fn rshx(
    harness: &support::Harness,
    file: &std::path::Path,
    flags: &[&str],
) -> std::process::Command {
    let mut cmd = harness.rshx();
    cmd.args(["-H", file.to_str().unwrap(), "-f", "1"]);
    cmd.args(flags);
    cmd
}

/// A command line with a terminal that can render colour.
///
/// `TERM` is pinned rather than inherited, so a colour assertion cannot depend
/// on the terminal the suite happens to be run from. It matters twice over: a
/// terminal that cannot render colour switches the colour off, and one that can
/// switches the heartbeat on. The tests below therefore attach one stream at a
/// time, so the heartbeat's erase sequences on stderr cannot land on the same
/// surface as the colour being asserted about.
fn on_terminal(
    harness: &support::Harness,
    file: &std::path::Path,
    flags: &[&str],
) -> std::process::Command {
    let mut cmd = rshx(harness, file, flags);
    cmd.env("TERM", "xterm-256color");
    cmd
}

#[test]
fn a_quiet_host_prints_one_line_and_none_of_its_stdout() {
    with_harness(|harness| {
        harness.respond_default(Response::ok().stdout("42G /data\n"));
        let file = harness.write("hosts.toml", ONE);

        let out = run(rshx(harness, &file, &["-q", "--", "du -hs /data"]));

        assert_eq!(out.code, 0, "{}", out.stderr);
        assert_eq!(out.stdout_lines().len(), 1, "{:?}", out.stdout);
        assert!(out.stdout.starts_with("node01 ok "), "{:?}", out.stdout);
        assert!(
            !out.stdout.contains("42G"),
            "a quiet Host's output is not printed: {:?}",
            out.stdout
        );
    });
}

#[test]
fn a_one_line_stream_folds_onto_the_status_line() {
    with_harness(|harness| {
        harness.respond_default(Response::ok().stdout("42G /data\n"));
        let file = harness.write("hosts.toml", TWO);

        let out = run(rshx(harness, &file, &["--", "du -hs /data"]));

        assert_eq!(out.code, 0, "{}", out.stderr);
        let lines = out.stdout_lines();
        assert_eq!(lines.len(), 2, "still one line per Host: {:?}", lines);
        for line in lines {
            assert!(
                line.contains(" ok ") && line.ends_with("42G /data"),
                "the single line folds onto the status line: {line:?}"
            );
        }
    });
}

#[test]
fn a_multi_line_stream_becomes_an_indented_block() {
    with_harness(|harness| {
        harness.respond_default(Response::ok().stdout("first\nsecond\nthird\n"));
        let file = harness.write("hosts.toml", ONE);

        let out = run(rshx(harness, &file, &["--", "printf"]));

        assert_eq!(out.code, 0, "{}", out.stderr);
        let lines = out.stdout_lines();
        assert_eq!(lines.len(), 4, "{:?}", lines);
        assert!(lines[0].starts_with("node01 ok "), "{:?}", lines[0]);
        assert_eq!(
            &lines[1..],
            ["  first", "  second", "  third"],
            "the block is indented so it stays attached to its Host"
        );
    });
}

#[test]
fn a_trailing_newline_does_not_make_a_block_of_one_line() {
    with_harness(|harness| {
        harness.respond_default(Response::ok().stdout("single\n"));
        let file = harness.write("hosts.toml", ONE);

        let out = run(rshx(harness, &file, &["--", "hostname"]));
        assert_eq!(out.stdout_lines().len(), 1, "{:?}", out.stdout);
    });
}

#[test]
fn a_host_that_is_not_ok_always_shows_its_stderr() {
    with_harness(|harness| {
        harness.respond(
            "node01",
            Response::failed(1).stderr("bash: nope: command not found\n"),
        );
        let file = harness.write("hosts.toml", ONE);

        let out = run(rshx(harness, &file, &["--", "nope"]));

        assert_eq!(out.code, 2);
        assert!(
            out.stdout.contains("  bash: nope: command not found"),
            "the reason is shown without being asked for: {:?}",
            out.stdout
        );
    });
}

#[test]
fn an_unreachable_host_shows_its_stderr_too() {
    with_harness(|harness| {
        harness.respond(
            "node01",
            Response::unreachable()
                .stderr("ssh: connect to host node01 port 22: Connection refused\n"),
        );
        let file = harness.write("hosts.toml", ONE);

        let out = run(rshx(harness, &file, &["--", "hostname"]));

        assert_eq!(out.code, 4, "{}", out.stderr);
        assert!(
            out.stdout.contains("  ssh: connect to host node01 port 22"),
            "an unreachable Host shows why, without --stderr: {:?}",
            out.stdout
        );
        assert!(
            out.stdout.contains("(connect)"),
            "and the cause sits next to it: {:?}",
            out.stdout
        );
    });
}

#[test]
fn a_multi_line_stderr_becomes_an_indented_block() {
    with_harness(|harness| {
        harness.respond(
            "node01",
            Response::failed(1).stderr("first problem\nsecond problem\n"),
        );
        let file = harness.write("hosts.toml", ONE);

        let out = run(rshx(harness, &file, &["--", "x"]));

        assert_eq!(out.code, 2, "{}", out.stderr);
        let lines: Vec<&str> = out.stdout.lines().collect();
        assert_eq!(
            lines.len(),
            3,
            "a status line and one line per stderr line: {lines:?}"
        );
        assert!(lines[0].starts_with("node01 failed"), "{lines:?}");
        assert!(
            lines[1].starts_with("  first problem") && lines[2].starts_with("  second problem"),
            "both lines are indented under their Host: {lines:?}"
        );
    });
}

#[test]
fn stderr_flag_shows_an_ok_hosts_stderr() {
    with_harness(|harness| {
        harness.respond_default(Response::ok().stderr("a warning\n"));
        let file = harness.write("hosts.toml", ONE);

        let silent = run(rshx(harness, &file, &["--", "hostname"]));
        assert!(
            !silent.stdout.contains("a warning"),
            "an ok Host's stderr is hidden by default: {:?}",
            silent.stdout
        );

        let loud = run(rshx(harness, &file, &["--stderr", "--", "hostname"]));
        assert!(
            loud.stdout.contains("  a warning"),
            "--stderr shows it: {:?}",
            loud.stdout
        );
    });
}

#[test]
fn the_cause_appears_for_failures_and_never_changes_the_outcome() {
    with_harness(|harness| {
        harness.respond(
            "node01",
            Response::unreachable()
                .stderr("ssh: connect to host 127.0.0.1 port 22: Connection refused\n"),
        );
        harness.respond(
            "node02",
            Response::unreachable().stderr("charon@host: Permission denied (publickey).\n"),
        );
        let file = harness.write("hosts.toml", TWO);

        let out = run(rshx(harness, &file, &["--", "hostname"]));

        assert!(
            out.stdout.contains("(connect)"),
            "the cause is shown next to the status: {:?}",
            out.stdout
        );
        assert!(out.stdout.contains("(auth)"), "{:?}", out.stdout);
        assert_eq!(out.code, 4, "both are still unreachable: {}", out.stderr);
        assert!(
            out.stderr.contains("2 unreachable"),
            "the summary counts statuses, not causes: {}",
            out.stderr
        );
    });
}

#[test]
fn a_failure_with_no_recognisable_text_gets_no_cause() {
    with_harness(|harness| {
        harness.respond(
            "node01",
            Response::failed(1).stderr("something odd happened\n"),
        );
        let file = harness.write("hosts.toml", ONE);

        let out = run(rshx(harness, &file, &["--", "x"]));

        assert_eq!(out.code, 2, "still failed: {}", out.stderr);
        assert!(
            !out.stdout.contains('('),
            "no cause is invented: {:?}",
            out.stdout
        );
    });
}

#[test]
fn a_dns_failure_gets_the_dns_cause() {
    with_harness(|harness| {
        harness.respond(
            "node01",
            Response::unreachable()
                .stderr("ssh: Could not resolve hostname node01: Name or service not known\n"),
        );
        let file = harness.write("hosts.toml", ONE);

        let out = run(rshx(harness, &file, &["--", "hostname"]));

        assert_eq!(out.code, 4, "{}", out.stderr);
        assert!(
            out.stdout.contains("(dns)"),
            "all three causes are reachable: {:?}",
            out.stdout
        );
        assert!(
            !out.stdout.contains("(auth)") && !out.stdout.contains("(connect)"),
            "and only the matching one is named: {:?}",
            out.stdout
        );
    });
}

#[test]
fn the_cause_comes_from_stderr_not_stdout() {
    with_harness(|harness| {
        // The command failed, and its stdout happens to contain ssh's own
        // connection-failure wording. The cause must not be read from it.
        harness.respond(
            "node01",
            Response::failed(1)
                .stdout("ssh: connect to host node01 port 22: Connection refused\n")
                .stderr("bash: nope: command not found\n"),
        );
        let file = harness.write("hosts.toml", ONE);

        let out = run(rshx(harness, &file, &["--", "x"]));

        assert_eq!(out.code, 2, "{}", out.stderr);
        assert!(
            out.stdout.contains("  ssh: connect to host node01 port 22"),
            "the stdout is shown by default: {:?}",
            out.stdout
        );
        assert!(
            !out.stdout.contains("(connect)"),
            "but its text is not read for a cause: {:?}",
            out.stdout
        );
    });
}

#[test]
fn a_terminal_gets_colour_and_a_pipe_does_not() {
    with_harness(|harness| {
        harness.respond_default(Response::ok());
        let file = harness.write("hosts.toml", ONE);

        let piped = run(rshx(harness, &file, &["--", "hostname"]));
        assert!(
            !piped.stdout.contains('\u{1b}'),
            "a pipe gets no colour: {:?}",
            piped.stdout
        );

        let terminal = run_on_tty(
            on_terminal(harness, &file, &["--", "hostname"]),
            Attach::STDOUT_ONLY,
        );
        assert!(
            terminal.stdout.contains('\u{1b}'),
            "a terminal gets colour under --color auto: {:?}",
            terminal.stdout
        );
    });
}

#[test]
fn no_color_is_honoured_on_a_terminal() {
    with_harness(|harness| {
        harness.respond_default(Response::ok());
        let file = harness.write("hosts.toml", ONE);

        let plain = run_on_tty(
            on_terminal(harness, &file, &["--", "hostname"]),
            Attach::STDOUT_ONLY,
        );
        assert!(plain.stdout.contains('\u{1b}'), "{:?}", plain.stdout);

        let out = run_on_tty(
            {
                let mut cmd = on_terminal(harness, &file, &["--", "hostname"]);
                cmd.env("NO_COLOR", "1");
                cmd
            },
            Attach::STDOUT_ONLY,
        );
        assert!(
            !out.stdout.contains('\u{1b}'),
            "NO_COLOR turns colour off: {:?}",
            out.stdout
        );
    });
}

#[test]
fn color_never_removes_colour_on_a_terminal() {
    with_harness(|harness| {
        harness.respond_default(Response::ok());
        let file = harness.write("hosts.toml", ONE);

        let out = run_on_tty(
            on_terminal(harness, &file, &["--color", "never", "--", "hostname"]),
            Attach::STDOUT_ONLY,
        );

        assert_eq!(out.code, 0);
        assert!(!out.stdout.contains('\u{1b}'), "{:?}", out.stdout);
    });
}

#[test]
fn an_explicit_color_always_beats_no_color() {
    with_harness(|harness| {
        harness.respond_default(Response::ok());
        let file = harness.write("hosts.toml", ONE);

        // NO_COLOR is a default, not an override of an explicit request.
        let out = run({
            let mut cmd = rshx(harness, &file, &["--color", "always", "--", "hostname"]);
            cmd.env("NO_COLOR", "1");
            cmd
        });

        assert!(
            out.stdout.contains('\u{1b}'),
            "--color always is explicit and wins: {:?}",
            out.stdout
        );
    });
}

#[test]
fn stdout_chrome_is_coloured_on_a_terminal() {
    with_harness(|harness| {
        harness.respond_default(Response::ok());
        let file = harness.write("hosts.toml", ONE);

        // stdout alone on the terminal, so the escape sequences cannot have
        // come from the stderr summary or from the heartbeat.
        let out = run_on_tty(
            on_terminal(harness, &file, &["--", "hostname"]),
            Attach::STDOUT_ONLY,
        );

        assert_eq!(out.code, 0);
        assert!(
            out.stdout.contains('\u{1b}'),
            "the result line's status is coloured: {:?}",
            out.stdout
        );
        assert!(
            out.stdout.contains("ok"),
            "and the text is still there: {:?}",
            out.stdout
        );
    });
}

#[test]
fn stdout_and_stderr_are_judged_separately() {
    with_harness(|harness| {
        harness.respond_default(Response::ok());
        let file = harness.write("hosts.toml", ONE);

        // stdout is a pipe and stderr is a terminal: the terminal stream is
        // still coloured, which one global answer could not produce.
        let out = run_on_tty(
            on_terminal(harness, &file, &["--", "hostname"]),
            Attach::STDERR_ONLY,
        );

        assert_eq!(out.code, 0);
        assert!(
            !out.stdout.contains('\u{1b}'),
            "the piped stdout stays clean: {:?}",
            out.stdout
        );
        assert!(
            out.stderr.contains('\u{1b}'),
            "the terminal stderr is still coloured: {:?}",
            out.stderr
        );
    });
}

#[test]
fn an_unknown_color_value_is_a_usage_error() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", ONE);
        let out = run(rshx(harness, &file, &["--color", "sometimes", "--", "x"]));
        assert_eq!(out.code, 5, "{}", out.stderr);
    });
}

#[test]
fn an_empty_stream_does_not_add_a_block() {
    with_harness(|harness| {
        harness.respond_default(Response::ok());
        let file = harness.write("hosts.toml", ONE);

        let out = run(rshx(harness, &file, &["--stderr", "--", "true"]));

        assert_eq!(
            out.stdout_lines().len(),
            1,
            "nothing to show means no extra lines: {:?}",
            out.stdout
        );
    });
}

#[test]
fn the_heading_names_the_command_and_only_appears_on_a_terminal() {
    with_harness(|harness| {
        harness.respond_default(Response::ok());
        let file = harness.write("hosts.toml", TWO);

        let terminal = run_on_tty(
            on_terminal(harness, &file, &["--", "du -hs /data"]),
            Attach::STDERR_ONLY,
        );
        assert!(
            terminal.stderr.contains("du -hs /data"),
            "the heading names the command that ran: {:?}",
            terminal.stderr
        );
        assert!(
            terminal.stderr.contains("2 hosts"),
            "and how many Hosts it is running on: {:?}",
            terminal.stderr
        );

        // Chrome, not report: a redirected stderr is a file nobody is watching,
        // and stdout must stay one line per Host either way.
        let piped = run(rshx(harness, &file, &["--", "du -hs /data"]));
        assert!(
            !piped.stdout.contains("du -hs /data") && !piped.stderr.contains("du -hs /data"),
            "a redirected run gets no heading: {:?} / {:?}",
            piped.stdout,
            piped.stderr
        );
    });
}

#[test]
fn a_mixed_run_aligns_the_columns_under_the_longest_status() {
    with_harness(|harness| {
        harness.respond("node01", Response::ok());
        harness.respond("node02", Response::failed(1));
        harness.respond("node03", Response::unreachable());
        let file = harness.write("hosts.toml", THREE);

        let out = run(rshx(harness, &file, &["--", "true"]));
        // A mixed run is a failure, by design: 2 for `failed` | 4 for
        // `unreachable`.
        assert_eq!(out.code, 6, "{}", out.stderr);

        // The duration must start in the same column whatever the status is,
        // so a run with mixed outcomes still reads as a table. Measured from
        // the status, since the Host names are all the same width here.
        let starts: Vec<usize> = out
            .stdout_lines()
            .iter()
            // A Host that is not `ok` also prints its stderr as an indented
            // block; the table is the lines that are not indented.
            .filter(|line| !line.starts_with(' '))
            .map(|line| {
                let status = ["unreachable", "failed", "ok"]
                    .into_iter()
                    .find_map(|word| line.find(word))
                    .unwrap_or_else(|| panic!("no status in {line:?}"));
                let duration = status + "unreachable".len() + 1;
                assert!(
                    line[duration..].starts_with(|c: char| c.is_ascii_digit()),
                    "the duration follows the padded status in {line:?}"
                );
                duration
            })
            .collect();
        assert_eq!(
            starts,
            vec![starts[0]; starts.len()],
            "every duration starts in the same column: {:?}",
            out.stdout
        );
    });
}
