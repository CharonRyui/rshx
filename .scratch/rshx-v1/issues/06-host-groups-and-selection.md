# 06: Host groups and selection

**What to build:** Named groups that decide which hosts a run touches, so a host file can describe a whole estate and a command can be aimed at part of it.

A group lists selectors — literal names or patterns — and may also list child groups, which are expanded recursively. `-g` picks groups; it may be repeated and each value may be a comma-separated list. Several groups combine as a union, and a host selected twice is run once. Running with no `-g` uses every host in the file.

`all` is reserved and cannot be the name of a group a user defines. A group that does not exist, a selector that matches no host, and a cycle among child groups are all errors: silently running on fewer hosts than asked for is the failure mode this ticket exists to prevent.

**Blocked by:** 04.

**Status:** done

- [x] `-g web` runs only the hosts that group selects.
- [x] `-g web -g db` and `-g web,db` both run the union, each host exactly once.
- [x] A group whose selectors include a pattern selects every host it expands to.
- [x] Child groups are expanded recursively, and a group reachable by two paths still contributes each host once.
- [x] A cycle among child groups is an error that names the cycle.
- [x] An unknown group name, a selector matching no host, and a user-defined group named `all` are each errors.
- [x] With no `-g`, every host in the file runs.
- [x] Group selection composes with `-H`: groups are resolved against the file that `-H` chose.
