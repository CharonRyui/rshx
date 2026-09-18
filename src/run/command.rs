use std::{process::Stdio, time::Duration};

use anyhow::Result;

use crate::{
    cli::CliOptions,
    host::Host,
    interrupt::Interrupt,
    report::Reporter,
    run::{Outcome, Prompts, construct_ssh_basic_cmd, execute_on_hosts, run_remote_command},
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
    let mut child = construct_ssh_basic_cmd(host);
    child.args(command);
    // Without `--privilege`, stdin is null so ssh cannot stop to prompt with
    // nobody there to answer; with it, stdin carries the password to sudo.
    child.stdin(if prompts.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    child.stdout(Stdio::piped()).stderr(Stdio::piped());

    run_remote_command(child, host, interrupt, limit, prompts).await
}
