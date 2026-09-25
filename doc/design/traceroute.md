# Traceroute with AS annotation

`T` on a selected target runs an ICMP traceroute to it and shows the hops
in a popup as they are found, each one annotated with its PTR name and
origin AS. It is the fourth of the on-demand per-target probes, after hop
count, path MTU and the origin AS of the target itself, and shares their
plumbing: the raw-socket helpers of [hopcount](hopcount.md), the
[Cymru lookup](as-lookup.md), and the `spawn_blocking` rules of
[keyboard-and-commands](keyboard-and-commands.md).

## Probing (`src/traceroute.rs`)

`trace_route(target, timeout, max_hops, debug, on_hop)` sends one Echo
Request per TTL, 1 upwards, on a raw ICMP socket whose TTL is set before
each send (`IP_TTL` / `IPV6_UNICAST_HOPS` via the shared `set_sockopt_int`).
For each TTL it waits up to `timeout` and classifies what comes back:

| Answer | Meaning | `HopKind` |
|---|---|---|
| Time Exceeded (v4 type 11 / v6 type 3) quoting our probe | the router at that distance | `Router` |
| Echo Reply from the target with our ident + seq | done | `Target` |
| Destination Unreachable quoting our probe | dead end | `Unreachable` |
| nothing | silent hop (`*`) | `Silent` |

The run ends on `Target` or `Unreachable`, at `max_hops` (30), or after
`MAX_SILENT` (10) consecutive silent hops — a target that never answers
would otherwise cost the full 30 timeouts. Every hop, silent ones
included, is handed to `on_hop` immediately; returning `false` from it
aborts the run (used when the target is stopped or removed meanwhile).

### Telling our answers apart

A raw ICMP socket sees every ICMP packet on the host: mping's own pings,
hopcount and PMTU probes, and — the new case — **other traceroutes running
at the same time**, whose routers send Time Exceeded messages of exactly
the same shape. The tests in `traceroute.rs` pin down the filter:

- The identifier is **random per run** (`rand::random`), not a fixed
  constant like hopcount's `0xb00b` or PMTU's `0x4d54`.
- The sequence number is the TTL, and an error must quote the current one.
- An error must also quote **our target as the destination** of the
  embedded packet (`embedded_dst_v4/v6`, offset 16 / 24 of the quoted IP
  header). Two runs with the same TTL in flight can then never accept
  each other's Time Exceeded even if their random idents collided.
- An Echo Reply counts only from the target's own address.

### Paris-style constant checksum

ECMP routers that balance ICMP usually hash on the checksum field, in
place of the ports they'd use for TCP/UDP. If the checksum changes with
every probe (it normally does, since the sequence number is under it),
consecutive TTLs can take different paths and the trace shows a mix of
two routes. `build_probe` sets the first payload word to `!seq`: in ones'
complement arithmetic `seq + !seq == 0xffff == −0`, so the checksum is the
same for every probe of a run. The `checksum_is_constant_across_probes`
test verifies it over 64 sequence numbers. The same word is set for IPv6,
where the kernel computes the checksum, with the same effect.

## Per-target state (`src/pingdata.rs`)

```text
PingTarget.trace: RwLock<TraceState>
    ├── hops: Vec<TraceHopInfo>     ── in TTL order, pushed as found
    │       ├── hop: TraceHop       ── ttl, addr, rtt, kind
    │       ├── name: QueryResponse ── PTR (None = pending, Empty = no PTR)
    │       └── asinfo: QueryResponse ── Cymru origin AS (As / TextStr / ...)
    ├── status: None | Running | Done(TraceEnd) | Error(String)
    └── started: Option<TimeSinceEpoch>
```

`trace_begin` returns `false` if a run is already in progress (the key is
then just a "show it" key); otherwise it discards the previous run.
`trace_push` appends a hop, `trace_annotate(ttl, ..)` fills in the names
later, `trace_finish` closes the run. `trace_lines()` renders the whole
thing for the popup.

## Command path (`src/structs.rs`)

`Command::Traceroute(idx)` → `AppState::traceroute_target`. The probing
goes on a blocking thread through `AppState::spawn_blocking`, for the same
reasons as `determine_hops`: blocking socket I/O and write locks on target
fields, with the keyboard thread as the caller. Inside the `on_hop`
callback, each answering hop spawns one async task onto the stored runtime
handle that runs the PTR lookup (`utils::lookup_ptr`) and the Cymru lookup
(`asinfo::lookup_as`) concurrently via `tokio::join!` and writes them back
with `trace_annotate`. The callback also checks `is_stopped()` and aborts
the run on a stopped or removed target.

No caching across hops or targets: adjacent hops in the same AS repeat the
AS-name query, which the resolver caches anyway.

## The popup (`src/ui/`, `src/main.rs`)

The event timeline popup (`E`) is a snapshot taken at key press. A
traceroute fills in over many seconds, so its popup must re-render:
`PopupContents::Trace(Arc<PingTarget>)` holds the target and its
`to_list` / `to_para` / `len` call `trace_lines()` on every frame while the
popup is open. Rendering shares the `Lines` arm in `render_popups`
(stateful list with scrollbar when taller than the popup), and Esc / `q`
close it like any popup, dropping the `Arc`.

The `T` key arm in `keyboard.rs` first executes the command (starting a run
if none is in progress), then opens the popup on the target. Line layout:

```text
Traceroute to 8.8.8.8 (dns.google) - running...
started 2026-09-25 10:12:03

 1      0.412 ms  192.168.1.1     router.lan             -
 2      8.921 ms  100.64.0.1      -                      -
 3      9.105 ms  62.78.1.1       core1.hel.example.net  AS1759 TSF-IP-CORE, FI
 4  *
 5     12.331 ms  8.8.8.8         dns.google             AS15169 GOOGLE - Google LLC, US
```

Address and name columns size to their content (name capped at 40); the
AS column is the `AsInfo::short()` form (ASN + name, no prefix), cyan; the
target line is bold green, an unreachable line red, pending annotations
show as `...`. The title carries the run status: `running...`, the
`TraceEnd` text (`target reached`, `hop limit reached`, ...) or an error.

## Caveats

- **ICMP only.** No UDP/TCP probe modes; a path that filters ICMP echo
  towards the target but passes it to routers shows the routers and then
  silence until the give-up limit.
- **One probe per hop.** Fast, but a single lost probe shows as `*`. Press
  `T` again after the run for a fresh one.
- **Return-path asymmetry**: as with every traceroute, the RTT includes
  the router's own return path and its ICMP rate limiting.
- **Private / unrouted hop addresses** get no AS (`-`), which is expected.

## Testing

`cargo test traceroute` covers the constant checksum, the v4/v6
classifiers against hand-built Time Exceeded / Echo Reply / Unreachable
datagrams (including another run's ident, another target's quoted
destination, a stale sequence number and truncated packets). The wire
path needs raw sockets: run `mping 8.8.8.8`, select it, press `T`.

## File map

- `src/traceroute.rs` — `trace_route`, `TraceHop`, `HopKind`, `TraceEnd`,
  `build_probe`, `classify_v4/v6`, `embedded_dst_v4/v6`, `Prober`, tests.
- `src/pingdata.rs` — `TraceState`, `TraceStatus`, `TraceHopInfo`,
  `PingTarget::trace_*`, `trace_lines`.
- `src/structs.rs` — `Command::Traceroute`, `traceroute_target`,
  `TRACE_PROBE_TIMEOUT`, `TRACE_MAX_HOPS`.
- `src/ui/tui.rs` — `PopupContents::Trace`.
- `src/ui/keyboard.rs` — the `T` key arm.
- `src/main.rs` — the shared `Lines | Trace` render arm.
- `src/utils.rs` — `lookup_ptr`.
- `src/asinfo.rs` — `AsInfo::short`.
- `src/strings.rs` — `TRACE_*`, `INFO_TRACE*`, the help table row.
