use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Operations to execute on hosts
#[derive(Debug, Subcommand)]
pub enum CliCommand {
    /// Run command or script on hosts
    Run(CliRunArgs),
}

#[derive(Debug, Args)]
pub struct CliOptions {
    /// The host file listing the hosts to run on.
    #[arg(short = 'H', long, value_name = "FILE")]
    pub host_file: Option<PathBuf>,

    /// How many commands run at once; a finish is replaced by a pending one.
    #[arg(short = 'f', long, value_name = "N", default_value_t = 32, value_parser = clap::value_parser!(u32).range(1..))]
    pub fanout: u32,

    /// Run only the Hosts these groups select. Repeatable, each value may be
    /// comma-separated; defaults to every Host in the host file.
    #[arg(
        short = 'g',
        long,
        value_name = "GROUP",
        value_delimiter = ',',
        num_args = 1..
    )]
    pub groups: Vec<String>,

    /// Hide each Host's stdout.
    #[arg(long, short = 'q')]
    pub quiet: bool,

    /// Show an `ok` Host's stderr; one that is not `ok` always shows stderr.
    #[arg(long)]
    pub stderr: bool,

    /// Run the command as root, with `sudo`, answering its password prompt
    /// from the terminal when a Host asks. The command is wrapped whole, so an
    /// inner `sudo` keeps its own options and elevates a second time inside
    /// rshx's; a Host that needs its own password says so in the host file.
    #[arg(long)]
    pub privilege: bool,

    /// How long to wait for any one Host, such as `30s` or `5m`; without it
    /// there is no limit. A Host that times out is reported as `timeout`, its
    /// ssh is terminated, and the remote command is not stopped.
    #[arg(long, value_name = "DURATION", value_parser = humantime::parse_duration)]
    pub timeout: Option<std::time::Duration>,

    /// Write one JSON object per Host, one per line, instead of the plain
    /// report. stdout carries nothing else, and the heartbeat is off.
    #[arg(long)]
    pub json: bool,

    /// When to colour the report.
    #[arg(long, value_enum, default_value_t = crate::report::ColorWhen::Auto)]
    pub color: crate::report::ColorWhen,
}

#[derive(Debug, Args)]
pub struct CliRunArgs {
    /// Script file to run
    #[arg(long, value_name = "SCRIPT_PATH")]
    script: Option<PathBuf>,

    /// The command to run on every host, after `--`.
    #[arg(
        last = true,
        required = true,
        num_args = 1..,
        value_name = "COMMAND",
        allow_hyphen_values = true
    )]
    pub command: Vec<String>,
}

/// Run operations on many hosts over ssh.
#[derive(Debug, Parser)]
#[command(name = "rshx", version, about, max_term_width = 100)]
pub struct Cli {
    /// Cli general options
    #[command(flatten)]
    pub options: CliOptions,

    /// subcommand for operation
    #[command(subcommand)]
    pub sub_command: CliCommand,
}
