# Path MTU discovery

mping can discover the path MTU (PMTU) to a target: the largest IP packet,
headers included, that reaches it without fragmentation. The result shows
in the selected-target info pane on the `PMTU` line, next to the hop count,
and is refreshed with Enter like the other on-demand queries.

The implementation (`src/pmtu.rs`) is a sibling of [hopcount](hopcount.md)
and reuses its raw-socket machinery: `make_socket`, the "skip everything
that isn't ours" receive discipline, `time_left`, and the embedded-echo
check on ICMP error messages (`embedded_echo_v4/v6`, which now return the
quoted `(identifier, sequence)` pair for both callers).

## Probes

A probe is an ICMP Echo Request padded so the whole IP packet is `size`
bytes (`Prober::build`: `size − 20 − 8` bytes of payload for IPv4,
`size − 40 − 8` for IPv6), sent with Don't Fragment:

| OS | IPv4 | IPv6 |
|---|---|---|
| Linux | `IP_MTU_DISCOVER = IP_PMTUDISC_PROBE` | `IPV6_MTU_DISCOVER = IPV6_PMTUDISC_PROBE` |
| macOS | `IP_DONTFRAG = 1` | `IPV6_DONTFRAG = 1` |

Linux `PROBE` mode sets DF *and* bypasses the kernel's cached path MTU, so
the packet really leaves at the requested size (up to the interface MTU,
beyond which `send` fails with `EMSGSIZE` — treated as "too big, no hint").
Plain `DO` mode would have the kernel answer from its cache without
touching the wire, which is exactly what we don't want to measure.

Each probe carries identifier `0x4d54` (distinct from hopcount's `0xb00b`,
since Enter fires both at once on the same target) and a fresh sequence
number. The receive loop (`Prober::probe` → `classify_v4/v6`) accepts
exactly three things and skips everything else until the per-probe
timeout:

- **Echo Reply** from the target with our identifier *and the current
  sequence number* → `Reply`: this size fits.
- **Fragmentation Needed** (v4: Destination Unreachable code 4) or
  **Packet Too Big** (v6: type 2) whose quoted packet carries our
  identifier and current sequence → `TooBig(next_hop_mtu)`. The MTU is the
  low 16 bits of the second header word (RFC 1191) or the full word (RFC
  4443); zero, or a v6 value over 65535, becomes "no hint".
- Any other **Destination Unreachable** quoting our probe → `Unreachable`,
  a hard error.

Matching on the sequence number matters here more than in hopcount: a late
error for an earlier, larger probe must not be mistaken for a verdict on
the current one.

An echo reply is as large as the probe, so the receive buffer is 64 KiB.

## The search

`search` is a pure function over a `probe(size)` closure so it can be
tested against simulated paths (see the tests in `pmtu.rs`). It tracks
`good` (largest size that replied) and `bad` (smallest size that failed)
and ends in either a definite answer or a hard error:

1. **Start at the upper bound**: the kernel's route MTU on Linux (`IP_MTU`
   / `IPV6_MTU` read off a connected, never-used UDP socket — interface
   MTU or a cached PMTU), else 1500. A reply there is the common one-probe
   case. Nothing above the upper bound is ever probed, so a stale low
   kernel cache caps the answer; that cache is also what the kernel uses
   for real traffic, so it's the honest number.
2. **Follow the router's hint** when a `TooBig` carries a next-hop MTU that
   actually narrows the gap (`min ≤ hint < failed size`, above `good`). A
   reply at a hinted size is **final**: the router that reported it is
   authoritative for its link, and any narrower link further along would
   have answered that probe with its own error. So a cooperating path
   costs two probes. A hint that is itself too big (stale or wrong) just
   fails and the search continues below it.
3. **Without a hint** (or on silence) and nothing known to pass yet, probe
   the floor — 576 for IPv4 (RFC 1122), 1280 for IPv6 (RFC 8200). If even
   the floor gets no reply the target simply doesn't answer pings
   (`timeout`); if the floor is reported too big the path is broken
   (`Even the minimum MTU is too big`).
4. **Bisect** between `good` and `bad` until they are adjacent.

A probe that gets **no answer at all** while smaller ones do marks the
result as a **PMTUD black hole**: some router drops DF packets above a
size without sending the ICMP error. The search still converges via
bisection, at the cost of one timeout per failed size, and the info line
says so: `1400 (PMTUD black hole)`.

`MAX_PROBES` (16) bounds the whole run: bisecting 576..1500 needs about ten
probes; router hints need two or three. The per-probe timeout is
`PMTU_PROBE_TIMEOUT` (1 s, `structs.rs`), shorter than the 3 s the other
Enter tasks get, because a black-hole bisection pays it several times.

## Result and display

`Pmtu { mtu, blackhole, probes }` (`Display`: `1500` or
`1400 (PMTUD black hole)`). `PingTarget::determine_pmtu` stores it in the
`pmtu: RwLock<QueryResponse>` slot as `Count(mtu)` when clean, `Text(..)`
when a black hole was seen, `Error(..)` otherwise; `PingTarget::pmtu()`
reads it. The info pane grew by one row (`CON_NFO_T` 11 → 12).

## When it runs, and where

Enter (`Command::UpdateTgtInfo`) spawns `determine_pmtu` via
`AppState::spawn_blocking`, next to `determine_hops` — same reasons: it
does blocking socket I/O and finally takes a write lock on a target field
(see [keyboard-and-commands](keyboard-and-commands.md)). It is not run
automatically: several sized probes per target would be noisy for a CIDR
sweep, and the PMTU rarely changes.

## Caveats

- **Requires the same raw-socket privilege** as pinging (`CAP_NET_RAW` /
  root).
- **Targets that rate-limit or drop large pings** look like a black hole
  even when the path is fine. Firewalls that allow only small ICMP are the
  usual cause.
- **No probing above the kernel's route MTU** — jumbo paths are reported
  as the local interface MTU at most. On macOS the ceiling is 1500.
- **Asymmetric paths**: the reply must also make it back. A reply too big
  for the *return* path is fragmented by the target (replies don't carry
  DF), so this measures the forward path, like every other PMTUD.

## Testing

`cargo test pmtu` covers the packet classifiers (built by hand: echo
replies, Fragmentation Needed with and without a next-hop MTU, Packet Too
Big, unreachable, foreign and stale probes, truncated datagrams) and the
search against simulated paths (router hint, no hint, black hole, bogus
hints, IPv6 floor, silent target, broken floor, socket error, probe budget).
The wire path itself needs raw sockets and a real network, so run
`mping` on a target behind a known small-MTU link (a PPPoE or tunnel
uplink is the classic 1492 / 1280 case) and press Enter.

## File map

- `src/pmtu.rs` — `Pmtu`, `determine_pmtu`, `search`, `Prober`,
  `classify_v4/v6`, `set_dont_fragment`, `kernel_mtu`, tests.
- `src/hopcount/mod.rs` — the shared `make_socket`, `time_left`,
  `set_sockopt_int`, `embedded_echo_v4/v6` and header-size constants.
- `src/pingdata.rs` — `PingTarget::pmtu` field, `determine_pmtu`, `pmtu()`.
- `src/structs.rs` — `PMTU_PROBE_TIMEOUT`, the `spawn_blocking` in
  `update_target_info`.
- `src/strings.rs` — `*_PMTU_*` strings, the `INFO_TARGET` line.
- `src/lib.rs` — `pub use pmtu::{Pmtu, determine_pmtu}`.
