# 11: Per-host timeout

**What to build:** `--timeout <duration>` bounds how long rshx will wait for any single host, so one wedged machine cannot hold a run open. When it fires, that host is reported as `timeout` and its ssh child is terminated exactly the way an interrupt terminates one — SIGTERM, then SIGKILL after the grace period — while every other host carries on.

`timeout` is its own status in the report, per CONTEXT.md, but it counts as unreachable for the exit code: a run in which every host timed out exits 4.

Durations are written the way humantime reads them, such as `30s` or `5m`. A malformed duration is a usage error, exit 5. Without the flag there is no limit.

**Blocked by:** 10.

**Status:** done

- [x] `--timeout 1s` against a host that takes longer reports that host as `timeout` and moves on.
- [x] Hosts that finish inside the limit are unaffected.
- [x] A run in which every host times out exits 4.
- [x] A timed-out host's child is terminated with SIGTERM, then SIGKILL after the grace period.
- [x] `--timeout 30s` and `--timeout 5m` are accepted; a malformed duration is a usage error.
- [x] No limit applies when the flag is absent.
- [x] The report distinguishes `timeout` from `unreachable` even though they share an exit code.
