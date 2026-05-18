# Per-target data model & status derivation

Each ping target is represented by a `PingTarget` (`src/pingdata.rs:185`)
that wraps an `RwLock<PingTargetInner>` plus a handful of separately-atomic
fields. Stats are mutated by the per-target `ping_loop` and read by the
render loop; the layout is structured so the contended `data` lock is only
held for short critical sections.

## Two layers: outer flags, inner statistics

```text
PingTarget (immutable shape)
├── addr: IpAddr               ── what we're pinging
├── rev: String                ── pre-computed PTR-style name (in-addr.arpa / ip6.arpa)
├── hostname: OnceLock<Arc<str>> ── set once from DNS resolution
├── paused: AtomicBool         ── flips fast in the ping loop's select!
├── cancel: CancellationToken  ── permanent stop; see shared-state.md
├── hops:    RwLock<QueryResponse> ── last hop-count result (separate, rarely updated)
├── ptr:     RwLock<QueryResponse> ── last PTR result
├── rev_ptr: RwLock<QueryResponse> ── reverse-of-PTR result
└── data: RwLock<PingTargetInner>
        ├── sent: u64
        ├── recv: u64
        ├── rtts: LatencyWindow    ── rolling RTT window (µs), see latency-window.md
        ├── recent: PacketHistory  ── short detailed history (default 100 packets)
        ├── raw_status: PingStatus ── only Ok / Timeout / Error(_) live here
        ├── last_seq: u16          ── most recent sent sequence number
        └── last_sent: Option<Instant>
```

Splitting `paused` and `cancel` out of the inner `data` lock is intentional —
the hot select in `ping_loop` checks them every tick, so they're behind their
own cheap atomic/token rather than fighting for `data`.

## `sent` is incremented before the network call

`mark_sent_and_next_seq` (`pinger.rs:28-46`) increments `data.sent` *before*
calling `pinger.ping(...)`. This way the sent counter stays accurate even if
the ping returns an error or the response arrives out of order. The one
exception: if `surge_ping` returns a non-timeout error, `update_stats`
decrements `sent` again (`pingdata.rs:250`) because in that case no packet
ever made it onto the wire.

The 16-bit sequence wraps at 65 536 (`pinger.rs:40`), which is the protocol
limit for ICMP Echo. The wrap is handled with `sent % 65_536` so a long-lived
target keeps producing valid sequence numbers indefinitely.

## Three tiers of status

The displayed status is computed in three layers. The raw status stored in
`PingTargetInner.raw_status` only ever takes one of:

- `PingStatus::Ok` — last reply received
- `PingStatus::Timeout` — last call returned `SurgeError::Timeout`
- `PingStatus::Error(String)` — any other surge error

The **effective status** is derived from `raw_status` plus recent history
analysis in `PingTargetInner::effective_status` (`pingdata.rs:148`):

- If `Timeout` and we look unreachable → `NotReachable`
- Otherwise, on top of `Ok`/`Timeout`, check in order: `Flappy`, `Lossy`,
  `Laggy`. First match wins.
- Other raw states pass through.

The **outer status** layered on top (`PingTarget::effective_status` at
`pingdata.rs:499`) overrides everything with `Stopped` if cancelled, then
`Paused` if paused. So the precedence is `Stopped > Paused > NotReachable >
Flappy > Lossy > Laggy > raw`.

## The flappy/lossy/laggy heuristics

All three look at the most recent `DEFAULT_WIN = 10` packets in
`PacketHistory`:

| State | Predicate | Threshold |
|---|---|---|
| `Flappy` | `recent_transitions(10) >= 4` | `FLAP_THRESH = 4` up/down flips in last 10 |
| `Lossy`  | `losses(10) / 10 >= 0.5` | `LOSSY_THRESH = 0.5` (≥ 50 % loss) |
| `Laggy`  | `mean(last 10) > LAGGY_FACTOR × overall_mean` | `LAGGY_FACTOR = 2.0` |

All four constants live in `pingdata.rs:30-37` if you need to tune them.

## "Unreachable"

`PingTargetInner::is_unreachable` (`pingdata.rs:127`) returns true if:

- We've sent more than `DEFAULT_WIN` packets and received zero replies, **or**
- The last `min(50, recent.len())` packets all timed out.

`Error` / `Paused` / `Resuming` raw statuses are explicitly *not* treated as
unreachable (we don't know that the network is broken, only that something
about the local socket call failed, or the user paused us).

Unreachable targets get a distinctive `light_red` row style in the TUI and
are the target of `Ctrl-Delete` (`Command::RemoveAllUnreach`).

## PacketHistory — recent detailed view

`PacketHistory` (`pingdata.rs:701-863`) is a fixed-capacity `VecDeque` of
`PacketRecord`s. Each record stores the sequence number, the send `Instant`,
and an optional RTT. The capacity defaults to 100 (`--detailed`) and exists
to support the recent-trend heuristics above. The rolling RTT window for
graphing / min / max / stdev is the separate `LatencyWindow` — see
[latency-window](latency-window.md).

`PacketHistory` exposes:

- `loss()` — overall loss ratio across the buffer
- `recent_losses(n)` — count of unanswered packets in the last *n*
- `recent_transitions(n)` — up/down state flips in the last *n*
- `min` / `max` / `mean(window?)` — these scan the buffer, so they are O(n)
  in the detailed window. `mean(Some(n))` errors if `n > len()`.
- `timespan()` — total wall-clock duration from oldest to newest sent

## StatsSnapshot

`StatsSnapshot::new_from(&tgt, timeout)` (called from `format_row` at
`main.rs:58`) grabs a single short read lock and copies out the data the
renderer needs: sent/recv counts, latency window summary, effective status.
This keeps the lock held for one short critical section per visible target,
which matters when there are many targets in the table.

## File map

- `src/pingdata.rs:39-71` — `PingStatus` enum and Display.
- `src/pingdata.rs:73-168` — `PingTargetInner`, the predicates, and
  `effective_status`.
- `src/pingdata.rs:185-635` — `PingTarget` (pause/resume/stop/reset, status
  layering, hop/PTR display, distance — see
  [distance-estimation](distance-estimation.md)).
- `src/pingdata.rs:639-697` — `PacketRecord`.
- `src/pingdata.rs:701-863` — `PacketHistory`.
- `src/pinger.rs:28-46` — `mark_sent_and_next_seq` (sent-before-network
  invariant).
