# Add-target input dialog

The `a` key opens a modal dialog for adding targets at runtime. It collects
addresses, exclusions, and a "start paused?" flag, then hands the strings
to `pinger::collect_and_spawn` to be parsed, resolved, and pinged — all
without blocking the keyboard or render loops.

## Pieces

| Type | Where | Role |
|---|---|---|
| `AddTargetDialogState` | `src/ui/input.rs:39` | The mutable state (two `Input`s, a bool, a focused-field enum, a `DialogFeedback`: `None` / `Error` / `Working` / `Summary`) |
| `AddTgtDialog` | `src/ui/input.rs:150+` | The `StatefulWidget` Ratatui renders against `AddTargetDialogState` |
| `ActiveField` | `src/ui/input.rs:19` | Which field has focus: `Addresses`, `Exclusions`, `Paused`, `Submit`, `Cancel` |
| `DialogAction` | `src/ui/input.rs:29` | `None` / `Redraw` / `Cancel` / `Submit { addrs, excls, paused }` — what the keyboard branch reacts to |

`tui-input` provides the text-editing primitive for the two text fields
(arrows, backspace, char insert, etc.); we route crossterm `KeyEvent`s to
it inside `on_key` (`input.rs:91-97`).

## Routing & focus

When `tui.layout.read().input_visible` is true, `key_event_poll` routes
every key to `handle_input_dialog` except the global keys (Ctrl-C quits,
F1 toggles help, F12 toggles the log popup) and PageUp/PageDown while a
text popup is open over the dialog (they page it). That helper calls
`AddTargetDialogState::on_key(key) -> DialogAction` and acts on the
result:

- `None` — no state change, don't even notify the render loop.
- `Redraw` — state changed (focus moved, character typed, paused
  toggled). Notify the loop.
- `Cancel` — Esc pressed (or Enter on Cancel) → `close_top_overlay`,
  which closes help or the log popup first if one is on top of the
  dialog, and the dialog itself otherwise.
- `Submit { addrs, excls, paused }` — Enter on the Submit button →
  parse the input, start the async work, keep the dialog open showing
  `Working` until the summary arrives.

The focus order is a simple cycle: Addresses → Exclusions → Paused →
Submit → Cancel → Addresses (`input.rs:107-125`). Tab moves forward,
Shift-Tab backward. Space toggles the Paused checkbox only when focus is
on it (`input.rs:77-82`). Enter while a text field has focus is
deliberately a no-op — you can't accidentally submit by hitting Enter
mid-typing; you have to tab to Submit first.

## Submit path — async, non-blocking

The Submit branch of `handle_input_dialog` (`keyboard.rs`):

```text
1. Split addrs/excls on whitespace/commas → Vec<String>.
2. If addrs is empty → feedback = Error(..), redraw, nothing submitted.
3. Otherwise:
     feedback = Working; clear both fields; focus Addresses
     app.spawn(async move {
         let outcome = pinger::collect_and_spawn(&app, &addrs, excl_opt, paused).await;
         input_state.feedback = Summary(format_add_summary(&outcome, paused));
         key_event.notify_one();   // redraw
     });
```

The dialog stays open across submissions. DNS resolution and target
spawning happen in the background on the Tokio runtime, with per-target
progress messages written to the log (visible via F12, also from inside
the dialog), so a slow DNS lookup never freezes the UI. While the
feedback is `Working`, Enter on Submit is ignored (`on_key`), so only one
batch is in flight at a time; the user can already type the next batch.

`collect_and_spawn` (`src/pinger.rs`) does the full pipeline: parse the
strings via `collect_targets`, fold new name↔IP mappings into
`AppState::resolved`, build `PingTarget`s with current `TargetDefaults`,
add them (deduplicating against existing targets), and spawn a
`ping_loop` for each survivor. The returned `AddOutcome` is rendered into
the dialog's feedback area by `format_add_summary`.

### Closing and reopening mid-submission

Esc only hides the dialog; the spawned task still writes its summary
into `TuiState::input_state`. `add_tgt_dialog_open` therefore calls
`AddTargetDialogState::reopen`, which resets the state to default
*unless* a submission is still `Working`. Resetting it would unblock a
second submit while the first is resolving, and the first batch's summary
would then land on the dialog as if it were the second's result.

## Cursor positioning

`AddTgtDialog::cursor_position(state)` (called from the renderer at
`main.rs:444`) returns the screen position of the caret in the
currently-focused text field. `frame.set_cursor_position(pos)` then
puts the cursor there, so the terminal's actual cursor follows the
focused field. For non-text focuses (Paused, Submit, Cancel) this
returns `None` and the cursor stays hidden.

## Overlays over the dialog

F1 (help) and F12 (log) can be opened on top of the input dialog: the
render order is dialog, then text popup, then help, so both draw above
it (and the dialog's caret is hidden while a text popup covers it). The
Cancel branch in `handle_input_dialog` goes through `close_top_overlay`,
which closes help, then the text popup, then the dialog — Esc always
dismisses the topmost overlay first. Opening the dialog with `a` closes
a text popup left open from before, since it would otherwise cover the
dialog.

## File map

- `src/ui/input.rs` — `ActiveField`, `DialogAction`,
  `AddTargetDialogState`, `AddTgtDialog`, rendering.
- `src/ui/keyboard.rs:42-44, 238-278` — routing and submit handling.
- `src/ui/tui.rs` — `add_tgt_dialog_open` / `close` helpers and the
  `input` rect inside `AppLayout`.
- `src/pinger.rs:195-225` — the async pipeline the dialog submits to.
- `src/main.rs:440-447` — render-time integration with cursor.
