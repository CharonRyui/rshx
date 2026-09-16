//! Host patterns: one bracketed range per name, expanded to many Hosts.

mod support;

use support::{Response, run, with_harness};

/// Runs `rshx` over a one-entry host file and returns the Hosts it ran on, in
/// order. The fake ssh records each destination, so the expansion is visible.
fn expand(name: &str) -> Result<Vec<String>, (i32, String)> {
    let harness = support::Harness::new();
    harness.respond_default(Response::ok());
    let file = harness.write("hosts.toml", &format!("[[hosts]]\nname = {name:?}\n"));

    let out = run({
        let mut cmd = harness.rshx();
        cmd.args(["-H", file.to_str().unwrap(), "-f", "1", "--", "hostname"]);
        cmd
    });

    let mut hosts: Vec<String> = harness
        .invocations()
        .iter()
        .filter_map(|argv| argv.get(1).cloned())
        .collect();
    hosts.dedup();

    if out.code == 0 {
        Ok(hosts)
    } else {
        Err((out.code, out.stderr))
    }
}

#[test]
fn a_pattern_expands_into_several_hosts() {
    assert_eq!(
        expand("node[01-03]").unwrap(),
        ["node01", "node02", "node03"]
    );
    assert_eq!(
        expand("node[0-3]").unwrap(),
        ["node0", "node1", "node2", "node3"]
    );
    assert_eq!(
        expand("gpu[1,3,5-7]").unwrap(),
        ["gpu1", "gpu3", "gpu5", "gpu6", "gpu7"]
    );
    assert_eq!(
        expand("rack[01-02]-eth0").unwrap(),
        ["rack01-eth0", "rack02-eth0"]
    );
}

#[test]
fn the_width_of_the_lower_bound_decides_the_padding() {
    assert_eq!(expand("n[8-11]").unwrap(), ["n8", "n9", "n10", "n11"]);
    assert_eq!(expand("n[08-11]").unwrap(), ["n08", "n09", "n10", "n11"]);
    assert_eq!(expand("n[008-011]").unwrap().len(), 4);
    assert_eq!(expand("n[008-011]").unwrap()[0], "n008");
}

#[test]
fn a_pattern_with_one_number_expands_to_one_host() {
    assert_eq!(expand("node[07]").unwrap(), ["node07"]);
    assert_eq!(expand("node[3-3]").unwrap(), ["node3"]);
}

#[test]
fn expansion_is_always_ascending() {
    let hosts = expand("node[01-12]").unwrap();
    assert_eq!(hosts.len(), 12);
    assert_eq!(hosts.first().unwrap(), "node01");
    assert_eq!(hosts.last().unwrap(), "node12");
}

#[test]
fn patterns_that_cannot_be_expressed_are_rejected() {
    for (name, expected) in [
        ("node[03-01]", "counts down"),
        ("node[01-10:2]", "not a number"),
        ("node[01-02-03]", "stride"),
        ("node[]", "empty"),
        ("node[01-]", "incomplete"),
        ("node[-01]", "incomplete"),
        ("node[01-02", "never closed"),
        ("node]01[02]", "before its `[`"),
        ("node[01][02]", "more than one"),
        ("node[a-c]", "not a number"),
        ("node[01,]", "not a number"),
        ("node[1-999999999]", "expands to more than"),
        ("node[1-40000,1-40000]", "expands to more than"),
        ("node[0-4294967295]", "expands to more than"),
    ] {
        let (code, stderr) = expand(name).unwrap_err();
        assert_eq!(code, 1, "{name:?} is a local error: {stderr}");
        assert!(
            stderr.contains(expected),
            "{name:?} should be rejected with a message mentioning {expected:?}, got: {stderr}"
        );
    }
}

#[test]
fn an_oversized_range_is_rejected_without_expanding_it() {
    // The cap has to be checked before anything is allocated: a range like
    // this one names a billion Hosts, and building them would exhaust memory
    // rather than report a typo. The cap's own boundary is unit-tested in
    // src/host.rs, where the expansion can be called directly instead of
    // through sixty thousand ssh processes.
    let (out, elapsed) = support::timed(|| expand("node[1-999999999]"));
    assert!(out.is_err());
    assert!(
        elapsed < support::ms(500),
        "rejection must not depend on the size of the range, took {elapsed:?}"
    );
}

#[test]
fn every_expanded_name_is_validated_like_a_literal_one() {
    // The prefix is what carries the illegal character, so this is the
    // expansion's fault, not the pattern syntax's. The set is the same one a
    // literal name is rejected for, reached through a pattern.
    for (name, bad) in [
        ("ro@ot[01-02]", "@"),
        ("node/[01-02]", "/"),
        ("node [01-02]", " "),
        ("-node[01-02]", "`-`"),
    ] {
        let (code, stderr) = expand(name).unwrap_err();
        assert_eq!(code, 1, "{name:?} is a local error: {stderr}");
        assert!(
            stderr.contains(bad),
            "{name:?} should be rejected with a reason mentioning {bad}: {stderr}"
        );
    }

    // The same character is fine in the middle of a literal name, so the
    // rejection is the expansion running the literal validator, not a blanket
    // rule about punctuation.
    assert_eq!(expand("node-01[1-2]").unwrap(), ["node-011", "node-012"]);
}

#[test]
fn a_pattern_and_a_literal_may_not_claim_the_same_host() {
    with_harness(|harness| {
        harness.respond_default(Response::ok());
        let file = harness.write(
            "hosts.toml",
            "[[hosts]]\nname = \"node[01-03]\"\n\n[[hosts]]\nname = \"node02\"\n",
        );

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "--", "hostname"]);
            cmd
        });

        assert_eq!(out.code, 1, "{}", out.stderr);
        assert!(
            out.stderr.contains("node02"),
            "the error names the contested Host: {}",
            out.stderr
        );
        assert!(
            out.stderr.contains("node[01-03]") && out.stderr.contains("node02"),
            "the error names both entries: {}",
            out.stderr
        );
        assert!(
            harness.invocations().is_empty(),
            "nothing runs when the host file is rejected"
        );
    });
}

#[test]
fn two_patterns_may_not_overlap() {
    with_harness(|harness| {
        harness.respond_default(Response::ok());
        let file = harness.write(
            "hosts.toml",
            "[[hosts]]\nname = \"node[01-03]\"\n\n[[hosts]]\nname = \"node[03-05]\"\n",
        );

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "--", "hostname"]);
            cmd
        });

        assert_eq!(out.code, 1, "{}", out.stderr);
        assert!(out.stderr.contains("node03"), "{}", out.stderr);
    });
}

#[test]
fn expanded_hosts_are_reported_by_their_expanded_names() {
    with_harness(|harness| {
        harness.respond_default(Response::ok());
        let file = harness.write("hosts.toml", "[[hosts]]\nname = \"node[01-03]\"\n");

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "-f", "1", "--", "hostname"]);
            cmd
        });

        assert_eq!(out.code, 0, "{}", out.stderr);
        let lines = out.stdout_lines();
        assert_eq!(lines.len(), 3);
        for (line, expected) in lines.iter().zip(["node01", "node02", "node03"]) {
            assert!(
                line.starts_with(expected),
                "{line:?} should be {expected:?}"
            );
        }
        assert!(
            !out.stdout.contains("node[01-03]"),
            "the pattern never appears in the report: {:?}",
            out.stdout
        );
    });
}

#[test]
fn a_literal_name_with_no_brackets_is_unchanged() {
    assert_eq!(expand("node01").unwrap(), ["node01"]);
    assert_eq!(expand("a.b-c_d").unwrap(), ["a.b-c_d"]);
}
