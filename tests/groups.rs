//! Host groups and selection: `-g`, nesting, cycles and the reserved `all`.

mod support;

use support::{Response, run, with_harness};

/// A host file describing two clusters and some groups over them.
const ESTATE: &str = r#"
[[hosts]]
name = "web[01-03]"

[[hosts]]
name = "db01"

[[hosts]]
name = "db02"

[[hosts]]
name = "cache01"

[groups]
web = ["web[01-03]"]
db = ["db01", "db02"]
prod = ["web", "db"]
everything = ["web[01-03]", "db01", "db02", "cache01"]
"#;

/// Runs rshx over `ESTATE` with the given `-g` values and returns the Hosts it
/// actually ran on, in the order the fake ssh saw them.
fn select(groups: &[&str]) -> Result<Vec<String>, (i32, String)> {
    let flags: Vec<&str> = groups.iter().flat_map(|group| ["-g", *group]).collect();
    hosts_for(&flags)
}

/// Runs rshx over `ESTATE` with the given flags, and returns the Hosts it ran
/// on, in the order the fake ssh saw them.
fn hosts_for(flags: &[&str]) -> Result<Vec<String>, (i32, String)> {
    let harness = support::Harness::new();
    harness.respond_default(Response::ok());
    let file = harness.write("hosts.toml", ESTATE);

    let mut cmd = harness.rshx();
    cmd.args(["-H", file.to_str().unwrap(), "-f", "1"]);
    cmd.args(flags);
    cmd.args(["run", "--", "hostname"]);
    let out = run(cmd);

    let hosts: Vec<String> = harness
        .invocations()
        .iter()
        .filter_map(|argv| {
            let separator = argv.iter().position(|a| a == "--")?;
            argv.get(separator + 1).cloned()
        })
        .collect();

    if out.code == 0 {
        Ok(hosts)
    } else {
        Err((out.code, out.stderr))
    }
}

#[test]
fn a_group_runs_only_the_hosts_it_selects() {
    assert_eq!(select(&["web"]).unwrap(), ["web01", "web02", "web03"]);
    assert_eq!(select(&["db"]).unwrap(), ["db01", "db02"]);
}

#[test]
fn no_group_runs_every_host() {
    assert_eq!(
        select(&[]).unwrap(),
        ["web01", "web02", "web03", "db01", "db02", "cache01"]
    );
}

#[test]
fn several_groups_are_a_union_with_each_host_once() {
    let union = select(&["web", "db"]).unwrap();
    assert_eq!(union, ["web01", "web02", "web03", "db01", "db02"]);

    // Naming the same group twice, and overlapping groups, still run each
    // Host exactly once.
    let repeated = select(&["web", "web"]).unwrap();
    assert_eq!(repeated, ["web01", "web02", "web03"]);

    let overlapping = select(&["web", "prod"]).unwrap();
    assert_eq!(overlapping, ["web01", "web02", "web03", "db01", "db02"]);
}

#[test]
fn a_comma_separated_list_is_the_same_as_repeating_the_flag() {
    assert_eq!(
        select(&["web,db"]).unwrap(),
        hosts_for(&["-g", "web", "-g", "db"]).unwrap()
    );
}

#[test]
fn a_pattern_selector_selects_every_host_it_expands_to() {
    assert_eq!(select(&["web"]).unwrap().len(), 3);
    assert_eq!(
        select(&["everything"]).unwrap(),
        ["web01", "web02", "web03", "db01", "db02", "cache01"]
    );
}

#[test]
fn child_groups_are_expanded_recursively() {
    assert_eq!(
        select(&["prod"]).unwrap(),
        ["web01", "web02", "web03", "db01", "db02"]
    );
}

#[test]
fn a_group_reachable_by_two_paths_contributes_each_host_once() {
    with_harness(|harness| {
        harness.respond_default(Response::ok());
        let file = harness.write(
            "hosts.toml",
            r#"
[[hosts]]
name = "node01"

[[hosts]]
name = "node02"

[groups]
left = ["node01"]
right = ["node01", "node02"]
both = ["left", "right"]
"#,
        );

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args([
                "-H",
                file.to_str().unwrap(),
                "-g",
                "both",
                "-f",
                "1",
                "run",
                "--",
                "hostname",
            ]);
            cmd
        });

        assert_eq!(out.code, 0, "{}", out.stderr);
        assert_eq!(
            harness.invocations().len(),
            2,
            "node01 is selected by both children but runs once: {:?}",
            harness.invocations()
        );
        assert_eq!(out.stdout_lines().len(), 2);

        // Which two, not merely how many: running node01 twice and skipping
        // node02 would also be two invocations.
        let mut ran: Vec<String> = harness
            .invocations()
            .iter()
            .filter_map(|argv| {
                let separator = argv.iter().position(|arg| arg == "--")?;
                argv.get(separator + 1).cloned()
            })
            .collect();
        ran.sort();
        assert_eq!(
            ran,
            ["node01", "node02"],
            "each selected Host ran, and the duplicate was dropped"
        );
    });
}

#[test]
fn a_cycle_among_groups_is_an_error_naming_the_cycle() {
    with_harness(|harness| {
        let file = harness.write(
            "hosts.toml",
            r#"
[[hosts]]
name = "node01"

[groups]
a = ["b"]
b = ["c"]
c = ["a"]
"#,
        );

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args([
                "-H",
                file.to_str().unwrap(),
                "-g",
                "a",
                "run",
                "--",
                "hostname",
            ]);
            cmd
        });

        assert_eq!(out.code, 1, "{}", out.stderr);
        assert!(out.stderr.contains("cycle"), "{}", out.stderr);
        for name in ["a", "b", "c"] {
            assert!(
                out.stderr.contains(name),
                "the cycle is named in full: {}",
                out.stderr
            );
        }
        assert!(harness.invocations().is_empty());
    });
}

#[test]
fn an_unknown_group_is_an_error_listing_the_known_ones() {
    with_harness(|harness| {
        let file = harness.write("hosts.toml", ESTATE);
        let out = run({
            let mut cmd = harness.rshx();
            cmd.args([
                "-H",
                file.to_str().unwrap(),
                "-g",
                "nope",
                "run",
                "--",
                "hostname",
            ]);
            cmd
        });

        assert_eq!(out.code, 1, "{}", out.stderr);
        assert!(out.stderr.contains("nope"), "{}", out.stderr);
        assert!(
            out.stderr.contains("web") && out.stderr.contains("db"),
            "the message lists what is declared: {}",
            out.stderr
        );
        assert!(harness.invocations().is_empty());
    });
}

#[test]
fn a_selector_matching_no_host_is_an_error() {
    with_harness(|harness| {
        // `web` matches nothing here, so asking for it would silently run on
        // fewer Hosts than the user meant.
        let file = harness.write(
            "hosts.toml",
            r#"
[[hosts]]
name = "db01"

[groups]
web = ["web[01-03]"]
typo = ["db0"]
"#,
        );

        for (group, missing) in [("web", "web[01-03]"), ("typo", "db0")] {
            let out = run({
                let mut cmd = harness.rshx();
                cmd.args([
                    "-H",
                    file.to_str().unwrap(),
                    "-g",
                    group,
                    "run",
                    "--",
                    "hostname",
                ]);
                cmd
            });

            assert_eq!(out.code, 1, "group {group}: {}", out.stderr);
            assert!(
                out.stderr.contains(missing),
                "group {group}: the unmatched selector is named: {}",
                out.stderr
            );
            assert!(
                out.stderr.contains("matches no Host"),
                "group {group}: {}",
                out.stderr
            );
        }
        assert!(harness.invocations().is_empty());
    });
}

#[test]
fn all_is_reserved() {
    with_harness(|harness| {
        let file = harness.write(
            "hosts.toml",
            r#"
[[hosts]]
name = "node01"

[groups]
all = ["node01"]
"#,
        );

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args([
                "-H",
                file.to_str().unwrap(),
                "-g",
                "all",
                "run",
                "--",
                "hostname",
            ]);
            cmd
        });

        assert_eq!(out.code, 1, "{}", out.stderr);
        assert!(
            out.stderr.contains("reserved"),
            "`all` cannot be redefined: {}",
            out.stderr
        );
        assert!(harness.invocations().is_empty());
    });
}

#[test]
fn all_selects_every_host_when_it_is_not_redefined() {
    assert_eq!(
        select(&["all"]).unwrap(),
        ["web01", "web02", "web03", "db01", "db02", "cache01"]
    );
}

#[test]
fn groups_are_resolved_against_the_file_that_h_chose() {
    with_harness(|harness| {
        harness.respond_default(Response::ok());
        harness.write(
            "a.toml",
            "[[hosts]]\nname = \"from-a\"\n\n[groups]\ng = [\"from-a\"]\n",
        );
        harness.write(
            "b.toml",
            "[[hosts]]\nname = \"from-b\"\n\n[groups]\ng = [\"from-b\"]\n",
        );

        for (file, expected) in [("a.toml", "from-a"), ("b.toml", "from-b")] {
            let out = run({
                let mut cmd = harness.rshx();
                cmd.args(["-H", file, "-g", "g", "run", "--", "hostname"]);
                cmd
            });
            assert_eq!(out.code, 0, "{file}: {}", out.stderr);
            assert!(
                out.stdout.contains(expected),
                "{file} resolves `g` against itself: {:?}",
                out.stdout
            );
        }
    });
}

#[test]
fn a_group_selecting_a_host_declared_by_a_pattern_selects_it_once() {
    with_harness(|harness| {
        harness.respond_default(Response::ok());
        let file = harness.write(
            "hosts.toml",
            r#"
[[hosts]]
name = "node[01-03]"

[groups]
some = ["node01", "node[02-03]"]
"#,
        );

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args([
                "-H",
                file.to_str().unwrap(),
                "-g",
                "some",
                "-f",
                "1",
                "run",
                "--",
                "hostname",
            ]);
            cmd
        });

        assert_eq!(out.code, 0, "{}", out.stderr);
        assert_eq!(
            harness.invocations().len(),
            3,
            "{:?}",
            harness.invocations()
        );
        assert!(out.stdout.contains("node01") && out.stdout.contains("node03"));
    });
}

#[test]
fn a_group_may_select_hosts_declared_by_several_entries() {
    assert_eq!(
        select(&["everything"]).unwrap(),
        ["web01", "web02", "web03", "db01", "db02", "cache01"]
    );
}
