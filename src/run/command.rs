use std::{process::Stdio, time::Duration};

use anyhow::Result;

use crate::{
    cli::CliOptions,
    host::Host,
    interrupt::Interrupt,
    remote,
    report::Reporter,
    run::{Outcome, Prompts, execute_on_hosts, run_remote_command},
};

pub(super) async fn execute_command(
    selected: &Vec<&Host>,
    command: &[String],
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
            run_command_on_host(host, command, options.timeout, prompts, interrupt).await
        },
    )
    .await
}

/// Runs the command on one Host, waiting at most `limit` for it.
async fn run_command_on_host(
    host: &Host,
    command: &[String],
    limit: Option<Duration>,
    prompts: Option<Prompts>,
    interrupt: &Interrupt,
) -> Outcome {
    // The command is run under a marker, so that a Host cut short can be asked
    // to stop it: killing its ssh leaves the command running on the Host.
    let marker = remote::marker(&host.name);
    let mut child = remote::ssh(host, &[]);
    child.arg(remote::marked(&marker, command));
    // Without `--privilege`, stdin is null so ssh cannot stop to prompt with
    // nobody there to answer; with it, stdin carries the password to sudo.
    child.stdin(if prompts.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    child.stdout(Stdio::piped()).stderr(Stdio::piped());

    run_remote_command(child, host, interrupt, limit, prompts, Some(&marker)).await
}
