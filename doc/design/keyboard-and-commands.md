# Keyboard handling & command dispatch

Keyboard input is polled by a dedicated `std::thread` (not a Tokio task),
translated into `Command` variants, and dispatched through
`AppState::execute`. The render loop is woken via a `tokio::sync::Notify` so
the screen refreshes immediately instead of waiting for the next tick.

## The keyboard thread

`key_event_handler` (`src/ui/keyboard.rs:18`) is a plain
`std::thread::spawn`ed function that runs the polling loop until `quit` is
set:

```text
while !state.is_quitting() {
    if key_event_poll(50ms, ...).is_ok_and(|handled| handled) {
        state.key_event.notify_one();
    }
}
```

50 ms is a balance between responsiveness and CPU cost — fast enough that
typing feels native, slow enough that an idle session uses negligible CPU.
The notify wakes the main loop's `key_event.notified()` branch
(`src/main.rs:498`) for an immediate redraw.

### Why a `std::thread` and not a Tokio task?

Crossterm's event API (`event::poll` + `event::read`) is blocking. We
*could* run it on Tokio's blocking pool, but a dedicated OS thread is
simpler, lets the polling happen continuously without consuming a
runtime worker, and isolates input from any future `block_in_place`
shenanigans on the runtime. The cost is that the thread runs outside the
runtime, which has implications below.

### Spawning async work from the keyboard thread

Calling `tokio::spawn` from outside a runtime panics. Any command that
needs to do async work (e.g. `UpdateTgtInfo` triggers PTR resolution and
hop-count probing) must go through `AppState::spawn` or
`AppState::spawn_blocking`, both of which use the stored
`tokio::runtime::Handle` captured at `from_conf` time
(`structs.rs:105, 165-182`).

There is a load-bearing comment block at `structs.rs:328-332` explaining
this for `update_target_info`. **Don't** replace those calls with bare
`tokio::spawn` — the keyboard thread will panic on the first press of
Enter.

`spawn_blocking` is specifically required for `determine_hops`
(`structs.rs:318`) because it does blocking socket I/O *and* eventually
acquires write locks on the target's fields. Scheduling it on a runtime
worker that's also holding a read lock on the same target can deadlock.

## The Command enum and execute

`Command` (`structs.rs:575-596`) is the contract between the UI and the
state mutations it can trigger:

| Variant | Effect | Notes |
|---|---|---|
| `Quit` | Sets the quit flag | Returns `CmdResult::ByeBye` |
| `PauseAll` / `ResumeAll` | Mutates every target's paused flag | |
| `TogglePause(idx)` | Toggles one target | Re-uses the inner write lock to record `Paused`/`Resuming` status |
| `StopTarget(idx)` | Cancels the target's `CancellationToken` | Irreversible |
| `RemoveTarget(idx)` | Stop + remove from the targets vec | Takes a write lock on `targets` |
| `UpdateTgtInfo(idx)` | Fires hop-count (blocking) + PTR (async) tasks | See above |
| `ResetTgtStats(idx)` | Zeroes counts and clears the latency window / history | |
| `TogglePerf` | Flips the `perf` atomic | See [concurrency](concurrency.md) |
| `RemoveAllUnreach` | Bulk-removes unreachable targets | Returns `CmdResult::Count(n)` so the keyboard handler can clear the selection |

`AppState::execute(cmd)` (`structs.rs:149-162`) is a simple match dispatch.
It is synchronous — anything async-y inside individual handlers happens via
`spawn` / `spawn_blocking` from inside the handler itself.

## Key bindings (current)

| Key | Action |
|---|---|
| `q`, Ctrl-C | Quit (Ctrl-C only because raw mode swallows the SIGINT) |
| ↑ / ↓ | Move row selection |
| ← / → | Move column selection |
| PageUp / PageDown | Scroll one viewport (or scroll popup if visible) |
| Shift+PageUp/Down | Scroll 10 rows |
| Home / End | First / last row |
| Backspace | Clear table selection |
| Space | Toggle pause on selected target |
| `p` / `P` | Pause all / Resume all |
| `S` | Stop selected target |
| `R` | Reset selected target's stats |
| Enter | Update info (hops + PTR) for selected target |
| Delete | Remove selected target |
| Ctrl-Delete | Remove all unreachable targets |
| `a` | Open add-target dialog |
| Esc | Close active modal (help / popup / dialog) |
| F1 | Toggle help popup |
| F10 | Toggle perf mode |
| F12 | Toggle log popup |

Definitions live in `src/ui/keyboard.rs:37-235`. After handling an event,
the loop drains any backed-up events with `event::poll(0)`
(`keyboard.rs:221`) so a held-down key doesn't queue dozens of redundant
notifies.

## The add-target dialog branch

When the add-target dialog is visible (`tui.layout.read().input_visible`),
the key handler routes the event to `handle_input_dialog`
(`keyboard.rs:238`) instead of the main match. That handler delegates to
`AddTargetDialogState::on_key` and reacts to the returned `DialogAction`.
See [add-target-dialog](add-target-dialog.md).

## Adding a new keyboard command

1. Add a variant to `Command` in `src/structs.rs`.
2. Handle it in `AppState::execute` (`structs.rs:149`). If it needs async
   work, use `self.spawn` / `self.spawn_blocking`.
3. Add a key arm in `key_event_poll`'s match (`src/ui/keyboard.rs:45`).
   Call `app.execute(Command::YourVariant)`.
4. If your command can change the table contents in a way that affects
   row selection or column widths, mutate `tui.layout` (e.g.
   `lo.reset_table_widths()` after a removal — see the `Delete` arm at
   `keyboard.rs:143`).

The main loop's `key_event` `Notify` causes the immediate redraw — no need
to wake it manually.

## File map

- `src/ui/keyboard.rs` — the thread, the polling helper, the input-dialog
  branch.
- `src/structs.rs:149-162` — `AppState::execute`.
- `src/structs.rs:575-607` — `Command` and `CmdResult`.
- `src/main.rs:485-507` — thread spawn + the main loop's `Notify` branch.
