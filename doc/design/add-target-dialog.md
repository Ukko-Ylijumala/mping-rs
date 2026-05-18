# Add-target input dialog

The `a` key opens a modal dialog for adding targets at runtime. It collects
addresses, exclusions, and a "start paused?" flag, then hands the strings
to `pinger::collect_and_spawn` to be parsed, resolved, and pinged — all
without blocking the keyboard or render loops.

## Pieces

| Type | Where | Role |
|---|---|---|
| `AddTargetDialogState` | `src/ui/input.rs:39` | The mutable state (two `Input`s, a bool, a focused-field enum, an optional error string) |
| `AddTgtDialog` | `src/ui/input.rs:150+` | The `StatefulWidget` Ratatui renders against `AddTargetDialogState` |
| `ActiveField` | `src/ui/input.rs:19` | Which field has focus: `Addresses`, `Exclusions`, `Paused`, `Submit`, `Cancel` |
| `DialogAction` | `src/ui/input.rs:29` | `None` / `Redraw` / `Cancel` / `Submit { addrs, excls, paused }` — what the keyboard branch reacts to |

`tui-input` provides the text-editing primitive for the two text fields
(arrows, backspace, char insert, etc.); we route crossterm `KeyEvent`s to
it inside `on_key` (`input.rs:91-97`).

## Routing & focus

When `tui.layout.read().input_visible` is true,
`key_event_poll` routes every key to `handle_input_dialog`
(`keyboard.rs:42`). That helper calls
`AddTargetDialogState::on_key(key) -> DialogAction` and acts on the
result:

- `None` — no state change, don't even notify the render loop.
- `Redraw` — state changed (focus moved, character typed, paused
  toggled). Notify the loop.
- `Cancel` — Esc pressed (and no help popup on top of the dialog) →
  `tui.add_tgt_dialog_close()`.
- `Submit { addrs, excls, paused }` — Enter on the Submit button →
  parse the input, fire-and-forget async work, close the dialog.

The focus order is a simple cycle: Addresses → Exclusions → Paused →
Submit → Cancel → Addresses (`input.rs:107-125`). Tab moves forward,
Shift-Tab backward. Space toggles the Paused checkbox only when focus is
on it (`input.rs:77-82`). Enter while a text field has focus is
deliberately a no-op — you can't accidentally submit by hitting Enter
mid-typing; you have to tab to Submit first.

## Submit path — async, non-blocking

The Submit branch (`keyboard.rs:254-276`):

```text
1. Split addrs/excls on whitespace → Vec<String>.
2. If addrs is empty → set state.error, redraw, don't close.
3. Otherwise:
     app.spawn(async move {
         pinger::collect_and_spawn(&app, &addrs, excl_opt, paused).await;
     });
     tui.add_tgt_dialog_close();
```

The dialog closes immediately. DNS resolution and target spawning happen
in the background on the Tokio runtime, with per-target progress messages
written to the log (visible via F12). This means a slow DNS lookup never
freezes the UI.

`collect_and_spawn` (`src/pinger.rs:195-225`) does the full pipeline:
parse the strings via `collect_targets`, fold new name↔IP mappings into
`AppState::resolved`, build `PingTarget`s with current `TargetDefaults`,
add them (deduplicating against existing targets), and spawn a
`ping_loop` for each survivor. The result is logged but not surfaced
back into the dialog — the dialog is gone by then.

## Cursor positioning

`AddTgtDialog::cursor_position(state)` (called from the renderer at
`main.rs:444`) returns the screen position of the caret in the
currently-focused text field. `frame.set_cursor_position(pos)` then
puts the cursor there, so the terminal's actual cursor follows the
focused field. For non-text focuses (Paused, Submit, Cancel) this
returns `None` and the cursor stays hidden.

## Help-over-dialog edge case

F1 can open the help popup on top of the input dialog. The Cancel
branch in `handle_input_dialog` (`keyboard.rs:246-251`) detects this
and closes the help popup instead of cancelling the dialog. That way
Esc dismisses the topmost overlay first.

## File map

- `src/ui/input.rs` — `ActiveField`, `DialogAction`,
  `AddTargetDialogState`, `AddTgtDialog`, rendering.
- `src/ui/keyboard.rs:42-44, 238-278` — routing and submit handling.
- `src/ui/tui.rs` — `add_tgt_dialog_open` / `close` helpers and the
  `input` rect inside `AppLayout`.
- `src/pinger.rs:195-225` — the async pipeline the dialog submits to.
- `src/main.rs:440-447` — render-time integration with cursor.
