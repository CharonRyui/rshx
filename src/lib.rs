//! rshx: run one command on many hosts over ssh, with a bounded fanout and a
//! readable per-host report.

pub mod cause;
pub mod cli;
pub mod heartbeat;
pub mod host;
pub mod interrupt;
pub mod privilege;
pub mod report;
pub mod run;

use std::process::ExitCode;

use clap::Parser;

/// Every Host was `ok`.
pub const EXIT_OK: u8 = 0;
/// A local error: the host file is missing or malformed.
pub const EXIT_LOCAL: u8 = 1;
/// At least one Host `failed`.
pub const EXIT_FAILED: u8 = 2;
/// At least one Host was `unreachable` (or `timeout`).
pub const EXIT_UNREACHABLE: u8 = 4;
/// The command line could not be understood. Not clap's default `2`, which
/// means "a Host failed".
pub const EXIT_USAGE: u8 = 5;
/// The run was interrupted.
pub const EXIT_INTERRUPTED: u8 = 99;

pub fn run() -> ExitCode {
    let cli = match cli::Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            let code = match err.kind() {
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => {
                    EXIT_OK
                }
                _ => EXIT_USAGE,
            };
            let _ = err.print();
            return ExitCode::from(code);
        }
    };

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("rshx: {err}");
            return ExitCode::from(EXIT_LOCAL);
        }
    };

    match runtime.block_on(run::execute(&cli)) {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("rshx: {err:#}");
            ExitCode::from(EXIT_LOCAL)
        }
    }
}
