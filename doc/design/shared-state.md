# Shared state & locking conventions

The single shared object is `Arc<AppState>` (`src/structs.rs:43`). It owns
the targets list, the ICMP clients, the logger, the resolver, the quit flag,
the runtime handle, the key-event `Notify`, and various counters. Once
constructed it is read-only at the `Arc` level — interior mutability lives in
its fields.

## Two-phase construction

Construction is deliberately split in two:

```text
MpConfig::parse()  →  AppState::from_conf(&conf)  →  app.build(targets)?
                      (infallible)                   (returns Result<Arc<Self>>)
```

`from_conf` builds the bulk of the struct from already-parsed config. `build`
then sets up the IPv4 / IPv6 `surge_ping::Client`s, which can fail with EPERM
when raw sockets aren't permitted — `nice_permission_error`
(`src/utils.rs:64`) rewrites the message to point at `setcap`. Two phases
keep `from_conf` infallible while still letting client creation surface a
nice error, and `build` is the only function that wraps `self` in `Arc`.

## Locks

All locks are `parking_lot::RwLock` — no poisoning, faster than `std`. Atomics
default to `Ordering::Relaxed` since they only carry visibility, not
synchronization (quit, paused, stopped, perf, spawned-task counter).

Top-level shared fields and their lock granularity:

| Field | Type | Notes |
|---|---|---|
| `targets` | `RwLock<Vec<Arc<PingTarget>>>` | Read on every render; write only when adding/removing |
| `tasks` | `RwLock<Vec<JoinHandle<()>>>` | Written when spawning ping loops; read on shutdown to join |
| `quit` | `Arc<AtomicBool>` | Cloned to the signal thread; checked by every loop |
| `perf` | `AtomicBool` | Runtime toggle; see [concurrency](concurrency.md) |
| `spawned_tasks` | `AtomicU64` | Stat counter, `inc_spawned_tasks()` increments it on every spawn |
| `key_event` | `tokio::sync::Notify` | Keyboard thread → render loop immediate-redraw signal |
| `resolved` | `RwLock<Resolved>` | Name↔IP map, written by `collect_and_spawn`, read by `add_targets` |
| `logger` | `Arc<MessageBuffer>` | Internally `RwLock<VecDeque<Message>>`; see [logging](logging.md) |
| `runtime` | `tokio::runtime::Handle` | Captured during `from_conf`; the keyboard thread spawns through this |

Each `PingTarget` then has its own `RwLock<PingTargetInner>` plus a few
separate atomics — see [per-target-data](per-target-data.md).

## "Drop the lock early" invariant

The render path (`main.rs:205-393`) is deliberately ordered to release the
`targets` read lock as soon as the rendering function no longer needs it.
String formatting (`format_row`) only happens *after* a per-target snapshot
has been taken, so the inner-data write lock held by `ping_loop` doesn't
contend with display work. The comment at `main.rs:64` ("Do all the
(expensive) string formatting after releasing the lock") and the
documentation comment on `render_frame` (`main.rs:202`) call this out
explicitly — preserve that ordering when editing.

The same discipline applies in `gather_target_data`: it clones the
`Arc<PingTarget>` handles under a short-lived read guard and formats rows
only after the guard is dropped. This also means it never holds `targets`
while acquiring any other lock — see below.

## Lock-ordering invariant: `layout` before `targets`, or no nesting at all

Two threads take both the TUI `layout` lock and the `targets` lock: the
render path (`render_frame`: `layout` write → `targets` read) and the
keyboard thread. To keep that cycle-free:

- **Never hold `targets` while acquiring `layout`.** `gather_target_data`
  snapshots the target Arcs and drops the guard before it reads the
  viewport (which locks `layout`).
- **Never call `AppState::execute` while holding a `layout` guard.** Most
  command variants lock `targets`. The keyboard handlers read the selected
  index into a local first (dropping the guard at the end of the
  statement), dispatch the command, and only then re-acquire `layout` if
  they need to fix up selection state (see the Delete branch in
  `ui/keyboard.rs`).

Violating either rule re-creates a cross-thread ABBA deadlock that hangs
the UI permanently (the quit flag is checked under these same locks).

## CancellationToken per target

`PingTarget::cancel` is a `tokio_util::sync::CancellationToken`
(`pingdata.rs:190`). Stopping a target — either via `Command::StopTarget` or
during `Command::RemoveTarget` — calls `cancel.cancel()`, which `ping_loop`
observes via `tgt.is_stopped_async()` in its `select!` and exits cleanly. A
stopped target is irreversible; it never returns to active pinging.

This is cleaner than polling another atomic and avoids the "select branch
returns immediately forever" failure mode you'd hit by hand-rolling the
equivalent flag.

## Counters via atomic

`spawned_tasks` is purely informational (shown in the lower-right status
area) and uses `Relaxed`. Same for `logger.total` (total messages ever
pushed) in `MessageBuffer`. Don't tighten these orderings without a reason.
The one place that uses `SeqCst` is `toggle_perf` (`structs.rs:203`), where
the stronger ordering doesn't really buy correctness but signals "this is a
deliberate state flip" — `Relaxed` would have been fine there too.

## What `AppState::execute` does and does not do

`execute(cmd: Command) -> CmdResult` (`structs.rs:149`) is the single
dispatch point for commands from the UI. It is synchronous: each variant
acquires the locks it needs, performs the mutation, and returns. The
exceptions are `UpdateTgtInfo` (which fires-and-forgets two tasks via
`spawn_blocking` + `spawn`) and `RemoveAllUnreach` (which can affect many
targets and returns `CmdResult::Count(n)` so the caller can react).

Adding a new command means adding a variant to the `Command` enum
(`structs.rs:575`), a branch in `execute`, and a key in `ui/keyboard.rs` —
see [keyboard-and-commands](keyboard-and-commands.md) for the recipe.

## File map

- `src/structs.rs:43-417` — `AppState`, construction, command dispatch,
  target add/remove/stop/reset.
- `src/structs.rs:418-441` — `TargetDefaults`.
- `src/structs.rs:444-517` — `Resolved` map (name ↔ IP mappings).
- `src/main.rs:461-526` — wiring of construction and shutdown.
