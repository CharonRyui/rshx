use std::{process::Stdio, time::Duration};

use anyhow::Result;

use crate::{
    cli::{CliCommand, CliOptions},
    host::Host,
    interrupt::Interrupt,
    privilege,
    report::Reporter,
    run::{Outcome, Prompts, construct_ssh_basic_cmd, execute_on_hosts, run_remote_command},
};

pub(super) fn generate_command(
    options: &CliOptions,
    subcommand: &CliCommand,
) -> Result<Vec<String>> {
    let command = match &subcommand {
        CliCommand::Run(args) => {
            if !args.command.is_empty() {
                if options.privilege {
                    // A command that runs sudo itself gets rshx's sudo in front of it and
                    // elevates a second time: rshx reads none of the command's own options —
                    // that would mean knowing sudo's grammar — so it warns rather than guesses.
                    privilege::under_sudo(&args.command)
                } else {
                    args.command.clone()
                }
            } else {
                unreachable!()
            }
        }
        CliCommand::Ping => {
            vec!["echo".to_string(), "pong".to_string()]
        }
    };
    Ok(command)
}

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
