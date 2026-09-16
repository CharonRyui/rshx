# 05: Target overrides

**What to build:** A `[[hosts]]` entry may carry `user`, `port` and `ip` to override what ssh would otherwise resolve. Each is passed to ssh as its own `-o` option, so the override applies to that host only and every other setting — `IdentityFile`, `ProxyJump`, `ProxyCommand`, `ControlMaster` — still comes from the user's ssh configuration, per ADR-0001.

`user` and `port` may be written on a pattern entry and apply to every host it expands to. `ip` may only be written on an entry with a literal name, because an address is a property of one machine and cannot be shared by a pattern. An entry carrying only a `name` produces no `-o` options at all.

**Blocked by:** 04.

**Status:** done

- [x] `user`, `port` and `ip` on a literal entry each reach ssh as the corresponding `-o` option.
- [x] An entry with only a `name` adds no `-o` options.
- [x] `user` and `port` on a pattern entry apply to every host it expands to.
- [x] `ip` on a pattern entry is rejected when the file is read, with a message saying why.
- [x] An out-of-range `port` is rejected.
- [x] A host with an override still picks up `ProxyJump`, `ProxyCommand` and `IdentityFile` from the user's ssh configuration.
- [x] The override changes only the target ssh connects to, never the name the host is reported under.
