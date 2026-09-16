# 07: A readable per-host report

**What to build:** Make the default report worth looking at. One line per host carries the host name, its status and how long it took. A host that is ok does not print its output — a hundred hosts running `du -hs /data` should give a hundred lines, not a hundred blocks.

A host that is not ok always shows why: its stderr is printed, whether or not anything was asked for. Alongside it, a short best-effort `cause` — `auth`, `dns` or `connect` — is derived from ssh's own stderr text. The cause is advisory only: it never changes the host's status and never changes the exit code, which stay a pure function of the exit code per ADR-0005.

`--stdout` and `--stderr` opt into showing a stream for hosts that are ok. A single line folds onto the status line; several lines become an indented block beneath it, which is what keeps a chatty host from destroying the report.

Colour is on when the stream being written is a terminal and off when it is not, so piping to a file gives clean text. `--color auto|always|never` overrides that, and `NO_COLOR` is honoured. The decision is per stream: stdout may be a pipe while stderr is a terminal.

**Blocked by:** 02.

**Status:** done

- [x] An ok host prints one line and none of its stdout.
- [x] `--stdout` shows an ok host's stdout, folded onto the status line when it is one line and indented beneath it when it is several.
- [x] A failed or unreachable host prints its stderr regardless of `--stdout`/`--stderr`.
- [x] `cause` is one of `auth`, `dns`, `connect`, or absent; it comes only from stderr; it never changes a status or an exit code.
- [x] Piping stdout to a file produces no escape sequences, while a terminal gets colour.
- [x] `--color never` removes colour on a terminal, and `NO_COLOR` does the same.
- [x] stdout and stderr are judged separately: a piped stdout with a terminal stderr still colours the stderr chrome.
- [x] Multi-line output is indented so it stays visually attached to its host.
