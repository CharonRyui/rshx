# 02: Run a command on every host in a host file

**What to build:** The first complete path through every layer. `rshx -H hosts.toml -- <command>...` reads a host file, runs the command on each host over ssh, prints one line per host, and exits with a code that says what happened. Hosts run one at a time in this ticket; bounding concurrency is the next one.

The host file is TOML with a `[[hosts]]` array of tables, each carrying at least a `name`. A `name` is both the identity of the host in the report and the ssh destination, so it is restricted to `[A-Za-z0-9._-]+` and may not begin with `-`. Two entries declaring the same name is an error. Per CONTEXT.md this is a Host, and its `name` is the Host's identity.

Everything after `--` is the remote command and is forwarded to ssh verbatim, including arguments that themselves begin with `-`. The destination is passed after a `--` of its own so ssh stops parsing options before it. ssh inherits the user's own configuration for everything else — `~/.ssh/config`, agent, `ProxyJump`, `ProxyCommand`, `IdentityFile`, `known_hosts` — per ADR-0001.

The report is one line per host on stdout and a closing summary on stderr, per ADR-0006. A host's status is a pure function of the exit code alone, never of output text, per ADR-0005: exit 0 is `ok`, exit 255 is `unreachable`, anything else is `failed`.

Exit codes follow ADR-0004: 0 when every host is ok; 1 for a local error such as a missing or malformed host file; 2 when any host failed; 4 when any host was unreachable; 6 when both happened; 5 for a usage error, which means taking over clap's default exit code of 2.

When `-H` is not given, look for `./rshx.toml`, then `$XDG_CONFIG_HOME/rshx/hosts.toml`.

**Blocked by:** 01.

**Status:** done

- [x] `rshx -H <file> -- hostname` prints one line per host, in host-file order.
- [x] The remote command's arguments reach ssh verbatim, including ones that begin with `-`.
- [x] A host name containing `@`, `/`, a space, or starting with `-` is rejected when the file is read.
- [x] Two entries declaring the same name are rejected, and the error names both.
- [x] A missing or malformed host file exits 1; a TOML syntax error keeps the line and column from the parser.
- [x] A usage error exits 5, not 2.
- [x] Exit codes 0, 2, 4 and 6 are produced by the rules above, and the stderr summary agrees with them.
- [x] Without `-H`, `./rshx.toml` is used when it exists, otherwise the XDG path is tried.
- [x] No status decision reads stdout or stderr text.
