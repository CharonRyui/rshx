//! Running the command on Hosts, listing the Hosts a run would select, and
//! deciding what a run's outcome means.
//!
//! This module is the run itself: it reads the command line, picks the Hosts,
//! and schedules them. Around it, one module per concern — `outcome` is what a
//! Host's command did, `ssh` runs one Host's ssh to that outcome, `stream` reads
//! its two streams, `prompt` answers the password a remote sudo asks for, and
//! `detach` is what a detached launch does differently.
use std::rc::Rc;
use std::time::Instant;

use anyhow::Result;
use futures_util::StreamExt;

use crate::EXIT_LOCAL;
use crate::cli::{Cli, CliCommand, CliOptions};
use crate::heartbeat::{self, Heartbeat};
use crate::host::{self, Host};
use crate::interrupt::Interrupt;
use crate::privilege;
use crate::report::{self, Reporter};
use crate::run::command::execute_command;
use crate::run::script::execute_local_script;

mod command;
mod detach;
mod list;
mod outcome;
mod prompt;
mod script;
mod ssh;
pub(crate) mod stream;

pub use outcome::{Outcome, Status, exit_code};

/// Loads the host file, then answers the subcommand the command line asked for.
pub async fn execute(cli: &Cli) -> Result<u8> {
    let options = &cli.options;
    let located = host::locate(options.host_file.as_deref())?;
    // A file rshx found for itself is named before anything runs: the report is
    // about Hosts, and which host file they came from is not in it.
    if located.found {
        report::host_file_found(&located.path, options.color);
    }
    let file = host::load(&located.path)?;
    let selected = file.select(&options.groups)?;

    match &cli.sub_command {
        // `list` answers from the host file alone: nothing runs, so no Host has
        // an outcome, and a listing is not a report.
        CliCommand::List => list::execute(&selected, options),

        CliCommand::Run(args) => {
            let detach = args.detach;
            let mut reporter = reporter(options);

            // A script is copied to each Host and run there.
            if let Some(script_path) = &args.script {
                return execute_local_script(
                    &selected,
                    script_path,
                    detach,
                    options,
                    &mut reporter,
                )
                .await;
            }

            // A command is handed to each Host's own ssh to run.
            let command = detach::command(&args.command, options.privilege, detach);

            // `--privilege` wraps the command once, before anything runs, so every Host
            // gets that same command: a remote sudo reads its password from stdin.
            // Before the heading, so what rshx says about the command is read first.
            // Asked of the command as it was typed: `command` is already wrapped, and
            // its own leading `sudo` is rshx's.
            if options.privilege && privilege::runs_sudo(&args.command) {
                reporter.warning(
                    "the command runs sudo itself; --privilege puts its own sudo in front of it",
                );
            }

            execute_command(&selected, &command, detach, options, &mut reporter).await
        }

        CliCommand::Ping => {
            let mut reporter = reporter(options);
            // A ping is a run like any other: it names its command on the Host
            // too, so a Host cut short is stopped the same way.
            execute_command(
                &selected,
                &["echo".to_string(), "pong".to_string()],
                false,
                options,
                &mut reporter,
            )
            .await
        }
    }
}

/// The reporter a run writes through. Chrome goes on stderr, and only when a
/// terminal is watching it: a redirected stderr must stay free of it, and
/// `--json` must be parseable.
fn reporter(options: &CliOptions) -> Reporter {
    let chrome = !options.json && std::io::IsTerminal::is_terminal(&std::io::stderr());
    Reporter::new(
        options.color,
        report::Detail {
            stdout: !options.quiet,
            stderr: options.stderr,
        },
        if options.json {
            report::Format::Json
        } else {
            report::Format::Plain
        },
        chrome,
    )
}

/// Runs every selected Host, at most `fanout` of them at a time, reporting each
/// one as it settles. `exit` turns their outcomes into the run's exit code: a
/// run rshx could not ask for a password exits as a local failure instead, since
/// the Hosts it cancelled never got to fail.
async fn execute_on_hosts<F>(
    selected: &Vec<&Host>,
    options: &CliOptions,
    reporter: &mut Reporter,
    per_host: F,
    exit: fn(&[Outcome]) -> u8,
) -> Result<u8>
where
    F: AsyncFn(&Host, &Interrupt, Option<prompt::Prompts>) -> Outcome,
{
    let started = Instant::now();
    let total = selected.len();

    let (prompts, mut asks) = prompt::init(options.privilege);

    let heartbeat = Rc::new(Heartbeat::new(total, reporter.chrome()));
    let interrupt = Interrupt::install();
    let per_host = Rc::new(per_host);

    // The pdsh sliding window: at most `fanout` remote commands in flight, each
    // one that finishes replaced by a pending Host.
    let mut settling = futures_util::stream::iter(selected)
        .map(|host| {
            let interrupt = interrupt.clone();
            let heartbeat = Rc::clone(&heartbeat);
            let prompts = prompts.clone();
            let per_host = Rc::clone(&per_host);
            async move {
                // Checked here, not on entry, so a Host waiting for a slot
                // does not start after an interrupt — and is not reported.
                if interrupt.is_stopped() {
                    return None;
                }
                heartbeat.start(&host.name);
                Some(per_host(host, &interrupt, prompts).await)
            }
        })
        .buffer_unordered(options.fanout as usize);

    let mut ticker = tokio::time::interval(heartbeat::TICK);
    // A missed tick means the run was busy, not that it owes several redraws.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut outcomes = Vec::with_capacity(total);
    let mut not_started = 0;
    // Why the run could not carry on: nowhere to ask, or nothing typed.
    let mut fatal = None;
    loop {
        tokio::select! {
            next = settling.next() => match next {
                Some(Some(outcome)) => {
                    heartbeat.finish(&outcome);
                    heartbeat.suspend(|| reporter.outcome(&outcome));
                    outcomes.push(outcome);
                }
                Some(None) => not_started += 1,
                None => break,
            },
            // A Host is asking for a password. Answered in this task, which
            // owns the terminal and the heartbeat: one prompt for the run.
            request = prompt::next_request(&mut asks) => match request {
                Some(request) => {
                    // A stopping run has nobody to type for it, and dropping
                    // the receiver releases the Hosts still waiting.
                    if interrupt.is_stopped() {
                        asks = None;
                        continue;
                    }
                    let prompts = prompts.as_ref().expect("--privilege built one");
                    let asking = prompts.serve(request, &interrupt, &heartbeat).await;
                    if let prompt::Asking::Done { reason } = asking {
                        asks = None;
                        fatal = reason;
                    }
                }
                // Every Host that could ask is gone.
                None => asks = None,
            },
            _ = ticker.tick() => heartbeat.tick(),
        }
    }
    drop(settling);
    heartbeat.clear();

    if let Some(reason) = &fatal {
        eprintln!("rshx: {reason}");
    }
    reporter.summary(&outcomes, not_started, started.elapsed());
    Ok(match fatal {
        // A run rshx could not ask for a password is a local failure, not a
        // Host's: the Hosts it cancelled never got to fail.
        Some(_) => EXIT_LOCAL,
        None => exit(&outcomes),
    })
}
