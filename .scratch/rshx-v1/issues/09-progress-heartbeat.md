# 09: Progress heartbeat

**What to build:** A long run against many hosts should not look hung. While hosts are in flight, one status line on stderr updates in place with how many hosts are done, how many are still running, and which host has been going longest.

The heartbeat exists only when stderr is an interactive terminal. Redirected to a file or a pipe it is absent entirely, escape sequences included. It is off under `--json`, where stderr is for humans but the run is not being watched.

The result lines are the report; the heartbeat must never mangle them. When a result is printed, the heartbeat gets out of the way and is redrawn afterwards, and this must hold both on a terminal and in a redirected file.

**Blocked by:** 03, 08.

**Status:** done

- [x] With stderr on a terminal, a heartbeat line updates while hosts run and is gone when the run ends.
- [x] With stderr redirected, the file contains no heartbeat and no escape sequences.
- [x] `--json` produces no heartbeat.
- [x] Result lines are never overwritten or interleaved with the heartbeat, on a terminal or in a file.
- [x] The counts agree with the closing summary.
- [x] A run with a single host still reports it sensibly rather than dividing by zero or flashing.
