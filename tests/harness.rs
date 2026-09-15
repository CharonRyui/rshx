//! Proves the test harness itself works: the fake ssh records the argv it was
//! given, replays the response scripted for its destination, and reports how
//! many copies were running at once. Every other test's evidence rests on this.

mod support;

use std::process::Command;

use support::{Harness, Response, ms, run, with_harness};

#[test]
fn the_stub_records_its_argv_and_replays_its_script() {
    let harness = Harness::new();
    harness.respond(
        "node01",
        Response::ok()
            .stdout("node01\n")
            .stderr("a warning\n")
            .code(3)
            .delay_ms(20),
    );

    let out = run({
        let mut cmd = Command::new(harness.stub());
        cmd.args(["-o", "User=root", "--", "node01", "uptime", "-p"])
            .env("RSHX_STUB_DIR", harness.path());
        cmd
    });

    assert_eq!(
        out.code, 3,
        "the scripted exit code is what the stub exits with"
    );
    assert_eq!(out.stdout, "node01\n");
    assert_eq!(out.stderr, "a warning\n");
    assert_eq!(
        harness.invocations(),
        vec![vec!["-o", "User=root", "--", "node01", "uptime", "-p"]],
        "the stub records the exact argv, including the separator"
    );
    assert_eq!(harness.peak_concurrency(), 1);
    assert_eq!(harness.leftover_concurrency(), 0);
}

#[test]
fn the_stub_falls_back_to_the_default_response() {
    let harness = Harness::new();
    harness.respond_default(Response::failed(7).stdout("default\n"));

    let out = run({
        let mut cmd = Command::new(harness.stub());
        cmd.args(["--", "unscripted"])
            .env("RSHX_STUB_DIR", harness.path());
        cmd
    });

    assert_eq!(out.code, 7);
    assert_eq!(out.stdout, "default\n");
}

#[test]
fn the_stub_reports_overlapping_invocations() {
    let harness = Harness::new();
    harness.respond_default(Response::ok().delay_ms(150));

    let children: Vec<_> = (0..3)
        .map(|i| {
            let mut cmd = Command::new(harness.stub());
            cmd.args(["--", &format!("node0{i}")])
                .env("RSHX_STUB_DIR", harness.path())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for mut child in children {
        child.wait().unwrap();
    }

    assert_eq!(
        harness.peak_concurrency(),
        3,
        "three stubs started together overlap, so the timeline sees a peak of three"
    );
    assert_eq!(harness.leftover_concurrency(), 0);
}

#[test]
fn the_stub_honours_a_per_destination_delay() {
    let harness = Harness::new();
    harness.respond("slow01", Response::ok().stdout("slow\n").delay_ms(600));
    harness.respond("node01", Response::ok().stdout("quick\n"));

    // Started at the same instant, so finishing order is the delay's doing and
    // nothing else's. A stub that ignored the scripted delay would finish in
    // start order.
    let mut slow = Command::new(harness.stub())
        .args(["--", "slow01"])
        .env("RSHX_STUB_DIR", harness.path())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut quick = Command::new(harness.stub())
        .args(["--", "node01"])
        .env("RSHX_STUB_DIR", harness.path())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let (out, elapsed) = support::timed(|| {
        use std::io::Read;
        let mut text = String::new();
        quick
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        let quick_code = quick.wait().unwrap().code();
        text.clear();
        slow.stdout
            .take()
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        let slow_code = slow.wait().unwrap().code();
        (quick_code, slow_code)
    });

    assert_eq!(
        out,
        (Some(0), Some(0)),
        "both exit with their scripted code"
    );
    assert!(
        elapsed >= ms(500),
        "the delayed destination actually waited, took {elapsed:?}"
    );
}

#[test]
fn the_harness_adds_no_dependencies_of_its_own() {
    // The harness is std plus what the crate already depends on, so it needs
    // no section of its own. A `[dev-dependencies]` entry would mean it
    // brought something new in.
    let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .expect("Cargo.toml");

    assert!(
        !manifest.contains("[dev-dependencies]") && !manifest.contains("[build-dependencies]"),
        "the harness must not add dependencies:\n{manifest}"
    );
}

#[test]
fn the_source_tree_has_no_leftover_placeholder_modules() {
    // Ticket 01 cleared a layout that had been abandoned: module directories
    // with nothing in them and a host-loading stub with no caller. Flat
    // modules are what is left.
    let src = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src"));
    let dirs: Vec<std::path::PathBuf> = std::fs::read_dir(src)
        .expect("src")
        .map(|entry| entry.expect("entry").path())
        .filter(|path| path.is_dir())
        .collect();

    assert!(
        dirs.is_empty(),
        "src/ holds no subdirectories, only modules: {dirs:?}"
    );
}

#[test]
fn the_harness_gives_rshx_a_private_path_and_config() {
    with_harness(|harness| {
        let cmd = harness.rshx();
        let path = cmd
            .get_envs()
            .find(|(k, _)| *k == "PATH")
            .map(|(_, v)| v.unwrap());
        assert!(
            path.unwrap()
                .to_string_lossy()
                .starts_with(&harness.path().display().to_string()),
            "the fake ssh comes first on PATH"
        );
        assert!(harness.path().join("xdg").is_dir());
    });
}
