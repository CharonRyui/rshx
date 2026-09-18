//! `list`: the Hosts a run would select, printed from the host file alone.

mod support;

use support::{Response, run, with_harness};

/// A host file with every field there is, over two clusters and a group that
/// only holds another group.
const ESTATE: &str = r#"
[[hosts]]
name = "web[01-02]"
user = "deploy"
port = 2222

[[hosts]]
name = "db01"
ip = "10.0.0.7"

[[hosts]]
name = "bastion"
unique_privilege_pass = true

[groups]
web = ["web[01-02]"]
core = ["db01", "bastion"]
web-all = ["web"]
"#;

/// Runs `rshx list` over `contents`, and returns the run.
fn listing(harness: &support::Harness, contents: &str, flags: &[&str]) -> support::Run {
    let file = harness.write("hosts.toml", contents);
    run({
        let mut cmd = harness.rshx();
        cmd.args(["-H", file.to_str().unwrap()]);
        cmd.args(flags);
        cmd.arg("list");
        cmd
    })
}

#[test]
fn every_host_is_listed_with_the_fields_it_sets() {
    with_harness(|harness| {
        let out = listing(harness, ESTATE, &[]);

        assert_eq!(out.code, 0, "{}", out.stderr);
        assert_eq!(
            out.stdout,
            "\
HOST     USER    PORT  IP        OWN-PASSWORD
web01    deploy  2222  -         -
web02    deploy  2222  -         -
db01     -       -     10.0.0.7  -
bastion  -       -     -         yes
"
        );
    });
}

#[test]
fn a_group_lists_only_the_hosts_it_selects() {
    with_harness(|harness| {
        let out = listing(harness, ESTATE, &["-g", "core"]);

        assert_eq!(out.code, 0, "{}", out.stderr);
        assert_eq!(
            out.stdout,
            "\
HOST     IP        OWN-PASSWORD
db01     10.0.0.7  -
bastion  -         yes
"
        );

        let nested = listing(harness, ESTATE, &["-g", "web-all"]);

        assert_eq!(
            nested.stdout,
            "\
HOST   USER    PORT
web01  deploy  2222
web02  deploy  2222
"
        );
    });
}

#[test]
fn a_file_of_bare_names_lists_bare_names() {
    with_harness(|harness| {
        let out = listing(harness, "[[hosts]]\nname = \"node[01-03]\"\n", &[]);

        assert_eq!(out.code, 0, "{}", out.stderr);
        assert_eq!(out.stdout, "node01\nnode02\nnode03\n");
    });
}

#[test]
fn json_lists_the_host_file_fields() {
    with_harness(|harness| {
        let out = listing(harness, ESTATE, &["--json"]);

        assert_eq!(out.code, 0, "{}", out.stderr);
        let objects: Vec<serde_json::Value> = out
            .stdout
            .lines()
            .map(|line| {
                serde_json::from_str(line)
                    .unwrap_or_else(|err| panic!("{line:?} is not JSON: {err}"))
            })
            .collect();

        assert_eq!(
            objects,
            vec![
                serde_json::json!({
                    "host": "web01",
                    "user": "deploy",
                    "port": 2222,
                    "unique_privilege_pass": false,
                }),
                serde_json::json!({
                    "host": "web02",
                    "user": "deploy",
                    "port": 2222,
                    "unique_privilege_pass": false,
                }),
                serde_json::json!({
                    "host": "db01",
                    "ip": "10.0.0.7",
                    "unique_privilege_pass": false,
                }),
                serde_json::json!({
                    "host": "bastion",
                    "unique_privilege_pass": true,
                }),
            ]
        );
    });
}

#[test]
fn listing_contacts_no_host() {
    with_harness(|harness| {
        // A response for every destination, so an ssh that should not have run
        // would still succeed, and only the record would show it.
        harness.respond_default(Response::ok());

        // The flags that shape a run are accepted and change nothing: `list`
        // runs nothing for them to shape.
        let out = listing(
            harness,
            ESTATE,
            &[
                "-g",
                "core",
                "-f",
                "4",
                "--privilege",
                "--timeout",
                "30s",
                "--stderr",
                "-q",
            ],
        );

        assert_eq!(out.code, 0, "{}", out.stderr);
        assert_eq!(harness.invocations(), Vec::<Vec<String>>::new());
        assert_eq!(out.stderr, "");
        assert!(out.stdout.contains("db01"), "{}", out.stdout);
    });
}
