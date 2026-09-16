# 03: Bound concurrency with fanout

**What to build:** Stop running hosts one at a time. Keep a fixed number of ssh children in flight and start the next host the moment one finishes — the pdsh sliding window. `-f/--fanout` sets the bound and defaults to 32.

Because hosts now settle out of order, the report prints each host as it settles rather than in host-file order. The closing summary on stderr gains the run's totals and wall time.

**Blocked by:** 02.

**Status:** done

- [x] With `-f 2` against eight hosts that each take 200ms, the observed peak concurrency never exceeds 2 and the run takes about four times a single host.
- [x] The number in flight stays at the bound while work remains, and drains to zero at the end.
- [x] Result lines appear as hosts settle, not in host-file order.
- [x] `-f 1` is accepted; `-f 0` is a usage error.
- [x] The default is 32.
- [x] The exit code rules from the previous ticket still hold when hosts finish out of order.
