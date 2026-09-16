//! Ticket 03: the fanout sliding window.

mod support;

use support::{Response, ms, run, timed, with_harness};

fn hosts(names: &[&str]) -> String {
    names
        .iter()
        .map(|name| format!("[[hosts]]\nname = \"{name}\"\n"))
        .collect::<Vec<_>>()
        .join("\n")
}

const EIGHT: [&str; 8] = [
    "node01", "node02", "node03", "node04", "node05", "node06", "node07", "node08",
];

#[test]
fn fanout_caps_how_many_commands_run_at_once() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", &hosts(&EIGHT));
        // A serial run of eight 250ms hosts would take about 2s.
        harness.respond_default(Response::ok().delay_ms(250));

        let (out, elapsed) = timed(|| {
            run({
                let mut cmd = harness.rshx();
                cmd.args(["-H", file.to_str().unwrap(), "-f", "2", "--", "hostname"]);
                cmd
            })
        });

        assert_eq!(out.code, 0, "{}", out.stderr);
        assert_eq!(out.stdout_lines().len(), 8);
        assert!(
            harness.peak_concurrency() <= 2,
            "at most two commands run at once, saw {}",
            harness.peak_concurrency()
        );
        assert_eq!(
            harness.peak_concurrency(),
            2,
            "the bound is reached, not merely respected"
        );
        assert!(
            elapsed < ms(1600),
            "four waves of 250ms is well under a serial 2s, took {elapsed:?}"
        );
        // Eight hosts two at a time is four waves, and four waves of 250ms
        // cannot finish in less than a second. A run that started everything
        // at once would beat that, so the lower bound is what pins the bound
        // being applied rather than merely observed.
        assert!(
            elapsed >= ms(800),
            "the work was serialized into four waves, took {elapsed:?}"
        );
        assert_eq!(
            harness.leftover_concurrency(),
            0,
            "the window drains to nothing when the work runs out"
        );
        assert_eq!(harness.processes().len(), 8, "every Host ran exactly once");
    });
}

#[test]
fn fanout_one_runs_hosts_strictly_one_at_a_time() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", &hosts(&EIGHT[..3]));
        harness.respond_default(Response::ok().delay_ms(80));

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "-f", "1", "--", "hostname"]);
            cmd
        });

        assert_eq!(out.code, 0, "{}", out.stderr);
        assert_eq!(harness.peak_concurrency(), 1, "one at a time, no overlap");
    });
}

#[test]
fn a_finished_host_is_replaced_immediately() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", &hosts(&EIGHT));
        // The window is two wide. One Host holds its slot for 400ms, the other
        // for 1200ms, and the remaining six are instant. The slot freed at
        // 400ms has to be refilled then: a run that drained the window before
        // refilling it would leave that slot idle until 1200ms.
        harness.respond("node01", Response::ok().delay_ms(400));
        harness.respond("node02", Response::ok().delay_ms(1200));
        harness.respond_default(Response::ok());

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "-f", "2", "--", "hostname"]);
            cmd
        });

        assert_eq!(out.code, 0, "{}", out.stderr);
        assert_eq!(out.stdout_lines().len(), 8);

        // Which Hosts were running at the same time. Asserting on this rather
        // than on how long the run took keeps the test about the scheduler:
        // a loaded machine stretches wall-clock time without changing the
        // order anything ran in.
        let timeline = harness.timeline();
        let span = |dest: &str| -> Option<(u128, u128)> {
            let start = timeline.iter().find(|t| t.started && t.dest == dest)?.at;
            let end = timeline.iter().find(|t| !t.started && t.dest == dest)?.at;
            Some((start, end))
        };
        let (held_from, held_to) = span("node02").expect("node02 ran");
        let overlapped: Vec<&str> = EIGHT
            .iter()
            .filter(|dest| !matches!(**dest, "node01" | "node02"))
            .filter(|dest| span(dest).is_some_and(|(from, to)| from < held_to && to > held_from))
            .copied()
            .collect();
        assert!(
            !overlapped.is_empty(),
            "a Host starts in the slot node01 freed, while node02 still holds \
             its own: {timeline:?}"
        );
        assert_eq!(harness.peak_concurrency(), 2, "and the bound still holds");
    });
}

#[test]
fn results_are_reported_as_hosts_settle_not_in_host_file_order() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", &hosts(&EIGHT));
        harness.respond("node01", Response::ok().delay_ms(400));
        harness.respond_default(Response::ok().delay_ms(10));

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "-f", "8", "--", "hostname"]);
            cmd
        });

        assert_eq!(out.code, 0, "{}", out.stderr);
        let lines = out.stdout_lines();
        assert_eq!(lines.len(), 8);
        assert!(
            !lines[0].starts_with("node01"),
            "the slowest host is reported last, not first: {:?}",
            lines
        );
        assert!(
            lines[7].starts_with("node01"),
            "the slowest host settles last: {:?}",
            lines
        );
    });
}

#[test]
fn the_default_fanout_is_thirty_two() {
    with_harness(|harness| {
        let names: Vec<String> = (1..=40).map(|i| format!("node{i:02}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let file = harness.write("hosts.toml", &hosts(&names));
        // The delay has to outlast the time it takes to fork and exec the
        // window's worth of ssh children, or the first Host settles before the
        // thirty-second has started and the window never fills. Measured on a
        // loaded machine, starting 32 children spreads over ~150ms, so 600ms
        // leaves room for a machine several times busier than that.
        harness.respond_default(Response::ok().delay_ms(600));

        let (out, elapsed) = timed(|| {
            run({
                let mut cmd = harness.rshx();
                cmd.args(["-H", file.to_str().unwrap(), "--", "hostname"]);
                cmd
            })
        });

        assert_eq!(out.code, 0, "{}", out.stderr);
        assert_eq!(out.stdout_lines().len(), 40);
        assert_eq!(
            harness.peak_concurrency(),
            32,
            "forty hosts with a default fanout of 32 run in two waves"
        );
        assert!(
            elapsed < ms(2500),
            "two waves of 600ms, not forty serial, took {elapsed:?}"
        );
    });
}

#[test]
fn a_fanout_below_one_is_a_usage_error() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", &hosts(&EIGHT[..1]));

        let zero = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "-f", "0", "--", "hostname"]);
            cmd
        });
        assert_eq!(
            zero.code, 5,
            "a usage error, not a Host failure: {}",
            zero.stderr
        );
        assert!(
            harness.invocations().is_empty(),
            "nothing runs when the fanout is rejected"
        );

        let negative = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "-f", "-1", "--", "hostname"]);
            cmd
        });
        assert_eq!(negative.code, 5, "{}", negative.stderr);
    });
}

#[test]
fn exit_codes_hold_when_hosts_finish_out_of_order() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", &hosts(&EIGHT));
        harness.respond("node01", Response::ok().delay_ms(300));
        harness.respond("node02", Response::failed(3).delay_ms(200));
        harness.respond("node03", Response::unreachable().delay_ms(100));
        harness.respond_default(Response::ok().delay_ms(50));

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "-f", "8", "--", "hostname"]);
            cmd
        });

        assert_eq!(out.code, 6, "{}", out.stderr);
        let ok = out.stdout.matches(" ok ").count();
        assert_eq!(ok, 6, "{:?}", out.stdout);
        assert!(out.stdout.contains("node02 failed"), "{:?}", out.stdout);
        assert!(
            out.stdout.contains("node03 unreachable"),
            "{:?}",
            out.stdout
        );
        assert!(
            out.stderr
                .contains("8 hosts: 6 ok, 1 failed, 1 unreachable"),
            "the summary counts the same run: {}",
            out.stderr
        );

        // Each of the other exit codes, with the same out-of-order settling:
        // a slow first Host is what makes the settling order differ from the
        // file order, and the exit code must not care.
        harness.respond("node02", Response::ok());
        harness.respond("node03", Response::ok());
        let all_ok = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "-f", "8", "--", "hostname"]);
            cmd
        });
        assert_eq!(all_ok.code, 0, "{}", all_ok.stderr);

        harness.respond("node03", Response::ok());
        harness.respond("node05", Response::failed(9).delay_ms(20));
        let one_failed = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "-f", "8", "--", "hostname"]);
            cmd
        });
        assert_eq!(
            one_failed.code, 2,
            "a late-settling failure still sets the failed bit: {}",
            one_failed.stderr
        );

        harness.respond("node05", Response::ok());
        harness.respond("node06", Response::unreachable().delay_ms(20));
        let one_unreachable = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "-f", "8", "--", "hostname"]);
            cmd
        });
        assert_eq!(
            one_unreachable.code, 4,
            "and an early-settling unreachable sets the unreachable bit: {}",
            one_unreachable.stderr
        );
    });
}

#[test]
fn the_summary_reports_the_run_duration() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", &hosts(&EIGHT[..2]));
        harness.respond_default(Response::ok());

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "--", "hostname"]);
            cmd
        });

        assert!(
            out.stderr.contains(" in ") && out.stderr.trim_end().ends_with('s'),
            "the summary carries the run's wall time: {}",
            out.stderr
        );
    });
}
