# Concurrency model & async ping loop

mping runs three execution contexts in parallel and coordinates them through
shared `Arc<AppState>`. Understanding which context a piece of code runs in is
load-bearing for any change in this area.

## The three contexts

| Context | What it does | Spawned at | File |
|---|---|---|---|
| Tokio multi-thread runtime (8 worker threads) | Per-target [`ping_loop`](../../src/pinger.rs)s, all async I/O, hop-count / PTR lookups | `#[tokio::main(worker_threads = 8)]` on `main` | `src/main.rs:461` |
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
(`pinger.rs:112`). It owns an `Interval` and a per-loop `FuturesUnordered`
buffer. The loop body is a `biased` `tokio::select!`:

```text
biased ordering (top wins on tie):
  1. quit flag        — break out
  2. target stopped   — break out
  3. inflight.next()  — drain a completed perf-mode future
  4. ticker.tick()    — try to issue the next ping
```

Keep that ordering when adding branches. The quit/stop checks must always win
over ticks so shutdown doesn't queue another wave of work first. The
`inflight.next()` branch is guarded by `if !inflight.is_empty()` so it is only
considered when the perf-mode buffer has something to drain.

When unpausing, the loop sets `next_ping = now` so the resumed target fires
immediately and then resumes at the normal cadence — no backlog catch-up
(`pinger.rs:138-144`).

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
  drives them all. The bound is computed as
  `ceil(timeout / interval).clamp(1, 4)` so you never accumulate more than four
  pending pings per target even with adversarial settings.

The mode is a runtime toggle, not a startup flag — F10 flips it live. That
means a user can react to observed jitter under load without restarting:
`Command::TogglePerf` (`structs.rs:159`) flips an `AtomicBool` that the loop
re-reads on every tick (`pinger.rs:149`).

The name is historical and arguably a misnomer; "low-jitter mode" or
"reduced-spawn mode" would describe it more accurately. The help text
("Try to be more performant by reducing task spawn overhead") describes the
mechanism, not the goal.

## Internal tick

`AppState::internal_tick` is the minimum scheduling granularity:
`min(100ms, interval)` (`structs.rs:99`). The render loop uses it as a no-op
"keep the select alive" branch (`main.rs:505`); the per-target ping loops use
it as their `Interval` period and gate work with `now >= next_ping`. This
indirection means a fast interval (e.g. 10 ms) still drives both render and
ping loops without two separate tickers.

## What runs where, by example

- A keystroke (e.g. pressing `R` to reset a target):
  keyboard `std::thread` → `app.execute(Command::ResetTgtStats(idx))`
  → synchronous mutation of `PingTarget` → `notify_one` → render loop runs.
- Pressing Enter to refresh target info:
  keyboard thread → `Command::UpdateTgtInfo(idx)` →
  `AppState::spawn_blocking(determine_hops)` (blocking ICMP) **and**
  `AppState::spawn(resolve_ptr)` (async DNS), both via the stored runtime
  handle (`structs.rs:318, 334`).
- A signal: signal thread → `quit.store(true)` → both the render loop and
  every `ping_loop` notice via `is_quitting_async`, break, and clean up.

## File map

- `src/main.rs:461-526` — the render loop and shutdown.
- `src/pinger.rs:112-168` — `ping_loop`.
- `src/pinger.rs:73-90, 92-107` — `build_ping_future` (perf path) and
  `ping_task` (default path).
- `src/structs.rs:43-209` — `AppState`, the runtime handle, `perf` atomic, the
  `spawn` / `spawn_blocking` passthroughs.
