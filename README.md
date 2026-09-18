# rshx

Run one command — or one script — on many hosts over ssh, with a bounded fanout
and a readable per-host report.

```console
$ rshx -H hosts.toml run -- du -hs /data
du -hs /data  ·  4 hosts
node01 ok          0.31s  42G	/data
node02 ok          0.28s  38G	/data
node03 failed      0.12s
  du: cannot access '/data': No such file or directory
node04 unreachable 5.00s (connect)
  ssh: connect to host node04 port 22: Connection timed out
4 hosts: 2 ok, 1 failed, 1 unreachable in 5.02s
```

Each line is one host, reported as it settles rather than in host-file order.
The status column is padded to the longest status, so a mixed run still reads as
a table rather than as ragged text.

## Why

**Connections are system `ssh`.** rshx runs `ssh` and forwards your command to
it as arguments, so `~/.ssh/config`, `ssh-agent`, `known_hosts`, `ProxyJump`,
`ProxyCommand`, `IdentityFile`, `ControlMaster` and the rest of your ssh setup
apply unchanged. rshx opens no connection of its own.

**Hosts come from a file, not a plugin.** The host file is the only host
source: no modules, no inventory schema, no service discovery.

**Fanout is a sliding window.** At most `-f` remote commands run at once, and
each one that finishes is replaced by a pending one. 1000 hosts at the default
fanout never means more than 32 ssh processes.

**A status never depends on parsing text.** Whether a host is `ok`, `failed` or
`unreachable` is a pure function of ssh's exit status. Text from ssh's stderr
can add a `cause`, but it can never change a status or an exit code.

**A script is sent, not fetched.** `--script` writes the file to the host over
the ssh connection rshx already has, runs it there, and removes it. Nothing is
installed, nothing is kept, and no host has to reach the machine rshx runs on.

rshx is not a configuration-management tool. It runs one command or one script
and reports what happened; there is no task model, no modules, no YAML, and no
state between runs.

## Install

Download the archive for your machine from the
[Releases](https://github.com/CharonRyui/rshx/releases) page, then:

```console
$ tar xzf rshx-<tag>-x86_64-unknown-linux-musl.tar.gz
$ install -m 755 rshx ~/.local/bin/
```

Each archive holds the single `rshx` binary and nothing else. The `musl` builds
are static and run on any Linux; the `gnu` builds are dynamically linked against
glibc. Releases cover Linux only. The only runtime requirement is `ssh` on
`PATH`.

## Quick start

```console
$ cat hosts.toml
[[hosts]]
name = "node[01-04]"

[groups]
ascend = ["node[01-04]"]

$ rshx -H hosts.toml run -- uptime
$ rshx -H hosts.toml -g ascend run -- npu-smi info
$ rshx -H hosts.toml -f 64 --timeout 30s run -- systemctl status kubelet
$ rshx -H hosts.toml run --script ./collect.sh
$ rshx -H hosts.toml --json run -- hostname | jq -r '.host'
$ rshx -H hosts.toml ping
$ rshx -H hosts.toml -g ascend list
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

# A password of its own, for a host whose sudo wants a different one.
[[hosts]]
name = "bastion"
unique_privilege_pass = true    # --privilege asks for this host separately

# Groups name hosts, so a run can select a subset.
[groups]
gpu = ["gpu[01-08]"]
fleet = ["node01", "gpu"]   # a group may name other groups
```

| Field                   | Type   | Notes |
|-------------------------|--------|-------|
| `name`                  | string | Required. An ssh destination: selects `Host` blocks in `~/.ssh/config`. |
| `ip`                    | string | An IP literal, passed as `-o HostName=`. |
| `user`                  | string | Passed as `-o User=`. |
| `port`                  | integer | Passed as `-o Port=`. |
| `unique_privilege_pass` | bool | `--privilege` only: ask for this host's own password instead of the one the run shares. Defaults to `false`. |

Unknown fields are an error rather than ignored, so a typo is reported instead
of silently dropping a host's configuration. Host names and users are
restricted to letters, digits, `.`, `_` and `-`, and may not start with `-`.

## Host patterns

A `name` may contain **one** bracketed range: literal text, the range, literal
text.

```
node[01-32]         node01 … node32
gpu[01,03,05-08]    gpu01, gpu03, gpu05 … gpu08
rack[01-04]-node    rack01-node … rack04-node
```

A name cannot have a second range, so `rack[01-04]-n[1-2]` is rejected rather
than guessed at. The width of the lower bound as written decides the zero
padding: `[1-32]` gives `node1`, `[01-32]` gives `node01`. Ranges must ascend,
and strides are not supported. Anything the syntax cannot express is an error
rather than a guess — a pattern that silently expanded to the wrong hosts would
run a command somewhere nobody asked for. One entry expands to at most 65536
hosts.

## Groups

`[groups]` maps a name to selectors. A selector is a host name, a host pattern,
or another group's name; groups may nest, and a cycle among them is reported as
an error naming the cycle. Every selector must match at least one declared host.

`-g` selects groups. It is repeatable, each value may be comma-separated, and
several groups are a union with each host appearing once. `-g all` selects
every host in the file, which is also the default when `-g` is absent. `all` is
reserved and cannot be declared as a group.

Each `-g` takes one value, so `-g web,db` and `-g web -g db` both select two
groups while `-g web db` does not: the options come before the subcommand, and
a second bare word there would be read as the subcommand rather than as another
group.

## Listing hosts

`list` prints the hosts a run would select, and contacts none of them:

```console
$ cat hosts.toml
[[hosts]]
name = "web[01-02]"
user = "deploy"
port = 2222

[[hosts]]
name = "db01"
ip = "10.0.0.7"

[[hosts]]
name = "bastion"
unique_privilege_pass = true

[groups]
web = ["web[01-02]"]
core = ["db01", "bastion"]

$ rshx -H hosts.toml list
HOST     USER    PORT  IP        OWN-PASSWORD
web01    deploy  2222  -         -
web02    deploy  2222  -         -
db01     -       -     10.0.0.7  -
bastion  -       -     -         yes

$ rshx -H hosts.toml -g core list
HOST     IP        OWN-PASSWORD
db01     10.0.0.7  -
bastion  -         yes
```

It takes the same `-H` and `-g` as a run, so a listing is the cheap way to see
what a group selects — or what a pattern expanded to — before committing to a
run. The hosts come out in host-file order, with each host's overrides beside
it.

A column appears only when some listed host fills it, so a file of bare names
lists bare names:

```console
$ rshx -H hosts.toml list
node01
node02
node03
node04
```

`--json` writes one object per host instead, with the host file's field names
and a field omitted when the host does not set it:

```console
$ rshx -H hosts.toml --json list
{"host":"web01","user":"deploy","port":2222,"unique_privilege_pass":false}
{"host":"web02","user":"deploy","port":2222,"unique_privilege_pass":false}
{"host":"db01","ip":"10.0.0.7","unique_privilege_pass":false}
{"host":"bastion","unique_privilege_pass":true}
```

A listing is not a report: no host has a status, because no host did anything.
stdout carries the listing and nothing else, so `rshx list > hosts.txt` gives a
file of names, and the exit code is `0` unless the host file or the selection
was itself an error.

## Privilege

`--privilege` runs each host's command under a remote `sudo`, and answers the
password prompt when a host's sudo asks for one.

```console
$ rshx -H hosts.toml --privilege run -- systemctl restart nginx
sudo -S -p rshx-password: systemctl restart nginx · 4 hosts
privilege password:
node01 ok 0.42s
node02 ok 0.39s
node03 ok 0.31s
node04 unreachable 5.00s (connect)
ssh: connect to host node04 port 22: Connection timed out
4 hosts: 3 ok, 1 unreachable in 5.02s
```

`sudo` is put in front of the command, which is forwarded after it verbatim. A
command that runs `sudo` itself is wrapped like any other, so its options mean
what they always meant, one elevation further in — `--privilege -- sudo -u
www-data psql` runs as root and then as `www-data`. rshx warns when it sees
that, since the nesting is rarely what was meant. The heading names the command
as it actually runs, plumbing included.

`--privilege` elevates to root and to nothing else: there is no runas option.
To be another user, be it from root, which needs no password to do it:

```console
$ rshx -H hosts.toml --privilege run -- sh -c 'sudo -u postgres pg_dump mydb'
```

The password is asked for **once per run**, on the terminal, and written to
each asking host's sudo on its stdin — not once per host, which for 200 hosts
would be 200 identical questions. A host whose sudo rejects the password is
asked about again, and the new answer goes to every host still waiting for one.
A host whose sudo never asks, because sudoers says `NOPASSWD`, is never
prompted for and never written to.

An empty password stops the run rather than half a fleet getting a password
that cannot work, and so does having nowhere to ask: a run with no controlling
terminal stops the moment a host asks for one. Both are a local failure — exit
`1`, with the reason on stderr and nothing written to any host. Ctrl-C at the
prompt stops the run the way Ctrl-C anywhere else does.

A host that needs a password of its own says so in the host file, so the run's
shared password is never handed to it:

```toml
[[hosts]]
name = "bastion"
unique_privilege_pass = true
```

A command that takes its password from an askpass program (`sudo -A`) needs no
special case: the wrapper's sudo is the one that authenticates, and an inner
`sudo -A` runs as root, where it has nothing to ask.

## Scripts

`run --script FILE` runs a local script on every host instead of a command:

```console
$ rshx -H hosts.toml run --script ./collect.sh
./collect.sh (script)  ·  4 hosts
node01 ok          0.31s
node02 ok          0.28s
node03 failed      0.12s
  collect: /data: No such file or directory
node04 unreachable 5.00s (connect)
4 hosts: 2 ok, 1 failed, 1 unreachable in 5.02s
```

Each host gets its own copy, and every copy is sent over that host's ssh on
stdin, so a script needs no scp, no shared filesystem, and no `authorized_keys`
change. On the host rshx writes it to a temporary file, makes it executable,
runs it, and removes it again — whatever the script exits with, so nothing is
left behind. The script is run from the path it was copied to, so `$0`,
`dirname "$0"` and a shebang all mean what they would locally; a script with no
shebang is run by the host's shell, the way it would be if you typed its path.

A script's own exit status is the host's: zero is `ok`, anything else is
`failed`, and its output is reported exactly as a command's is. `--privilege`
elevates the script to root, and only the script: the copy and the removal are
the host user's own work, so a sudo that refuses a password still leaves no
file behind.

The script must be a readable regular file — a missing one, or a directory, is
a local error, reported once before anything is sent anywhere. `--timeout`
covers both steps: what the copy spends comes off what the run has left. A copy
that exits zero without printing where it put the file leaves nothing to run:
that host is reported `unreachable`, with rshx's note and the host's own output,
and the script is not run there.

## Ping

`ping` runs `echo pong` on every selected host, which is the cheapest way to
see which hosts answer:

```console
$ rshx -H hosts.toml ping
hellohpc-ascend0 ok 0.28s  pong
hellohpc-ascend1 ok 0.29s  pong
2 hosts: 2 ok in 0.31s
```

It takes the same selection options as `run`, and reports through the same
statuses, exit codes and `--json` output — a host that answers is `ok`, and one
that does not is `unreachable`.

## Output

stdout carries the host results and nothing else; stderr carries the chrome —
the heading, the heartbeat, the summary, and the note about unfinished
commands. So `rshx … > out` captures the results alone, while the progress and
the summary still reach your terminal. That split is what makes `--json` safe to
pipe: stdout stays machine-readable even when the run is long enough to draw a
heartbeat.

Every host's stdout is shown, folded onto its status line when it is a single
line of at most 200 bytes — a line of up to 199 characters plus its newline —
and written as an indented block beneath it otherwise. An `ok` host's stderr is
hidden: its line is the result, which is what keeps a wide run readable. A host
that is not `ok` always shows its stderr, because that is where the reason is.

| Flag       | Effect |
|------------|--------|
| `-q`, `--quiet` | Hide every host's stdout. |
| `--stderr` | Show an `ok` host's stderr. A host that is not `ok` shows it either way. |

Each stream is capped at 1 MiB: bytes past the cap are dropped and a marker is
written where it was cut, so a reader is never left trusting a silently
shortened stream. `--json` reports it as `truncated` instead.

`--color auto` (the default) colours each stream according to the terminal it
is attached to, so stdout and stderr are decided independently — piping stdout
gives plain text while the summary on stderr stays coloured. `NO_COLOR` turns
colour off under `auto`; it is a default, not an override, so an explicit
`--color always` still colours. `--color never` is the way to be sure.

Colour marks a host's **outcome**, the one thing a reader scans for: green `ok`,
red `failed`, yellow `unreachable`, magenta `timeout`, dim `cancelled`.
Everything secondary — the duration, the cause, the run's elapsed time — is
dimmed, and the host's name is bold rather than coloured. A host's own output is
never coloured: it is remote bytes rshx cannot interpret, so it is passed
through as it arrived. Every styled token is also written as plain text, so the
report never depends on colour to be read.

### JSON

`--json` writes one object per host, one per line, on stdout. Absent fields are
omitted rather than written as `null`.

```console
$ rshx -H hosts.toml --json run -- du -hs /data | jq -c 'select(.status != "ok")'
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

## Progress

A heartbeat is drawn on stderr while hosts are in flight, and only when stderr
is a terminal: it is absent from a redirected stderr, and off entirely under
`--json`. It is one line, redrawn in place, so a long run shows that it is
alive rather than looking hung:

```console
$ rshx -H hosts.toml run -- du -hs /data
du -hs /data  ·  200 hosts, fanout 32
⠸ 95/200 done ━━━━━━━━━━━━━━━━━━━╸───────────────────── 30 running, node096 0.4s
```

The line carries the spinner, how far the run has got, a bar, and what is still
in flight — including the slowest host, which is what tells you a run is waiting
on one machine rather than on the network. The heading names the command and how
many hosts it will run on, mentioning the fanout only when the fanout, rather
than the host count, is what limits the run. The line is drawn to the width of
the terminal, so it never wraps however narrow the window is.

The heartbeat is chrome, not report: it is cleared before the summary, and it
never touches stdout.

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
ssh's text says nothing useful.

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

A host waiting at a `--privilege` password prompt is not taking too long: the
time it spends at the prompt is subtracted from what its limit measures, and
only the hosts actually waiting are stopped. A host whose command runs on while
another host's prompt is up keeps counting, and can still time out.

Ctrl-C stops the run: hosts still in flight are `cancelled`, hosts that had
already settled keep their real status, and hosts that never started are not
reported at all, since their command never ran. A second Ctrl-C does not wait
out the grace period.

## Options

`rshx -h` prints:

```
Run operations on many hosts over ssh

Usage: rshx [OPTIONS] <COMMAND>

Commands:
  run   Run command or script on hosts
  ping  Check all hosts are available
  list  List the Hosts a run would select, without contacting any of them
  help  Print this message or the help of the given subcommand(s)

Options:
  -H, --host-file <FILE>    The host file listing the hosts to run on
  -f, --fanout <N>          How many commands run at once; a finish is replaced by a pending one [default: 32]
  -g, --groups <GROUP>      Run only the Hosts these groups select. Repeatable, each value may be comma-separated; defaults to every Host in the host file. One value per occurrence: a group name is never read as the subcommand
  -q, --quiet               Hide each Host's stdout
      --stderr              Show an `ok` Host's stderr; one that is not `ok` always shows stderr
      --privilege           Run the command as root, with `sudo`, answering its password prompt from the terminal when a Host asks. The command is wrapped whole, so an inner `sudo` keeps its own options and elevates a second time inside rshx's; a Host that needs its own password says so in the host file
      --timeout <DURATION>  How long to wait for any one Host, such as `30s` or `5m`; without it there is no limit. A Host that times out is reported as `timeout`, its ssh is terminated, and the remote command is not stopped
      --json                Write one JSON object per Host, one per line, instead of the plain report. stdout carries nothing else, and the heartbeat is off
      --color <COLOR>       When to colour the report [default: auto] [possible values: auto, always, never]
  -h, --help                Print help (see more with '--help')
  -V, --version             Print version
```

The options come before the subcommand; `rshx run -h` prints the run's own:

```
Run command or script on hosts

Usage: rshx run <--script <SCRIPT_PATH>|COMMAND>

Arguments:
  [COMMAND]...  The command to run on every host, after `--`

Options:
      --script <SCRIPT_PATH>  Script file to run on every Host
  -h, --help                  Print help (see more with '--help')
```

Exactly one of the two is given: a command after `--`, or `--script FILE`.

`ping` and `list` take no arguments of their own: everything they answer from
is in the options above. A run's options that shape a command — `-f`, `-q`,
`--stderr`, `--privilege`, `--timeout` — have nothing to shape in a listing,
which runs nothing, and are accepted and ignored there.

The command goes after `--` and is forwarded to ssh verbatim as its own
arguments — rshx does no shell joining, so ssh does it, exactly as it would if
you typed the command yourself. A destination is never read as an option,
because `--` ends option parsing.
