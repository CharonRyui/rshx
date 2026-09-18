//! `list`: the Hosts a run would select, printed from the host file alone.
//!
//! Nothing is contacted and nothing runs, so a listing is also the cheap way to
//! see what a `-g` selects before committing to a run. It is not a Report: no
//! Host has a Status, because no Host did anything.
use std::fmt::Write as _;
use std::io::Write as _;

use anstyle::Style;
use anyhow::Result;

use crate::EXIT_OK;
use crate::cli::CliOptions;
use crate::host::Host;
use crate::report;

/// A Host's name is bold, as it is in the report's first column: weight
/// survives where a colour may not.
const HOST: Style = Style::new().bold();
/// The header is dim, so the Hosts read as the listing and the header does not.
const HEADER: Style = Style::new().dimmed();

/// Prints one line per selected Host, in host-file order.
pub(super) fn execute(selected: &[&Host], options: &CliOptions) -> Result<u8> {
    // stdout carries the listing and nothing else, so it can be read or piped
    // on; the colour follows the run's `--color` policy.
    let mut stdout = report::stream(options.color, std::io::stdout());

    if options.json {
        for host in selected {
            // Nothing in a JsonHost can fail to encode.
            let line = serde_json::to_string(&JsonHost::of(host))?;
            let _ = writeln!(stdout, "{line}");
        }
    } else {
        // A listing that stopped being read, as by `list | head`, is the
        // reader's business rather than a local error.
        let _ = table(&mut stdout, selected);
    }
    let _ = stdout.flush();

    Ok(EXIT_OK)
}

/// Writes the Hosts as a table: one row each, one column per field.
fn table<W: std::io::Write>(out: &mut W, selected: &[&Host]) -> std::io::Result<()> {
    // A column no Host fills would be a stripe of dashes down the page, so a
    // field brings its column with it: a host file of bare names lists names.
    let show_user = selected.iter().any(|host| host.user.is_some());
    let show_port = selected.iter().any(|host| host.port.is_some());
    let show_ip = selected.iter().any(|host| host.ip.is_some());
    let show_own = selected.iter().any(|host| host.unique_privilege_pass);
    // A header over one column of names tells the reader nothing they cannot
    // see; over several it is what says which column is which.
    let titled = show_user || show_port || show_ip || show_own;

    let headers: Vec<&str> = ["HOST", "USER", "PORT", "IP", "OWN-PASSWORD"]
        .into_iter()
        .zip([true, show_user, show_port, show_ip, show_own])
        .filter_map(|(header, shown)| shown.then_some(header))
        .collect();

    let mut rows: Vec<Vec<String>> = Vec::with_capacity(selected.len() + 1);
    if titled {
        rows.push(headers.iter().map(|header| header.to_string()).collect());
    }
    rows.extend(selected.iter().map(|host| {
        let mut cells = vec![host.name.clone()];
        if show_user {
            cells.push(host.user.clone().unwrap_or_else(|| "-".to_string()));
        }
        if show_port {
            cells.push(
                host.port
                    .map_or_else(|| "-".to_string(), |port| port.to_string()),
            );
        }
        if show_ip {
            cells.push(host.ip.map_or_else(|| "-".to_string(), |ip| ip.to_string()));
        }
        if show_own {
            cells.push(
                if host.unique_privilege_pass {
                    "yes"
                } else {
                    "-"
                }
                .to_string(),
            );
        }
        cells
    }));

    let mut widths = vec![0; headers.len()];
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.len());
        }
    }

    for (index, row) in rows.iter().enumerate() {
        let header = titled && index == 0;
        let mut line = String::new();
        for (column, cell) in row.iter().enumerate() {
            let last = column + 1 == row.len();
            let style = if header {
                Some(HEADER)
            } else if column == 0 {
                Some(HOST)
            } else {
                None
            };
            if let Some(style) = style {
                let _ = write!(line, "{}", style.render());
            }
            // The padding is written inside the styled span: a space has no
            // colour, so the reset lands where the next column starts.
            if last {
                let _ = write!(line, "{cell}");
            } else {
                let _ = write!(line, "{cell:<width$}", width = widths[column]);
                line.push_str("  ");
            }
            if let Some(style) = style {
                let _ = write!(line, "{}", style.render_reset());
            }
        }
        writeln!(out, "{line}")?;
    }

    Ok(())
}

/// One Host, as a line of JSON: the host file's fields, under the names the
/// host file gives them, and absent when the Host does not set them.
#[derive(serde::Serialize)]
struct JsonHost<'a> {
    host: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ip: Option<std::net::IpAddr>,
    unique_privilege_pass: bool,
}

impl<'a> JsonHost<'a> {
    fn of(host: &'a Host) -> JsonHost<'a> {
        JsonHost {
            host: &host.name,
            user: host.user.as_deref(),
            port: host.port,
            ip: host.ip,
            unique_privilege_pass: host.unique_privilege_pass,
        }
    }
}
