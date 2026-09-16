# 10: Interrupting a run

**What to build:** Ctrl-C should stop a run in a way the user can predict. The first one stops dispatching new hosts, terminates the ssh children in flight with SIGTERM, and gives any that are still alive two seconds before SIGKILL. Hosts that were cut short are reported as `cancelled`, counted in the summary, and are not failures: they do not set exit code 2 or 4, and the run exits 99. A second Ctrl-C skips the grace period.

Each ssh child runs in its own process group, so the terminal's interrupt reaches rshx alone and rshx decides when its children die, instead of the kernel signalling them behind rshx's back.

The report must not imply the remote work stopped. Per ADR-0008 and the CONTEXT.md definition of `cancelled`, killing the local ssh leaves the remote command running: `cancelled` means this run stopped waiting, never that the remote command is gone.

**Blocked by:** 03.

**Status:** done

- [x] Ctrl-C stops new hosts from starting.
- [x] Children in flight get SIGTERM, and SIGKILL if they outlive the grace period.
- [x] Hosts cut short are reported as `cancelled` and appear in the summary.
- [x] `cancelled` hosts do not contribute to exit codes 2 or 4, and the run exits 99.
- [x] A second Ctrl-C does not wait out the grace period.
- [x] Hosts that had already settled keep their real status and are still reported.
- [x] The report makes clear that cancelling stops waiting, not the remote command.
- [x] Children are spawned in their own process group, verified by the interrupt reaching only rshx.
