use std::{process::Stdio, time::Duration};

use anyhow::Result;

use crate::{
    cli::CliOptions,
    host::Host,
    interrupt::Interrupt,
    remote,
    report::Reporter,
    run::{Outcome, Prompts, execute_on_hosts, exit_code, launched, run_remote_command},
};

pub(super) async fn execute_command(
    selected: &Vec<&Host>,
    command: &[String],
    detach: bool,
    options: &CliOptions,
    reporter: &mut Reporter,
) -> Result<u8> {
    // Before the heartbeat's first draw, so the heading is not written over.
    reporter.heading(command, selected.len(), options.fanout);

    execute_on_hosts(
        selected,
        options,
        reporter,
        async |host, interrupt, prompts| {
            run_command_on_host(host, command, detach, options.timeout, prompts, interrupt).await
        },
        exit_code,
    )
    .await
}

/// Runs the command on one Host, waiting at most `limit` for it — or, for a
/// detached run, only until the Host says it has started.
async fn run_command_on_host(
    host: &Host,
    command: &[String],
    detach: bool,
    limit: Option<Duration>,
    prompts: Option<Prompts>,
    interrupt: &Interrupt,
) -> Outcome {
    let marker = remote::marker(&host.name);
    let mut child = remote::ssh(host, &[]);
    // The command is run under a marker, so that a Host cut short can be asked
    // to stop it: killing its ssh leaves the command running on the Host.
    child.arg(match detach {
        // The launcher starts the command and returns: the elevation, when
        // there is one, is the launcher's, and what the command prints goes
        // nowhere.
        true => remote::detached(&marker, &command.join(" "), prompts.is_some()),
        false => remote::marked(&marker, command),
    });
    // Without `--privilege`, stdin is null so ssh cannot stop to prompt with
    // nobody there to answer; with it, stdin carries the password to sudo.
    child.stdin(if prompts.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    child.stdout(Stdio::piped()).stderr(Stdio::piped());

    let outcome = run_remote_command(child, host, interrupt, limit, prompts, Some(&marker)).await;
    match detach {
        // What the launcher printed is the pid of the shell running the
        // command, which is what the report says is running there.
        true => launched(outcome),
        false => outcome,
    }
}
