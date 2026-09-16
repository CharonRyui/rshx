use std::path::PathBuf;

use clap::Parser;

/// Run one command on many hosts over ssh.
#[derive(Debug, Parser)]
#[command(name = "rshx", version, about, max_term_width = 100)]
pub struct Cli {
    /// The host file listing the hosts to run on.
    #[arg(short = 'H', long, value_name = "FILE")]
    pub host_file: Option<PathBuf>,

    /// How many remote commands to run at once. Each command that finishes is
    /// replaced by a pending one.
    #[arg(short = 'f', long, value_name = "N", default_value_t = 32, value_parser = clap::value_parser!(u32).range(1..))]
    pub fanout: u32,

    /// Run only the Hosts these host groups select. Repeatable, and each value
    /// may be a comma-separated list. Defaults to every Host in the host file.
    #[arg(
        short = 'g',
        long,
        value_name = "GROUP",
        value_delimiter = ',',
        num_args = 1..
    )]
    pub groups: Vec<String>,

    /// Hide each Host's stdout. Without it every Host's output is shown,
    /// folded onto its status line when it is a single short line.
    #[arg(long, short = 'q')]
    pub quiet: bool,

    /// Show the stderr of a Host that is `ok`. A Host that is not `ok` always
    /// shows its stderr.
    #[arg(long)]
    pub stderr: bool,

    /// How long to wait for any one Host before giving up on it, such as
    /// `30s` or `5m`. Without it there is no limit. A Host that times out is
    /// reported as `timeout` and its ssh is terminated; the remote command is
    /// not stopped, and every other Host carries on.
    #[arg(long, value_name = "DURATION", value_parser = humantime::parse_duration)]
    pub timeout: Option<std::time::Duration>,

    /// Write one JSON object per Host, one per line, instead of the plain
    /// report. stdout carries nothing else, and the heartbeat is off.
    #[arg(long)]
    pub json: bool,

    /// When to colour the report.
    #[arg(long, value_enum, default_value_t = crate::report::ColorWhen::Auto)]
    pub color: crate::report::ColorWhen,

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
