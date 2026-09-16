# rshx

Run one command on many hosts over ssh, with a bounded fanout and a readable
per-host report.

```console
$ rshx -H hosts.toml --stdout -- du -hs /data
node01 ok 0.31s  42G	/data
node02 ok 0.28s  38G	/data
node03 failed 0.12s
  du: cannot access '/data': No such file or directory
node04 unreachable 5.00s (connect)
  ssh: connect to host node04 port 22: Connection timed out
4 hosts: 2 ok, 1 failed, 1 unreachable in 5.02s
```

Each line is one host, and hosts are reported as they settle rather than in
host-file order.

## Why

**Connections are system `ssh`.** rshx runs `ssh` and forwards your command to
it as arguments. `~/.ssh/config`, `ssh-agent`, `known_hosts`, `ProxyJump`,
`ProxyCommand`, `IdentityFile`, `ControlMaster` and everything else in your ssh
setup apply unchanged, because ssh is the one reading them. rshx implements no
connection of its own and has no ssh library dependency — so it is not an ssh
client, and cannot drift from the one you already have.

**Hosts come from a file, not a plugin.** The host file is the only host
source: no modules, no inventory schema, no service discovery.

**Fanout is a sliding window.** At most `-f` remote commands run at once, and
each one that finishes is immediately replaced by a pending one — the model
pdsh uses. 1000 hosts at the default fanout never means more than 32 ssh
processes.

**A status never depends on parsing text.** Whether a host is `ok`, `failed` or
`unreachable` is a pure function of ssh's exit status. Text from ssh's stderr
can add a `cause`, but it can never change a status or an exit code.

rshx is not a configuration-management tool. It runs one command and reports
what happened; there is no task model, no modules, and no YAML.

## Install

```console
$ cargo build --release
$ ./target/release/rshx --help
```

Or `cargo install --path .`. The only runtime requirement is `ssh` on `PATH`.

## Quick start

```console
$ cat hosts.toml
[[hosts]]
name = "node[01-04]"

[groups]
ascend = ["node[01-04]"]

$ rshx -H hosts.toml -- uptime
$ rshx -H hosts.toml -g ascend -- npu-smi info
$ rshx -H hosts.toml -f 64 --timeout 30s -- systemctl status kubelet
$ rshx -H hosts.toml --json -- hostname | jq -r '.host'
```

## The host file

TOML, passed with `-H FILE`. Without `-H`, rshx looks for `./rshx.toml`, then
`$XDG_CONFIG_HOME/rshx/hosts.toml` (usually `~/.config/rshx/hosts.toml`), and
fails telling you to pass `-H` or create `./rshx.toml` if neither exists. An
explicit `-H` is used as written, so a typo is reported rather than quietly
falling back to another file.

```toml
# Every host is one [[hosts]] entry. `name` is the only required field.
[[hosts]]
name = "node01"

# A pattern declares many hosts at once.
[[hosts]]
name = "gpu[01-08]"

# Target overrides. Each becomes an ssh -o option for this host only, so the
# rest of ~/.ssh/config still applies.
[[hosts]]
name = "jumpbox"
ip = "10.0.0.7"     # -o HostName=10.0.0.7   (an IP literal, not a name)
user = "root"       # -o User=root
port = 2222         # -o Port=2222

# Groups name hosts, so a run can select a subset.
[groups]
gpu = ["gpu[01-08]"]
fleet = ["node01", "gpu"]   # a group may name other groups
```

Field rules:

| Field  | Type   | Notes |
|--------|--------|-------|
| `name` | string | Required. An ssh destination: selects `Host` blocks in `~/.ssh/config`. |
| `ip`   | string | An IP literal, passed as `-o HostName=`. |
| `user` | string | Passed as `-o User=`. |
| `port` | integer | Passed as `-o Port=`. |

Overrides are `-o` options rather than a rewritten destination, so the rest of
`~/.ssh/config` still applies to that host.

Unknown fields are an error rather than ignored, so a typo is reported instead
of silently dropping a host's configuration. Host names and users are validated
against a whitelist — letters, digits, `.`, `_`, `-`, not starting with `-` —
because both are handed to ssh as argv elements, and a whitelist cannot be
escaped out of.

## Host patterns

A `name` may contain **one** bracketed range: literal text, the range, literal
text.

```
node[01-32]         node01 … node32
gpu[01,03,05-08]    gpu01, gpu03, gpu05 … gpu08
rack[01-04]-node    rack01-node … rack04-node
```

The literal text before and after the range may be anything; what a name cannot
have is a second range, so `rack[01-04]-n[1-2]` is rejected rather than guessed
at.

The width of the lower bound as written decides the zero padding: `[1-32]`
gives `node1`, `[01-32]` gives `node01`. Ranges must ascend, and strides are not
supported. Anything the syntax cannot express is an error rather than a guess —
a pattern that silently expanded to the wrong hosts would run a command
somewhere nobody asked for. One entry expands to at most 65536 hosts.

## Groups

`[groups]` maps a name to selectors. A selector is a host name, a host pattern,
or another group's name; groups may nest, and a cycle among them is reported as
an error naming the cycle. Every selector must match at least one declared host.

`-g` selects groups. It is repeatable, each value may be comma-separated, and
several groups are a union with each host appearing once. `-g all` selects
every host in the file, which is also the default when `-g` is absent. `all` is
reserved and cannot be declared as a group.

## Output

stdout carries one line per host and nothing else; stderr carries the summary,
the heartbeat, and the note about unfinished commands. So `rshx … > out`
captures the results alone, while the summary still reaches your terminal. That
split is what makes `--json` safe to pipe: stdout stays machine-readable even
when the run is long enough to draw a heartbeat.

By default an `ok` host shows neither stream: its line is the whole result,
which is what keeps a wide run readable. A host that is not `ok` always shows
its stderr, because that is where the reason is.

| Flag       | Effect |
|------------|--------|
| `--stdout` | Show every host's stdout, including `ok` hosts. |
| `--stderr` | Show an `ok` host's stderr. A host that is not `ok` shows it either way. |

A stream that is exactly one line and at most 200 bytes — so a line of up to
199 characters plus its newline — is folded onto the status line; anything
longer is written as an indented block beneath it. Folding a megabyte of text
onto one line would defeat the point of folding.

Each stream is capped at 1 MiB: bytes past the cap are dropped and a marker is
written where it was cut, so a reader is never left trusting a silently
shortened stream. `--json` reports it as `truncated` instead.

`--color auto` (the default) colours each stream according to the terminal it
is attached to, so stdout and stderr are decided independently — piping stdout
gives plain text while the summary on stderr stays coloured. `NO_COLOR` turns
colour off under `auto`; it is a default, not an override, so an explicit
`--color always` still colours. `--color never` is the way to be sure.

A progress heartbeat is drawn on stderr when stderr is a terminal, and only
then: it is absent from a redirected stderr, and off entirely under `--json`.

### JSON

`--json` writes one object per host, one per line, on stdout. Absent fields are
omitted rather than written as `null`.

```console
$ rshx -H hosts.toml --json -- du -hs /data | jq -c 'select(.status != "ok")'
{"host":"gpu02","status":"unreachable","exit_code":255,"cause":"auth","duration_ms":4,"stdout":"","stderr":"Permission denied (publickey).\n","truncated":false}
```

Because ssh reports a failed connection with its own exit status, an
authentication failure is `unreachable` — the command never ran — not `failed`.

| Field | Notes |
|-------|-------|
| `host` | The host's name. |
| `status` | `ok`, `failed`, `unreachable`, `timeout` or `cancelled`. |
| `exit_code` | ssh's exit status. Absent for a `cancelled` host: there is none to report. |
| `cause` | `auth`, `dns` or `connect`. Absent unless one was recognised. |
| `duration_ms` | How long the host took. |
| `stdout` / `stderr` | Captured verbatim, as JSON strings. |
| `truncated` | Whether either stream lost bytes to the cap. |

## Statuses and causes

| Status | Meaning |
|--------|---------|
| `ok` | The command ran and returned zero. |
| `failed` | The command ran and returned non-zero. |
| `unreachable` | ssh itself failed, so the command never ran. |
| `timeout` | rshx gave up waiting. The remote command is **not** stopped and may still be running. |
| `cancelled` | The run was interrupted before the host's outcome was known. Also not stopped. |

A `cause` is a best-effort explanation read from ssh's stderr — `auth`, `dns`
or `connect` — and it never changes a status or an exit code. It is absent when
ssh's text says nothing useful, which is the honest answer.

## Exit codes

| Code | Meaning |
|------|---------|
| `0`  | Every host was `ok`. |
| `1`  | A local error: the host file is missing, malformed, or names an unknown group. |
| `2`  | At least one host `failed`. |
| `4`  | At least one host was `unreachable` or `timeout`. |
| `6`  | Both `2` and `4` — the bits are independent, so they combine. |
| `5`  | The command line could not be understood. |
| `99` | The run was interrupted. |

`cancelled` is not a failure: rshx stopped waiting, the command did not fail. So
a run that was interrupted exits `99` and nothing else.

## Timeouts and interrupts

`--timeout 30s` bounds how long rshx waits for any one host. On expiry the
host's ssh is terminated and reported as `timeout`; every other host carries
on. The remote command is not stopped — rshx killed the connection, not the
work — and it says so under the summary.

Ctrl-C stops the run: hosts still in flight are `cancelled`, hosts that had
already settled keep their real status, and hosts that never started are not
reported at all, since their command never ran. A second Ctrl-C does not wait
out the grace period. Each ssh child runs in its own process group, so the
interrupt reaches rshx alone and rshx decides when its children die.

## Options

```
Usage: rshx [OPTIONS] -- <COMMAND>...

  -H, --host-file <FILE>    The host file listing the hosts to run on
  -f, --fanout <N>          How many remote commands to run at once [default: 32]
  -g, --groups <GROUP>...   Run only the hosts these groups select
      --stdout              Show each host's stdout
      --stderr              Show the stderr of a host that is ok
      --timeout <DURATION>  How long to wait for any one host, such as 30s or 5m
      --json                One JSON object per host, one per line
      --color <WHEN>        auto, always or never [default: auto]
```

The command goes after `--` and is forwarded to ssh verbatim as its own
arguments — rshx does no shell joining, so ssh does it, exactly as it would if
you typed the command yourself. A destination is never read as an option,
because `--` ends option parsing.

## Development

```console
$ cargo test
$ cargo clippy --all-targets -- -D warnings
$ cargo fmt --check
```

Integration tests never touch the network or an sshd. Each one runs the real
binary against a scripted fake `ssh` placed first on `PATH`, which records the
argv of every invocation and replays a response scripted per destination, so
how ssh was invoked and what the report says are both checkable offline. See
`tests/support/mod.rs`.
