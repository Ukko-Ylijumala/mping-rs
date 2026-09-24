# Outage tracking & the per-target event timeline

Each target keeps its own outage accounting and a bounded event timeline
in `EventTracker` (`src/pingdata.rs`), a field of `PingTargetInner`. All
mutations happen under the target's `data` write lock, which the ping
result path (`update_stats`) and the pause/resume/stop paths already
hold — the tracker adds no new locking.

## Outage semantics

- An **outage** is declared after `OUTAGE_THRESH` (3) consecutive missed
  probes. Shorter blips never become outages — per-packet loss already
  covers those in the Loss column.
- The outage **start is backdated to the send time of the first missed
  probe**, not the moment the third timeout fired. The `Down` event's
  wall-clock timestamp is backdated the same way (`TimeSinceEpoch -
  Duration`, which `timesince` supports).
- Any successful reply ends the outage. Its duration is measured between
  probe *send* times (first missed → first answered), so the configured
  timeout doesn't inflate it.
- Pausing or stopping a target closes an ongoing outage at that moment.
- Results are only accounted if the target is monitored and the probe was
  sent after the tracker's `epoch` (creation, last resume, last stats
  reset). Pings still in flight across a pause/stop/reset would otherwise
  open outages (and "DOWN for X" counters) for time nobody is monitoring.
  Packet counters (sent/recv/loss) are unaffected — a reply is a reply.
- A send the OS refuses (`SurgeError::IOError`: no route, network down)
  counts as a miss, so a dead uplink shows up as an outage. Other errors
  (`IdenticalRequests` etc.) are internal and ignored here, so they can't
  fake one. Such probes still don't count as *sent* (see
  [per-target-data](per-target-data.md)).
- In perf mode ping results can complete out of order, so consecutive-miss
  counting (and thus outage edges) is approximate to within the inflight
  window (≤ 5 probes).

## Availability

`EventTracker::summary()` produces an `OutageSummary` with a time-based
availability estimate: `1 − downtime / monitored`, where *monitored*
excludes paused and stopped time (tracked via `pause_since` /
`paused_total`; a stop is accounted as an indefinite pause). An ongoing
outage counts toward downtime. The estimate is withheld (`None` → "-")
until at least 500 ms of monitored time exists.

Note this is *time*-based availability, distinct from the packet-based
Loss column: a target that drops every other packet has 50% loss but no
outages and ~100% availability.

## The event timeline

A `VecDeque<TargetEvent>` ring capped at `EVENT_CAP` (100) entries.
Event kinds: `Down`, `Up(duration)`, `Paused`, `Resumed`, `Stopped`,
`StatsReset`, `RouteChange { from, to }`. Each carries a `TimeSinceEpoch`
wall-clock timestamp and renders itself as a styled `Line`
(`TargetEvent::as_line`) — red for Down, green for Up, yellow for a route
change, dim for lifecycle events.

## Route-change events from the reply TTL

Every IPv4 echo reply carries the IP header TTL, which surge-ping exposes
(`Icmpv4Packet::get_ttl`). The number of hops the reply crossed is
`initial − received` (the same bucketing as [hopcount](hopcount.md),
`estimate_hops`), so a TTL that changes mid-run means the return path —
or, rarely, the far end's initial TTL — changed. `update_stats` feeds the
TTL to `EventTracker::record_ttl` right after `record_success`:

- The first TTL seen in an epoch is remembered silently (`TtlUpdate::First`).
- A differing TTL becomes a candidate. Only when it has been seen on
  `TTL_CONFIRM` (2) **consecutive** replies is a `RouteChange { from, to }`
  event pushed (`TtlUpdate::Changed`) and the remembered TTL replaced. One
  stray reply over another ECMP path is therefore ignored, and a
  per-packet alternation between two paths of different length never
  confirms at all (each reply resets the other's count). A 2-2-2
  alternation would still flap; that is a real, visible path instability.
- Replies while paused or from before the epoch are ignored like every
  other result. The remembered TTL **survives pause/resume** (a path that
  changed while paused shows up after resume) but **not a stats reset**.
- IPv6 replies carry no TTL here — surge-ping's `Icmpv6Packet` never
  fills the hop limit — so v6 targets get no route events and their hop
  count stays Enter-only.

`TtlUpdate::First` and `Changed` also refresh the target's on-demand
`hops` slot with `estimate_hops(ttl)`, so for IPv4 the "Hops" info line is
live from the first reply and follows route changes without pressing
Enter. That write happens **after** `update_stats` drops the `data` lock:
`hops` is a separate `RwLock` and nothing may nest the two.

The event line reads `ROUTE - reply TTL 57 -> 55 (est. hops 7 -> 9)`.

A stats reset (`R`) clears the tracker along with everything else and
leaves a `StatsReset` marker as the first event of the new epoch.

## UI surfaces

1. **Info pane** (selected target): two lines appended to the upper info
   text — `Outages : 2 (max 40.0s, sum 55.2s)` (or `1 - DOWN for 35s!`
   while ongoing) and `Uptime : 99.876%`. `CON_NFO_T` grew 7 → 9 rows to
   fit (`ui/tui.rs`).
2. **Event popup** (`E` key): shows the selected target's timeline,
   oldest first, via `events_popup(...)` (`ui/tui.rs`) and the new
   `PopupContents::Lines` variant (pre-styled lines). Rendering is
   stateful when the timeline exceeds the popup height, so the existing
   PageUp/PageDown popup scrolling and scrollbar work unchanged. `E` is
   pure UI — it reads target data and fills the popup; there is no
   `Command` involved.

## File map

- `src/pingdata.rs` — `EventTracker`, `TargetEvent`, `EventKind`,
  `TtlUpdate`, `OutageSummary`, the `update_stats` / pause / resume /
  stop / reset hooks, `record_ttl`, `PingTarget::outage_summary` /
  `recent_events`.
- `src/hopcount/mod.rs` — `estimate_hops` (shared with the route-change
  event rendering and the live hops refresh).
- `src/ui/tui.rs` — `events_popup`, `PopupContents::Lines`,
  `PopupContents::len`.
- `src/ui/keyboard.rs` — the `E` key arm.
- `src/main.rs` — the info-pane lines and the `Lines` popup render arm.
- `src/utils.rs` — `human_duration`.
