//! The progress heartbeat: one line on stderr while Hosts are in flight.

mod support;

use std::process::Command;

use support::{Attach, Response, run, run_on_tty, screen, spawn, with_harness};

const SLOW_AND_QUICK: &str = "[[hosts]]\nname = \"slow01\"\n\n[[hosts]]\nname = \"slow02\"\n\n[[hosts]]\nname = \"node01\"\n";
const ONE_SLOW: &str = "[[hosts]]\nname = \"slow01\"\n";

/// A run whose stderr is a terminal. `TERM` matters: a terminal that cannot
/// render progress is one indicatif refuses to draw on.
fn on_terminal(harness: &support::Harness, file: &std::path::Path, flags: &[&str]) -> Command {
    let mut cmd = harness.rshx();
    cmd.args(["-H", file.to_str().unwrap(), "-f", "3"]);
    cmd.args(flags);
    cmd.env("TERM", "xterm-256color");
    cmd
}

/// Two slow Hosts and a quick one, so the heartbeat has something to say.
fn script(harness: &support::Harness) {
    harness.respond("slow01", Response::ok().stdout("done\n").delay_ms(700));
    harness.respond("slow02", Response::ok().stdout("done\n").delay_ms(700));
    harness.respond("node01", Response::ok().stdout("42G /data\n"));
}

#[test]
fn a_terminal_shows_a_heartbeat_that_is_gone_when_the_run_ends() {
    with_harness(|harness| {
        script(harness);
        let file = harness.write("hosts.toml", SLOW_AND_QUICK);

        let out = run_on_tty(
            on_terminal(harness, &file, &["--", "du -hs /data"]),
            Attach::STDERR_ONLY,
        );

        assert_eq!(out.code, 0, "{}", out.stderr);
        assert!(
            out.stderr.contains("0/3 done"),
            "the heartbeat starts before anything settles: {:?}",
            out.stderr
        );
        assert!(
            out.stderr.contains("2 running, slow01"),
            "it names the Host that has been running longest: {:?}",
            out.stderr
        );

        let shown = screen(&out.stderr);
        assert!(
            !shown.contains("running") && !shown.contains("done,"),
            "the heartbeat is erased when the run ends: {shown:?}"
        );
        assert!(
            shown.contains("3 hosts: 3 ok"),
            "and the summary is what is left: {shown:?}"
        );
    });
}

#[test]
fn a_redirected_stderr_gets_no_heartbeat_and_no_escape_sequences() {
    with_harness(|harness| {
        script(harness);
        let file = harness.write("hosts.toml", SLOW_AND_QUICK);

        // A pipe, not a terminal: exactly the redirected case.
        let mut cmd = harness.rshx();
        cmd.args([
            "-H",
            file.to_str().unwrap(),
            "-f",
            "3",
            "--",
            "du -hs /data",
        ]);
        cmd.env("TERM", "xterm-256color");
        let out = run(cmd);

        assert_eq!(out.code, 0, "{}", out.stderr);
        assert!(
            !out.stderr.contains("done,") && !out.stderr.contains("running"),
            "no heartbeat is written to a pipe: {:?}",
            out.stderr
        );
        assert!(
            !out.stderr.contains('\u{1b}'),
            "and no escape sequences either: {:?}",
            out.stderr
        );
    });
}

#[test]
fn json_produces_no_heartbeat_even_on_a_terminal() {
    with_harness(|harness| {
        script(harness);
        let file = harness.write("hosts.toml", SLOW_AND_QUICK);

        let out = run_on_tty(
            on_terminal(harness, &file, &["--json", "--", "du -hs /data"]),
            Attach::STDERR_ONLY,
        );

        assert_eq!(out.code, 0, "{}", out.stderr);
        assert!(
            !out.stderr.contains("done,") && !out.stderr.contains("running"),
            "--json has no heartbeat: {:?}",
            out.stderr
        );
        // The summary may be coloured, but a heartbeat draw would leave its
        // own erase sequence behind.
        assert!(
            !out.stderr.contains("\u{1b}[2K"),
            "and nothing is drawn over a line: {:?}",
            out.stderr
        );
        // The summary is still chrome, so stderr is not empty.
        assert!(out.stderr.contains("3 hosts:"), "{}", out.stderr);
    });
}

#[test]
fn result_lines_survive_the_heartbeat_on_a_terminal() {
    with_harness(|harness| {
        script(harness);
        let file = harness.write("hosts.toml", SLOW_AND_QUICK);

        // Both streams on the terminal: the case where the heartbeat and the
        // results share a surface.
        let out = run_on_tty(
            on_terminal(harness, &file, &["--", "du -hs /data"]),
            Attach::BOTH,
        );

        let shown = screen(&out.stdout);
        for host in ["slow01", "slow02", "node01"] {
            let line = shown
                .lines()
                .find(|line| line.starts_with(host))
                .unwrap_or_else(|| panic!("{host} has no intact line in {shown:?}"));
            assert!(
                line.contains(" ok "),
                "the result line is complete, not partly overwritten: {line:?}"
            );
        }
    });
}

#[test]
fn the_counts_agree_with_the_closing_summary() {
    with_harness(|harness| {
        script(harness);
        let file = harness.write("hosts.toml", SLOW_AND_QUICK);

        let out = run_on_tty(
            on_terminal(harness, &file, &["--", "du -hs /data"]),
            Attach::STDERR_ONLY,
        );

        // The heartbeat reaches the full count, and once it has, nothing is
        // still running and the summary counts the same run. Asserted on the
        // rendered text, since the summary's counts are coloured.
        let tail = screen(
            &out.stderr[out
                .stderr
                .rfind("3/3 done")
                .expect("the heartbeat reaches 3/3")..],
        );
        assert!(!tail.contains("running"), "{tail:?}");
        assert!(
            tail.contains("3 hosts: 3 ok"),
            "the summary agrees: {tail:?}"
        );
    });
}

#[test]
fn a_single_host_is_reported_sensibly() {
    with_harness(|harness| {
        script(harness);
        let file = harness.write("hosts.toml", ONE_SLOW);

        let out = run_on_tty(
            on_terminal(harness, &file, &["--", "du -hs /data"]),
            Attach::STDERR_ONLY,
        );

        assert_eq!(out.code, 0, "{}", out.stderr);
        assert!(
            out.stderr.contains("1 running, slow01"),
            "one Host is still described: {:?}",
            out.stderr
        );
        assert!(
            out.stderr.contains("1/1 done"),
            "and its count is out of one: {:?}",
            out.stderr
        );
        assert!(
            screen(&out.stderr).contains("1 host: 1 ok"),
            "the summary says one host: {:?}",
            screen(&out.stderr)
        );
    });
}

#[test]
fn a_run_with_no_hosts_in_flight_still_ends_cleanly() {
    with_harness(|harness| {
        // Nothing to run: the heartbeat must not be left on the screen, and
        // the summary still has to appear.
        let file = harness.write("hosts.toml", ONE_SLOW);
        harness.respond("slow01", Response::ok());

        let out = run_on_tty(
            on_terminal(harness, &file, &["--", "true"]),
            Attach::STDERR_ONLY,
        );

        assert_eq!(out.code, 0, "{}", out.stderr);
        let shown = screen(&out.stderr);
        assert!(shown.contains("1 host: 1 ok"), "{shown:?}");
        assert!(!shown.contains("running"), "{shown:?}");
    });
}

#[test]
fn result_lines_are_intact_when_both_streams_go_to_one_file() {
    with_harness(|harness| {
        script(harness);
        let file = harness.write("hosts.toml", SLOW_AND_QUICK);

        // The case a shell produces with `rshx ... >log 2>&1`: results and
        // chrome share one surface, and neither may be mangled by the other.
        let log = harness.path().join("run.log");
        let handle = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&log)
            .expect("create log");
        let mut cmd = harness.rshx();
        cmd.args([
            "-H",
            file.to_str().unwrap(),
            "-f",
            "3",
            "--",
            "du -hs /data",
        ]);
        cmd.env("TERM", "xterm-256color");
        // Both streams onto the one handle, which is what `2>&1` hands the
        // process.
        cmd.stdout(std::process::Stdio::from(handle.try_clone().unwrap()))
            .stderr(std::process::Stdio::from(handle));
        let mut child = spawn(&mut cmd);
        let code = child.wait().expect("wait").code().expect("exited normally");

        assert_eq!(
            code,
            0,
            "{}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );

        let text = std::fs::read_to_string(&log).expect("read log");
        assert!(
            !text.contains('\u{1b}'),
            "a file gets no escape sequences: {text:?}"
        );
        for host in ["slow01", "slow02", "node01"] {
            let line = text
                .lines()
                .find(|line| line.starts_with(host))
                .unwrap_or_else(|| panic!("{host} has no intact line in {text:?}"));
            assert!(
                line.contains(" ok "),
                "the result line is whole, not split by the heartbeat: {line:?}"
            );
        }
        assert!(
            text.contains("3 hosts: 3 ok"),
            "and the summary is there too: {text:?}"
        );
        assert!(
            !text.contains("done,") && !text.contains("running"),
            "with no heartbeat interleaved: {text:?}"
        );
    });
}

#[test]
fn the_heartbeat_is_not_drawn_again_after_the_last_host_settles() {
    with_harness(|harness| {
        // Every Host is instant, so the run has nothing left to say once the
        // last one is reported. Nothing may draw the heartbeat again after
        // that: a redraw there means the run sat and waited for its next tick
        // before ending.
        harness.respond_default(Response::ok());
        let file = harness.write("hosts.toml", SLOW_AND_QUICK);

        // Both streams on the terminal, so the heartbeat's frames and the
        // report's lines are one stream: their order in it is their order on
        // the screen.
        let out = run_on_tty(on_terminal(harness, &file, &["--", "true"]), Attach::BOTH);

        assert_eq!(out.code, 0, "{}", out.stdout);
        // Every frame says how many Hosts are done; no line of the report ever
        // does. So the last segment that names a Host without saying `done` is
        // the last result line, and everything from there on is what the run
        // drew after its last Host had settled.
        let drawn: Vec<&str> = out.stdout.split(['\r', '\n']).collect();
        let last = drawn
            .iter()
            .rposition(|segment| {
                !segment.contains("done")
                    && ["slow01", "slow02", "node01"]
                        .iter()
                        .any(|host| segment.contains(host))
            })
            .unwrap_or_else(|| panic!("every Host is reported: {:?}", out.stdout));
        // One frame belongs there: writing a result line takes the heartbeat
        // off the line and puts it back. Anything more is the run drawing the
        // heartbeat again once it had nothing left to report — a tick it
        // waited for before ending.
        assert_eq!(
            drawn[last..]
                .iter()
                .filter(|segment| segment.contains("done"))
                .count(),
            1,
            "the heartbeat is drawn once more once the run is over: {:?}",
            out.stdout
        );
    });
}
