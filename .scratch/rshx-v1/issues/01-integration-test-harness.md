# 01: Integration-test harness with a fake ssh

**What to build:** A way to exercise rshx end-to-end with no network and no sshd. A stub `ssh` executable sits first on `PATH`; it records the exact argv it was given and then plays a scripted response for that destination — what to write to stdout, what to write to stderr, what exit code to return, and how long to wait first. Every later ticket's acceptance criteria are verified through this harness, so it has to exist before any of them.

This ticket is prefactoring, not a user-visible slice: it delivers no new rshx behaviour. It also clears the stale host-loading stub and the unused empty module directories left over from an abandoned layout, so the crate builds clean before real work starts.

**Blocked by:** None (can start immediately).

**Status:** done

- [x] The crate builds with no warnings and no dead code; the stale host-loading stub and the unused empty module directories are gone.
- [x] A test can run the rshx binary (using the binary path cargo provides to integration tests) with a temporary directory whose stub `ssh` is first on `PATH`.
- [x] The stub writes down the argv it received, so a test can assert exactly how rshx invoked ssh.
- [x] The stub can be scripted per destination: stdout, stderr, exit code, and a delay before responding.
- [x] The stub can report how many invocations are running at once, so later tickets can assert a concurrency bound.
- [x] At least one test proves the harness itself works end-to-end.
- [x] No new dependencies.
