# Concurrency model & async ping loop

mping runs three execution contexts in parallel and coordinates them through
shared `Arc<AppState>`. Understanding which context a piece of code runs in is
load-bearing for any change in this area.

## The three contexts

| Context | What it does | Spawned at | File |
|---|---|---|---|
| Tokio multi-thread runtime (8 worker threads) | Per-target [`ping_loop`](../../src/pinger.rs)s, all async I/O, hop-count / PTR / AS lookups | `#[tokio::main(worker_threads = 8)]` on `main` | `src/main.rs:461` |
| Keyboard event `std::thread` | Polls crossterm events, dispatches `Command`s, wakes the render loop via `Notify` | `thread::spawn(move \|\| key_event_handler(...))` | `src/main.rs:485` |
| Signal `std::thread` | Listens for SIGINT/SIGTERM/SIGQUIT, flips the quit `AtomicBool` | `std::thread::spawn` inside `setup_signal_handler` | `src/utils.rs:48` |

The render loop itself runs on the Tokio runtime (it is the body of `main`), but
it's worth treating as a fourth "logical" actor — see
[tui-rendering](tui-rendering.md).

The keyboard thread is **not** inside the Tokio runtime. Anything it triggers
that needs to do async work has to go through `AppState::spawn` /
`AppState::spawn_blocking`, which use the stored `tokio::runtime::Handle`
captured during `from_conf` (`structs.rs:105`). There is a load-bearing comment
in `update_target_info` (`structs.rs:328`) explaining why a bare `tokio::spawn`
there would panic the keyboard thread — don't paper over it.

## Per-target ping loop

`ping_loop(tgt, app)` runs as a Tokio task per `PingTarget`
(`pinger.rs`). It owns an `Interval` running at the **ping interval itself**
and a per-loop `FuturesUnordered` buffer. The loop body is a `biased`
`tokio::select!` in which every branch is a real wakeup source — the loop
sleeps until one of them fires:

```text
biased ordering (top wins on tie):
  1. app.shutdown.cancelled()  — break out (quit)
  2. tgt.stopped()             — break out (target cancelled)
  3. inflight.next()           — drain a completed perf-mode future
  4. tgt.resumed(), if paused  — re-arm the ticker (ping immediately)
  5. ticker.tick(), if !paused — issue the next ping
```

Keep that ordering when adding branches. The quit/stop checks must always win
over ticks so shutdown doesn't queue another wave of work first. The
`inflight.next()` branch is guarded by `if !inflight.is_empty()` so it is only
considered when the perf-mode buffer has something to drain.

Scheduling details:

- The ticker uses `MissedTickBehavior::Skip`: after a stall (SIGSTOP, an
  overloaded runtime) one ping goes out late and the cadence snaps back to
  its grid — no burst of catch-up pings.
- While paused the ticker isn't polled at all. `PingTarget::resume` /
  `toggle_pause` call `Notify::notify_one`, which stores a permit if the
  loop isn't waiting yet, so a resume racing the loop's `is_paused()` read
  isn't lost (a stale permit costs one spurious wakeup). On wakeup the loop
  calls `ticker.reset_immediately()`: the resumed target fires at once and
  then continues at the normal cadence.
- Shutdown and stop are awaited as `CancellationToken`s, not polled flags,
  so even a 10 s interval loop exits immediately.

This replaced an earlier design that polled every target on a shared 100 ms
"internal tick" and gated work with `now >= next_ping`: it quantized send
times to the tick (a 250 ms interval went out as 300/200 ms gaps), woke
every target 10×/s regardless of its interval, and burst catch-up pings
after a stall.

When the loop exits for any reason *other than* quit, it drains outstanding
inflight pings so their stats get recorded. On quit it abandons them —
otherwise shutdown would stall up to `timeout` per target (`pinger.rs:163-167`).

## "Perf" mode — really a latency-stability mode

Despite the CLI flag name (`--perf`, F10 to toggle), this mode is **not** about
raw throughput. Its real purpose is to keep ping cadence smooth when there are
many targets in flight, by avoiding the per-ping `tokio::spawn` overhead that
otherwise dominates when the runtime is busy.

- **Default mode** (`app.perf() == false`): each tick calls
  `ping_task(...).await` (`pinger.rs:156`), which in turn `app.spawn`s a new
  Tokio task. One task per ping. Simple, but task setup/teardown adds up at
  high target counts.
- **Perf mode** (`app.perf() == true`): each tick pushes a future into a
  per-target `FuturesUnordered` of size up to `max_inflight`
  (`pinger.rs:149-154`). No `spawn` per ping — the existing `ping_loop` task
  drives them all. The bound comes from `max_inflight()`:
  `floor(timeout / interval) + 1`, clamped to `1..=5`. The `+ 1` matters at
  integer ratios (the default 2 s / 1 s): the oldest ping times out a hair
  *after* the tick that wants its slot, so a `ceil`-based bound skipped every
  third probe to an unresponsive target. Args caps the timeout at 4 intervals,
  so the clamp never binds with valid settings.

The mode is a runtime toggle, not a startup flag — F10 flips it live. That
means a user can react to observed jitter under load without restarting:
`Command::TogglePerf` (`structs.rs:159`) flips an `AtomicBool` that the loop
re-reads on every tick (`pinger.rs:149`).

The name is historical and arguably a misnomer; "low-jitter mode" or
"reduced-spawn mode" would describe it more accurately. The help text
("Try to be more performant by reducing task spawn overhead") describes the
mechanism, not the goal.

## Internal tick

`AppState::internal_tick` is `min(100ms, interval)` (`structs.rs`). Only the
render loop uses it now, as its no-op "keep the select alive" branch — it
bounds how quickly the render loop re-evaluates its refresh deadline (and
notices a flag-only quit; normal quits cancel `AppState::shutdown` and wake
it at once). The ping loops no longer use it (see above).

## What runs where, by example

- A keystroke (e.g. pressing `R` to reset a target):
  keyboard `std::thread` → `app.execute(Command::ResetTgtStats(idx))`
  → synchronous mutation of `PingTarget` → `notify_one` → render loop runs.
- Pressing Enter to refresh target info:
  keyboard thread → `Command::UpdateTgtInfo(idx)` →
  `AppState::spawn_blocking(determine_hops)` (blocking ICMP) **and**
  `AppState::spawn(resolve_ptr)` / `AppState::spawn(resolve_as)` (async
  DNS), all via the stored runtime handle (`structs.rs`).
- A signal: signal thread → `app.execute(Command::Quit)` → `AppState::quit`
  sets the flag and cancels `AppState::shutdown`, which wakes the render
  loop and every `ping_loop` at once. The `q` key, Ctrl-C and the panic
  hook all take this same path.

## File map

- `src/main.rs:461-526` — the render loop and shutdown.
- `src/pinger.rs:112-168` — `ping_loop`.
- `src/pinger.rs:73-90, 92-107` — `build_ping_future` (perf path) and
  `ping_task` (default path).
- `src/structs.rs:43-209` — `AppState`, the runtime handle, `perf` atomic, the
  `spawn` / `spawn_blocking` passthroughs.
