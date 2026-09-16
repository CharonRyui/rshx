# 08: Machine-readable output

**What to build:** `--json` turns rshx into something a script can consume, per ADR-0006: one JSON object per host, one per line, on stdout, written as soon as that host settles. No enclosing array, so a consumer sees results as they arrive instead of waiting for the slowest host.

Each object carries the host, its status, its exit code, the cause when there is one, the duration, stdout, stderr, and whether either stream was truncated. The exit code is absent when rshx itself killed the child, because then there is no exit code to report. Streams are decoded lossily so the strings are always valid JSON — byte-exact output stays the plain text mode's job. Each stream is capped at 1 MiB; past the cap the bytes are dropped and the object says so rather than the process growing without limit.

stdout carries nothing but JSON lines. The progress heartbeat is off under `--json`.

**Blocked by:** 07.

**Status:** done

- [x] Each settled host produces exactly one line of valid JSON, immediately, with no enclosing array.
- [x] The object has host, status, exit_code, cause, duration_ms, stdout, stderr and truncated.
- [x] `exit_code` is absent for a host rshx killed, and `cause` is present only when there is one.
- [x] Invalid UTF-8 in a stream is replaced rather than failing the run, and the line still parses.
- [x] A stream larger than 1 MiB sets `truncated` and does not grow memory without bound.
- [x] Output is flushed per line, so a line-oriented consumer sees results as they settle.
- [x] stdout contains only JSON lines; the summary stays on stderr.
- [x] Statuses and exit codes match what the plain report and the process exit code say for the same run.
