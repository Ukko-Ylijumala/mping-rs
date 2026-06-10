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
- In perf mode ping results can complete out of order, so consecutive-miss
  counting (and thus outage edges) is approximate to within the inflight
  window (≤ 4 probes).

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
`StatsReset`. Each carries a `TimeSinceEpoch` wall-clock timestamp and
renders itself as a styled `Line` (`TargetEvent::as_line`) — red for
Down, green for Up, dim for lifecycle events.

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
  `OutageSummary`, the `update_stats` / pause / resume / stop / reset
  hooks, `PingTarget::outage_summary` / `recent_events`.
- `src/ui/tui.rs` — `events_popup`, `PopupContents::Lines`,
  `PopupContents::len`.
- `src/ui/keyboard.rs` — the `E` key arm.
- `src/main.rs` — the info-pane lines and the `Lines` popup render arm.
- `src/utils.rs` — `human_duration`.
