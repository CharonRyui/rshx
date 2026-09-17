//! `--privilege`: running the command under a remote sudo, and answering the
//! password prompt when one Host asks for it.

mod support;

use support::{
    Response, Typed, ms, run, run_detached, run_on_tty_answering, screen, timed, with_harness,
};

const THREE: &str = r#"
[[hosts]]
name = "node01"

[[hosts]]
name = "node02"

[[hosts]]
name = "node03"
"#;

const TWO: &str = r#"
[[hosts]]
name = "node01"

[[hosts]]
name = "node02"
"#;

/// The marker rshx tells sudo to prompt with, so the ask is recognisable. The
/// fake ssh reads it back out of the argv rshx passed, so these tests fail if
/// rshx ever stops setting it.
const MARKER: &str = "rshx-password:";

fn sudo_argv(harness: &support::Harness, dest: &str) -> Vec<String> {
    harness
        .invocations()
        .into_iter()
        .find(|argv| argv.get(1).map(String::as_str) == Some(dest))
        .unwrap_or_else(|| panic!("{dest} was invoked"))
}

#[test]
fn the_command_is_rewritten_to_run_under_sudo() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", THREE);
        harness.respond_default(Response::ok());

        let out = run_on_tty_answering(
            {
                let mut cmd = harness.rshx();
                cmd.args([
                    "-H",
                    file.to_str().unwrap(),
                    "--privilege",
                    "--",
                    "du",
                    "-hs",
                    "/data",
                ]);
                cmd
            },
            &[],
        );

        assert_eq!(out.code, 0, "stderr: {}", out.stderr);
        assert_eq!(
            sudo_argv(harness, "node01"),
            vec![
                "--", "node01", "sudo", "-S", "-p", MARKER, "du", "-hs", "/data"
            ],
            "the command is run under a sudo that reads stdin and says so with rshx's prompt"
        );
        // The heading names what actually runs, not what was typed.
        let shown = screen(&out.stderr);
        assert!(
            shown.contains("sudo -S -p rshx-password: du -hs /data"),
            "the heading shows the rewritten command: {shown:?}"
        );
    });
}

#[test]
fn a_command_that_already_runs_sudo_is_wrapped_and_said_so() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", THREE);
        harness.respond_default(Response::ok());

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args([
                "-H",
                file.to_str().unwrap(),
                "--privilege",
                "--",
                "sudo",
                "-u",
                "postgres",
                "psql",
                "-c",
                "select 1",
            ]);
            cmd
        });

        // The command is wrapped whole, its own options untouched: it elevates
        // a second time inside rshx's, as root, where it authenticates nothing.
        assert_eq!(
            sudo_argv(harness, "node01"),
            vec![
                "--", "node01", "sudo", "-S", "-p", MARKER, "sudo", "-u", "postgres", "psql", "-c",
                "select 1"
            ],
            "the command's own sudo keeps every one of its options"
        );
        assert_eq!(out.code, 0, "stderr: {}", out.stderr);
        // Said out loud, because the second elevation is not what was asked for.
        assert!(
            out.stderr.contains("runs sudo itself"),
            "the nesting is warned about: {:?}",
            out.stderr
        );
    });
}

#[test]
fn the_warning_is_coloured_when_colour_is_asked_for() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", THREE);
        harness.respond_default(Response::ok());

        // The warning goes through the report's own stream, so `--color`
        // reaches it: the same run into a pipe carries the escape, and without
        // it does not.
        let coloured = run({
            let mut cmd = harness.rshx();
            cmd.args([
                "-H",
                file.to_str().unwrap(),
                "--color",
                "always",
                "--privilege",
                "--",
                "sudo",
                "uptime",
            ]);
            cmd
        });
        let warning = coloured
            .stderr
            .lines()
            .find(|line| line.contains("warning:"))
            .unwrap_or_else(|| panic!("the nesting is warned about: {:?}", coloured.stderr));
        assert!(
            warning.contains('\u{1b}'),
            "--color always colours the warning: {warning:?}"
        );

        let plain = run({
            let mut cmd = harness.rshx();
            cmd.args([
                "-H",
                file.to_str().unwrap(),
                "--privilege",
                "--",
                "sudo",
                "uptime",
            ]);
            cmd
        });
        let warning = plain
            .stderr
            .lines()
            .find(|line| line.contains("warning:"))
            .unwrap_or_else(|| panic!("the nesting is warned about: {:?}", plain.stderr));
        assert!(
            !warning.contains('\u{1b}') && warning.contains("runs sudo itself"),
            "a pipe gets the warning as plain text: {warning:?}"
        );
    });
}

#[test]
fn a_command_that_does_not_run_sudo_is_not_warned_about() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", THREE);
        harness.respond_default(Response::ok());

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "--privilege", "--", "uptime"]);
            cmd
        });

        assert_eq!(out.code, 0, "stderr: {}", out.stderr);
        assert!(
            !out.stderr.contains("warning"),
            "nothing is worth saying about a plain command: {:?}",
            out.stderr
        );
    });
}

#[test]
fn the_password_is_asked_for_once_and_shared_by_every_host() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", THREE);
        harness.respond_default(Response::ok().stdout("ok\n").prompt("hunter2"));

        let out = run_on_tty_answering(
            {
                let mut cmd = harness.rshx();
                cmd.args(["-H", file.to_str().unwrap(), "--privilege", "--", "uptime"]);
                cmd
            },
            &[Typed::now("hunter2")],
        );

        assert_eq!(out.code, 0, "stderr: {}", out.stderr);
        let shown = screen(&out.stderr);
        assert_eq!(
            shown.matches("privilege password").count(),
            1,
            "one prompt for the whole run, however many Hosts ask: {shown:?}"
        );
        assert!(
            !shown.contains("hunter2"),
            "the password is not echoed: {shown:?}"
        );
        assert!(
            !out.stdout.contains(MARKER),
            "the marker is rshx's own plumbing, never shown: {:?}",
            out.stdout
        );
        assert!(
            shown.matches(MARKER).count() == 1,
            "the only marker left is the heading naming the command: {shown:?}"
        );

        let mut passwords = harness.passwords();
        passwords.sort();
        assert_eq!(
            passwords,
            vec![
                ("node01".to_string(), "hunter2".to_string()),
                ("node02".to_string(), "hunter2".to_string()),
                ("node03".to_string(), "hunter2".to_string()),
            ],
            "every Host that asked was answered"
        );
        assert_eq!(out.stdout_lines().len(), 3, "{:?}", out.stdout);
    });
}

#[test]
fn a_host_that_needs_no_password_is_never_asked_about() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", THREE);
        // Only node02's sudo wants a password; the others are `NOPASSWD`.
        harness.respond_default(Response::ok().stdout("done\n"));
        harness.respond("node02", Response::ok().stdout("done\n").prompt("hunter2"));

        let out = run_on_tty_answering(
            {
                let mut cmd = harness.rshx();
                cmd.args([
                    "-H",
                    file.to_str().unwrap(),
                    "-f",
                    "1",
                    "--privilege",
                    "--",
                    "uptime",
                ]);
                cmd
            },
            &[Typed::now("hunter2")],
        );

        assert_eq!(out.code, 0, "stderr: {}", out.stderr);
        assert_eq!(
            harness.passwords(),
            vec![("node02".to_string(), "hunter2".to_string())],
            "only the Host that asked was written to"
        );
        assert_eq!(out.stdout_lines().len(), 3, "{:?}", out.stdout);
    });
}

#[test]
fn a_unique_host_is_asked_for_its_own_password() {
    with_harness(|harness| {
        let file = harness.write(
            "hosts.toml",
            r#"
[[hosts]]
name = "node01"
unique_privilege_pass = true

[[hosts]]
name = "node02"
"#,
        );
        harness.respond("node01", Response::ok().prompt("alpha"));
        harness.respond("node02", Response::ok().prompt("beta"));

        let out = run_on_tty_answering(
            {
                let mut cmd = harness.rshx();
                cmd.args([
                    "-H",
                    file.to_str().unwrap(),
                    "-f",
                    "1",
                    "--privilege",
                    "--",
                    "uptime",
                ]);
                cmd
            },
            &[Typed::now("alpha"), Typed::now("beta")],
        );

        assert_eq!(out.code, 0, "stderr: {}", out.stderr);
        let shown = screen(&out.stderr);
        assert!(
            shown.contains("privilege password for node01"),
            "a unique Host's prompt names it: {shown:?}"
        );
        assert_eq!(
            shown.matches("privilege password").count(),
            2,
            "a Host with a password of its own is asked, not given the run's: {shown:?}"
        );
        assert_eq!(
            harness.passwords(),
            vec![
                ("node01".to_string(), "alpha".to_string()),
                ("node02".to_string(), "beta".to_string()),
            ],
        );
    });
}

#[test]
fn a_rejected_password_is_asked_for_again() {
    with_harness(|harness| {
        // One Host, so the order of the two prompts is the order they were
        // typed in: a rejected password is asked for again.
        let file = harness.write(
            "hosts.toml",
            r#"
[[hosts]]
name = "node01"
"#,
        );
        harness.respond_default(Response::ok().prompt("right"));

        let out = run_on_tty_answering(
            {
                let mut cmd = harness.rshx();
                // `--stderr`, so the retry that was refused is visible even
                // though the Host came out `ok`.
                cmd.args([
                    "-H",
                    file.to_str().unwrap(),
                    "--stderr",
                    "--privilege",
                    "--",
                    "uptime",
                ]);
                cmd
            },
            &[Typed::now("wrong"), Typed::now("right")],
        );

        assert_eq!(
            out.code, 0,
            "stdout: {:?} stderr: {}",
            out.stdout, out.stderr
        );
        assert!(
            out.stdout.contains("Sorry, try again."),
            "sudo's own retry text is part of the Host's output: {:?}",
            out.stdout
        );
        assert!(
            !out.stdout.contains(MARKER),
            "a retried prompt is stripped too: {:?}",
            out.stdout
        );
        assert_eq!(
            harness.passwords(),
            vec![
                ("node01".to_string(), "wrong".to_string()),
                ("node01".to_string(), "right".to_string()),
            ],
            "each prompt was answered with what was typed for it"
        );
    });
}

#[test]
fn a_password_that_never_matches_fails_the_host() {
    with_harness(|harness| {
        // One Host, so the prompts are in the order they were typed.
        let file = harness.write(
            "hosts.toml",
            r#"
[[hosts]]
name = "node01"
"#,
        );
        harness.respond_default(Response::ok().prompt("right"));

        let out = run_on_tty_answering(
            {
                let mut cmd = harness.rshx();
                cmd.args(["-H", file.to_str().unwrap(), "--privilege", "--", "uptime"]);
                cmd
            },
            // sudo gives up after three, and each try is asked for again.
            &[
                Typed::now("wrong"),
                Typed::now("wrong"),
                Typed::now("wrong"),
            ],
        );

        assert_eq!(
            out.code, 2,
            "stdout: {:?} stderr: {}",
            out.stdout, out.stderr
        );
        assert!(
            out.stdout.contains("incorrect password attempts"),
            "{:?}",
            out.stdout
        );
        assert!(
            !out.stdout.contains(MARKER),
            "every prompt sudo printed was stripped: {:?}",
            out.stdout
        );
        assert_eq!(
            harness.passwords(),
            vec![
                ("node01".to_string(), "wrong".to_string()),
                ("node01".to_string(), "wrong".to_string()),
                ("node01".to_string(), "wrong".to_string()),
            ],
            "each of sudo's tries was answered"
        );
    });
}

#[test]
fn an_empty_password_stops_the_run() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", THREE);
        harness.respond_default(Response::ok().prompt("hunter2"));

        let out = run_on_tty_answering(
            {
                let mut cmd = harness.rshx();
                cmd.args(["-H", file.to_str().unwrap(), "--privilege", "--", "uptime"]);
                cmd
            },
            &[Typed::now("")],
        );

        assert_eq!(out.code, 1, "a local failure: {}", out.stderr);
        assert!(
            out.stderr.contains("empty"),
            "the reason is said plainly: {:?}",
            out.stderr
        );
        assert!(
            harness.passwords().is_empty(),
            "an empty password is never written to a Host"
        );
    });
}

#[test]
fn with_no_terminal_a_password_cannot_be_asked_for() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", THREE);
        harness.respond_default(Response::ok().prompt("hunter2"));

        // No session, no controlling terminal: `/dev/tty` is not there to open.
        let out = run_detached({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "--privilege", "--", "uptime"]);
            cmd
        });

        assert_eq!(out.code, 1, "a local failure: {}", out.stderr);
        assert!(
            out.stderr.contains("terminal"),
            "the reason names what was missing: {:?}",
            out.stderr
        );
        assert!(
            harness.passwords().is_empty(),
            "nothing is written when there was nobody to ask"
        );
    });
}

#[test]
fn a_prompt_is_not_the_hosts_time() {
    with_harness(|harness| {
        // A password of their own on every Host, so every Host waits on a
        // prompt: a shared password would only be asked for once.
        let file = harness.write(
            "hosts.toml",
            r#"
[[hosts]]
name = "node01"
unique_privilege_pass = true

[[hosts]]
name = "node02"
unique_privilege_pass = true

[[hosts]]
name = "node03"
unique_privilege_pass = true
"#,
        );
        harness.respond_default(Response::ok().delay_ms(700).prompt("hunter2"));

        let (out, elapsed) = timed(|| {
            run_on_tty_answering(
                {
                    let mut cmd = harness.rshx();
                    cmd.args([
                        "-H",
                        file.to_str().unwrap(),
                        "-f",
                        "1",
                        "--timeout",
                        "1s",
                        "--privilege",
                        "--",
                        "uptime",
                    ]);
                    cmd
                },
                // A slow typist: the prompt is answered long after the Host's
                // own second has passed.
                &[
                    Typed::after("hunter2", 1200),
                    Typed::after("hunter2", 1200),
                    Typed::after("hunter2", 1200),
                ],
            )
        });

        assert!(
            elapsed > ms(3000),
            "the run did wait for the typing: {elapsed:?}"
        );
        assert_eq!(
            out.code, 0,
            "waiting for a password is not the Host taking too long: {:?}",
            out.stdout
        );
        assert_eq!(
            out.stdout.matches("ok").count(),
            3,
            "every Host finished: {:?}",
            out.stdout
        );
    });
}

#[test]
fn another_hosts_prompt_is_not_this_hosts_time() {
    with_harness(|harness| {
        // node01 waits on a prompt; node02 asks for nothing and simply takes
        // longer than the limit. The prompt is not node02's doing, but it is
        // not its excuse either: node02 is not the Host that is blocked.
        harness.respond("node01", Response::ok().stdout("ok\n").prompt("hunter2"));
        harness.respond("node02", Response::ok().stdout("ok\n").delay_ms(3000));
        let file = harness.write("hosts.toml", TWO);

        let out = run_on_tty_answering(
            {
                let mut cmd = harness.rshx();
                cmd.args([
                    "-H",
                    file.to_str().unwrap(),
                    "-f",
                    "2",
                    "--timeout",
                    "1s",
                    "--privilege",
                    "--",
                    "uptime",
                ]);
                cmd
            },
            // Typed well after node02's own second has run out.
            &[Typed::after("hunter2", 2500)],
        );

        assert_eq!(out.code, 4, "a timeout: {}", out.stderr);
        let reported = |host: &str| {
            *out.stdout_lines()
                .iter()
                .find(|line| line.starts_with(host))
                .unwrap_or_else(|| panic!("{host} is reported: {:?}", out.stdout))
        };
        assert!(
            reported("node02").starts_with("node02 timeout"),
            "node02 timed out on its own time, not on the time node01 spent \
             waiting for a password: {:?}",
            out.stdout
        );
        assert!(
            reported("node01").starts_with("node01 ok"),
            "node01's wait for its password was not its time either: {:?}",
            out.stdout
        );
    });
}

#[test]
fn without_privilege_the_command_and_the_stdin_are_untouched() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", THREE);
        harness.respond_default(Response::ok().prompt("hunter2"));

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "--", "uptime"]);
            cmd
        });

        // A run without `--privilege` is not rewritten, and a command that asks
        // anyway is answered by nobody: it gets an empty stdin and fails.
        assert_eq!(out.code, 2, "{}", out.stderr);
        assert_eq!(
            sudo_argv(harness, "node01"),
            vec!["--", "node01", "uptime"],
            "the command is forwarded verbatim"
        );
        assert!(
            harness.passwords().is_empty(),
            "no password is written without `--privilege`"
        );
    });
}
