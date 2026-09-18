//! Target overrides: `user`, `port` and `ip`, passed to ssh as `-o` options.

mod support;

use support::{Response, output, run, with_harness};

/// The argv rshx handed ssh for `name`, with the `-o` options pulled out. The
/// destination is the argument after `--`, which is where rshx always puts it.
fn options_for(harness: &support::Harness, name: &str) -> Vec<String> {
    let argv = argv_for(harness, name);
    argv.windows(2)
        .filter(|pair| pair[0] == "-o")
        .map(|pair| pair[1].clone())
        .collect()
}

fn argv_for(harness: &support::Harness, name: &str) -> Vec<String> {
    harness
        .invocations()
        .into_iter()
        .find(|argv| destination(argv) == Some(name))
        .unwrap_or_else(|| panic!("{name} was never invoked"))
}

/// The destination of an ssh invocation: the argument after the `--` separator.
fn destination(argv: &[String]) -> Option<&str> {
    let separator = argv.iter().position(|arg| arg == "--")?;
    argv.get(separator + 1).map(String::as_str)
}

#[test]
fn each_override_reaches_ssh_as_its_own_option() {
    with_harness(|harness| {
        harness.respond_default(Response::ok());
        let file = harness.write(
            "hosts.toml",
            r#"
[[hosts]]
name = "node01"
user = "alice"
port = 2200
ip = "10.0.0.1"
"#,
        );

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "run", "--", "hostname"]);
            cmd
        });

        assert_eq!(out.code, 0, "{}", out.stderr);
        assert_eq!(
            options_for(harness, "node01"),
            ["User=alice", "Port=2200", "HostName=10.0.0.1"]
        );
    });
}

#[test]
fn an_entry_with_only_a_name_adds_no_options() {
    with_harness(|harness| {
        harness.respond_default(Response::ok());
        let file = harness.write("hosts.toml", "[[hosts]]\nname = \"node01\"\n");

        run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "run", "--", "hostname"]);
            cmd
        });

        assert!(
            options_for(harness, "node01").is_empty(),
            "a bare name leaves connection configuration to ~/.ssh/config"
        );
        assert_eq!(
            harness.invocations()[0],
            vec!["--", "node01", "hostname"],
            "no stray arguments"
        );
    });
}

#[test]
fn the_destination_stays_the_name_even_when_overridden() {
    with_harness(|harness| {
        harness.respond_default(Response::ok());
        let file = harness.write(
            "hosts.toml",
            "[[hosts]]\nname = \"node01\"\nuser = \"alice\"\nport = 2200\nip = \"10.0.0.1\"\n",
        );

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "run", "--", "hostname"]);
            cmd
        });

        let argv = argv_for(harness, "node01");
        assert_eq!(
            destination(&argv),
            Some("node01"),
            "ssh still resolves the Host by its name, so ~/.ssh/config applies: {argv:?}"
        );
        assert!(
            !argv.iter().any(|a| a.contains("alice@") || a.contains('@')),
            "the user is not folded into the destination: {argv:?}"
        );
        assert!(out.stdout.contains("node01 ok"), "{:?}", out.stdout);
    });
}

#[test]
fn a_pattern_carries_user_and_port_to_every_host_it_expands_to() {
    with_harness(|harness| {
        harness.respond_default(Response::ok());
        let file = harness.write(
            "hosts.toml",
            "[[hosts]]\nname = \"gpu[01-03]\"\nuser = \"gpuadmin\"\nport = 2222\n",
        );

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

        assert_eq!(out.code, 0, "{}", out.stderr);
        for name in ["gpu01", "gpu02", "gpu03"] {
            assert_eq!(
                options_for(harness, name),
                ["User=gpuadmin", "Port=2222"],
                "{name} carries the pattern's overrides"
            );
        }
    });
}

#[test]
fn an_ip_on_a_pattern_is_rejected_with_a_reason() {
    with_harness(|harness| {
        let file = harness.write(
            "hosts.toml",
            "[[hosts]]\nname = \"gpu[01-03]\"\nip = \"10.0.0.1\"\n",
        );

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "run", "--", "hostname"]);
            cmd
        });

        assert_eq!(out.code, 1, "{}", out.stderr);
        assert!(
            out.stderr.contains("host pattern"),
            "the message says why: {}",
            out.stderr
        );
        assert!(out.stderr.contains("gpu[01-03]"), "{}", out.stderr);
        assert!(
            harness.invocations().is_empty(),
            "nothing runs when the host file is rejected"
        );
    });
}

#[test]
fn an_out_of_range_port_is_rejected() {
    with_harness(|harness| {
        for port in ["70000", "-1", "0.5"] {
            let file = harness.write(
                "hosts.toml",
                &format!("[[hosts]]\nname = \"node01\"\nport = {port}\n"),
            );
            let out = run({
                let mut cmd = harness.rshx();
                cmd.args(["-H", file.to_str().unwrap(), "run", "--", "hostname"]);
                cmd
            });
            assert_eq!(
                out.code, 1,
                "port {port} should be rejected: {}",
                out.stderr
            );
            assert!(
                out.stderr.contains("port"),
                "port {port}: the message names the field: {}",
                out.stderr
            );
        }
        assert!(harness.invocations().is_empty());
    });
}

#[test]
fn a_malformed_ip_is_rejected() {
    with_harness(|harness| {
        for ip in [
            "\"not-an-ip\"",
            "\"10.0.0.999\"",
            "\"10.0.0\"",
            "\"127.0.0.01\"",
        ] {
            let file = harness.write(
                "hosts.toml",
                &format!("[[hosts]]\nname = \"node01\"\nip = {ip}\n"),
            );
            let out = run({
                let mut cmd = harness.rshx();
                cmd.args(["-H", file.to_str().unwrap(), "run", "--", "hostname"]);
                cmd
            });
            assert_eq!(out.code, 1, "ip {ip} should be rejected: {}", out.stderr);
            assert!(out.stderr.contains("ip"), "{}", out.stderr);
        }
    });
}

#[test]
fn a_user_that_could_inject_ssh_options_is_rejected() {
    with_harness(|harness| {
        // `user` reaches ssh inside an -o value, so a newline or space there
        // would be a second config directive.
        for user in [
            r#""alice\nProxyCommand=evil""#,
            r#""alice bob""#,
            r#""alice=root""#,
            r#""""#,
            r#""alice@prod""#,
        ] {
            let file = harness.write(
                "hosts.toml",
                &format!("[[hosts]]\nname = \"node01\"\nuser = {user}\n"),
            );
            let out = run({
                let mut cmd = harness.rshx();
                cmd.args(["-H", file.to_str().unwrap(), "run", "--", "hostname"]);
                cmd
            });
            assert_eq!(
                out.code, 1,
                "user {user} should be rejected: {}",
                out.stderr
            );
            assert!(
                out.stderr.contains("user"),
                "the message names the field: {}",
                out.stderr
            );
        }
        assert!(
            harness.invocations().is_empty(),
            "an injected option never reaches ssh"
        );
    });
}

#[test]
fn the_override_does_not_change_the_reported_name() {
    with_harness(|harness| {
        harness.respond_default(Response::ok());
        let file = harness.write(
            "hosts.toml",
            "[[hosts]]\nname = \"node01\"\nuser = \"alice\"\nip = \"10.0.0.1\"\n",
        );

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "run", "--", "hostname"]);
            cmd
        });

        assert!(
            out.stdout.starts_with("node01 "),
            "the Host is reported under its name: {:?}",
            out.stdout
        );
        assert!(!out.stdout.contains("10.0.0.1"), "{:?}", out.stdout);
        assert!(!out.stdout.contains("alice"), "{:?}", out.stdout);
    });
}

#[test]
fn a_host_with_an_override_still_picks_up_the_rest_of_ssh_config() {
    // The overrides are passed as options rather than replacing the config
    // file, so ssh still reads everything else it would have read. Real ssh is
    // the judge here: -G prints the effective configuration. `-F` is used
    // because ssh resolves `~` from the passwd entry, not from `$HOME`.
    let dir = std::env::temp_dir().join(format!("rshx-override-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config");
    std::fs::write(
        &config,
        "Host node01\n    ProxyJump bastion\n    IdentityFile /nonexistent/key\n    HostName from-config\n\nHost node02\n    ProxyCommand /bin/false %h\n    HostName from-config\n",
    )
    .unwrap();

    // ssh resolves ProxyJump in preference to ProxyCommand, so the two are
    // exercised on separate Hosts rather than both on one.
    let effective = |host: &str| {
        let mut cmd = std::process::Command::new("ssh");
        cmd.arg("-F").arg(&config).args([
            "-G",
            "-o",
            "HostName=10.0.0.1",
            "-o",
            "User=alice",
            "--",
            host,
        ]);
        let out = output(&mut cmd);
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    let text = effective("node01");
    assert!(
        text.contains("proxyjump bastion"),
        "ProxyJump survives an override:\n{text}"
    );
    assert!(
        text.contains("identityfile /nonexistent/key"),
        "IdentityFile survives an override:\n{text}"
    );
    assert!(
        text.contains("user alice") && text.contains("hostname 10.0.0.1"),
        "the override wins over the config file:\n{text}"
    );
    assert!(
        !text.contains("hostname from-config"),
        "the config file's HostName does not win:\n{text}"
    );

    let text = effective("node02");
    assert!(
        text.contains("proxycommand /bin/false %h"),
        "ProxyCommand survives an override:\n{text}"
    );
    assert!(
        text.contains("hostname 10.0.0.1"),
        "and the override still wins:\n{text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rshx_passes_the_options_ssh_needs_to_keep_reading_the_config() {
    // The test above proves ssh's own behaviour. This one proves rshx invokes
    // it that way: the options are passed, the config is not replaced, and
    // nothing tells ssh to ignore it.
    with_harness(|harness| {
        harness.respond_default(Response::ok());
        let file = harness.write(
            "hosts.toml",
            "[[hosts]]\nname = \"node01\"\nuser = \"alice\"\nip = \"10.0.0.1\"\n",
        );

        let out = run({
            let mut cmd = harness.rshx();
            cmd.args(["-H", file.to_str().unwrap(), "run", "--", "hostname"]);
            cmd
        });
        assert_eq!(out.code, 0, "{}", out.stderr);

        let argv = argv_for(harness, "node01");
        let options = options_for(harness, "node01");
        assert_eq!(
            options,
            ["User=alice", "HostName=10.0.0.1"],
            "the overrides go through as options, and nothing else does"
        );
        assert!(
            !argv.iter().any(|arg| arg == "-F"),
            "no config file is substituted for the user's, so ssh reads their own: {argv:?}"
        );
        assert!(
            !argv.iter().any(|arg| arg.contains("ProxyCommand")
                || arg.contains("IdentityFile")
                || arg.contains("ProxyJump")),
            "and nothing ssh would otherwise resolve is overridden: {argv:?}"
        );
    });
}
