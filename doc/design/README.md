# mping — design docs

These docs describe how mping is put together and *why* it's put together
that way. They are intended to be read alongside the source — every
section names the relevant files and line numbers — and to spare a new
reader from having to reverse-engineer the load-bearing details from
code.

## Index

| Doc | Subject |
|---|---|
| [concurrency.md](concurrency.md) | Three execution contexts (Tokio runtime, keyboard thread, signal thread), the per-target `ping_loop`, the `FuturesUnordered`-based perf/jitter mode and its runtime F10 toggle |
| [shared-state.md](shared-state.md) | `AppState`, two-phase construction, `parking_lot::RwLock` discipline, atomic ordering conventions, the `CancellationToken` per target |
| [per-target-data.md](per-target-data.md) | `PingTarget` / `PingTargetInner`, three-tier status derivation, `PacketHistory`, the flappy/lossy/laggy/unreachable heuristics |
| [outage-tracking.md](outage-tracking.md) | `EventTracker`: per-target outage accounting (declare/backdate/close semantics), time-based availability, the event timeline and its `E`-key popup |
| [latency-window.md](latency-window.md) | `LatencyWindow`: O(1) amortized rolling stats with monotonic min/max deques, computational-formula variance, all-time min and RFC 3550 smoothed jitter |
| [tui-rendering.md](tui-rendering.md) | The render loop, `AppLayout` and `maybe_update`, viewport-only formatting, non-shrinking columns, `LazyLock` widget statics |
| [keyboard-and-commands.md](keyboard-and-commands.md) | Keyboard thread vs Tokio split, the `Command` enum, `AppState::execute`, the `Notify`-driven immediate redraw, and how to add a new command |
| [signal-and-terminal.md](signal-and-terminal.md) | `TerminalGuard` RAII, panic hook, signal thread for SIGINT/SIGTERM/SIGQUIT, the alt-screen flag, `tput reset` recovery |
| [target-parsing.md](target-parsing.md) | IP / CIDR / range / DNS parsing pipeline, `collect_targets`, the `Resolved` name↔IP map, exclusion semantics, clamps |
| [add-target-dialog.md](add-target-dialog.md) | Modal dialog state, `tui-input` integration, focus cycle, async non-blocking submit path |
| [logging.md](logging.md) | `MessageBuffer` ring, syslog-style levels with `Trace`, level-based stderr gating, alt-screen aware output, F12 popup |
| [hopcount.md](hopcount.md) | Raw-ICMP TTL inference, the library function + standalone `hopcount` binary, why `spawn_blocking` and the AI-slop comment |
| [distance-estimation.md](distance-estimation.md) | RTT-to-distance heuristic, double stretch factor, quantization bands, caveats |

## Conventions across these docs

- File references are written as `src/path.rs:line` or `src/path.rs:N-M`
  for ranges. Open them in your editor — the line numbers are the
  authoritative source.
- Every doc includes a "File map" at the end with the most relevant
  source spans for that feature.
- When two docs touch the same code path, they link to each other rather
  than duplicating the explanation.

## Big picture in one paragraph

mping is a Tokio-based concurrent multi-pinger with a Ratatui TUI. Three
execution contexts run in parallel and coordinate through an
`Arc<AppState>`: per-target async ping loops on the Tokio runtime, a
render loop that's also on the runtime, and a keyboard event thread
that's a plain `std::thread` outside the runtime. All shared mutable
state is behind `parking_lot::RwLock`s; flags use atomics with
`Ordering::Relaxed`. The TUI renders only the visible viewport per
frame, lets columns grow but never shrink, and wakes immediately on
keyboard input via a `Notify`. Targets can be added, paused, stopped,
removed, or reset at runtime; targets-out-of-flight performance can be
toggled live with F10. Terminal cleanup is RAII-guarded with a panic
hook fallback and three signal handlers. The rolling RTT window is
O(1)-amortized via monotonic min/max deques. None of this is novel — it's
all carefully chosen off-the-shelf pieces glued together with attention
to a few load-bearing invariants. These docs name those invariants.
