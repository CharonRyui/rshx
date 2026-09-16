# 04: Host patterns

**What to build:** A `name` in the host file may be a pattern that stands for several hosts, so a cluster is written as one line instead of thirty-two.

A name has at most one bracketed range: literal text before it, the range inside, literal text after it. The range is a comma-separated list of items, each either a single number or an ascending `low-high`. Text after the closing bracket is literal and may not contain another bracket.

The width of the lower bound as written decides zero padding: `node[0-3]` expands to `node0` through `node3`, while `node[01-03]` expands to `node01` through `node03`. Expansion is always ascending. There is no stride syntax.

Anything a pattern cannot express is an error rather than a guess: a descending range, a stride, an empty range, an unclosed bracket, a second bracketed range, or non-numeric bounds. Each expanded name goes through exactly the same validation as a literal name, and a name already claimed by another entry is a duplicate.

**Blocked by:** 02.

**Status:** done

- [x] `node[01-03]` expands to `node01`, `node02`, `node03`.
- [x] `node[0-3]` expands to `node0` through `node3`.
- [x] `gpu[1,3,5-7]` expands to `gpu1`, `gpu3`, `gpu5`, `gpu6`, `gpu7`.
- [x] `rack[01-02]-eth0` expands to `rack01-eth0` and `rack02-eth0`.
- [x] A descending range, a stride, an empty range, an unclosed bracket, a second bracketed range, and non-numeric bounds are each rejected when the file is read.
- [x] A pattern that expands into a name another entry already declared is rejected, naming both entries.
- [x] Expansion is ascending, and every expanded name passes the same validation a literal name does.
