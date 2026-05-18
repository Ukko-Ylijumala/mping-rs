# TUI rendering pipeline & layout

The TUI is built on Ratatui + Crossterm. Rendering is driven by the main
async loop in `src/main.rs` and laid out by `AppLayout` in `src/ui/tui.rs`.
Two things make this not-completely-naive: a viewport-only data gathering
pass and a "columns can grow but never shrink" sizing policy.

## The main loop

```text
loop {
    tokio::select! {
        biased;
        true = app.is_quitting_async()       => break,
        true = tui.ui_refresh_elapsed_async() => render,
        _   = app.key_event.notified()       => immediate render,
        _   = tick.tick()                    => no-op
    }
}
```

Found at `src/main.rs:488-507`. The `biased` ordering is important: we
always check quit first, then the next scheduled refresh, then the
"keyboard nudged us, redraw now" path. The 5 ms sleep after the
keyboard-driven branch (`main.rs:503`) prevents busy-looping on a stream
of key events.

The `key_event.notified()` branch does **not** call
`ui_schedule_next_refresh()` — it draws immediately without resetting the
refresh ticker. That way a burst of keystrokes doesn't push the natural
refresh schedule out into the future.

## `gather_target_data` — viewport-only formatting

The TUI table can contain thousands of rows, but only a few dozen are
ever visible. `gather_target_data(state, tui, all)` (`main.rs:116-152`)
exploits this: when `all == false` (the live render path), it formats
only the rows currently inside the viewport and pads the rest with empty
`TableRow`s so Ratatui still has a row for every target (scroll position
and selection indices line up).

When `all == true` (the final stdout dump after teardown — `main.rs:519`),
it formats every row so the printed stats are complete.

Formatting itself happens in `format_row` (`main.rs:57`), which takes
`StatsSnapshot::new_from(&tgt, timeout)` first (one short read lock) and
then does all the string work outside the lock — see the lock-discipline
note in [shared-state](shared-state.md).

## `AppLayout` and `maybe_update`

`AppLayout` (`src/ui/tui.rs:111-331`) owns every `Rect` the renderer
needs. The layout looks like:

```text
┌─────────────────────────────────────────────┐
│              > title (1 line) <             │
├──────────────────────────┬──────────────────┤
│                          │ info_upper       │
│                          │   ├ text (7)     │
│                          │   ├ graph (20)   │
│  table                   │   └ histo (11)   │
│                          ├──────────────────┤
│                          │ info_lower (5)   │
├──────────────────────────┴──────────────────┤
│ status_l         |           status_r (≥43) │
└─────────────────────────────────────────────┘
```

Plus three modal areas (`help`, `popup`, `input`) that overlay everything
else.

`maybe_update(frame_rect, rows)` (`tui.rs:200`) is called once per render.
It is a no-op when neither the frame area nor the current column widths
have changed. When something does change:

- A frame-size change forces a full `update()` and recomputes every area.
- A column-width change recomputes the middle and info areas only.

### Non-shrinking columns

`update_col_widths` (`tui.rs:308`) takes the max of (header width, every
visible cell's width, previous constraint). Columns can grow but never
contract, so a momentarily long value (e.g. an error message) doesn't
cause the whole layout to twitch back to small the next frame. The user
can clear it by removing the offending target, and `Delete` /
`Ctrl-Delete` paths call `reset_table_widths()` explicitly
(`keyboard.rs:143, 161`) so the table can recompress after bulk
removals.

## Statics with `LazyLock`

Ratatui widgets are cheap but not free — `Block`, `Paragraph`, and
`Dataset` allocate. The renderer caches the ones it reuses every frame as
`static LazyLock<Block>` / `static Paragraph` constants
(`main.rs:155-197`). Each frame clones the lazy template and customises
title / data, which avoids rebuilding immutable parts from scratch.

The render path also caches `tui.title` and `tui.headers` (the column
header row) on `TuiState`, so they're built once at startup.

## The selected-target detail pane

When a row is selected, the right-hand `info_upper` area shows the target
summary, an RTT line graph, and an RTT histogram (`main.rs:217-302`). The
graph and histogram pull from `get_recent_rtts(GRAPH_SAMPLES)` —
`GRAPH_SAMPLES = 180`, so at the default 1 s interval the graph spans the
last 3 minutes. The y-axis is rounded to one decimal place; values below
0.5 ms snap the lower bound to 0 for a cleaner baseline.

The `targets` read lock is dropped (`main.rs:236, 304`) before the chart
widgets are built, so the heavy widget construction doesn't keep the lock
held longer than necessary.

## Popups, dialogs, and z-ordering

The render order at the bottom of `render_frame` is intentional: title,
procinfo, info_upper, info_lower, status_line, then the popups. Later
draws overwrite earlier ones, so the help popup, the log popup, and the
add-target input dialog all land on top of the rest of the frame.
`render_popups` (`main.rs:396`) handles each modal in priority order and
draws `Clear` first so the layer beneath doesn't bleed through.

The scrollbar is drawn directly over the right border of the table area
(no `b_tbl.inner()`), gaining one extra column of usable width. The
comment at `main.rs:360-368` explains why we use a throwaway
`ScrollbarState` per frame instead of caching one — Ratatui's
`Scrollbar` setters consume `self`, so caching wouldn't save anything.

## `TerminalGuard` and signal interplay

Terminal entry / exit is handled by `TerminalGuard` (RAII) and the signal
thread — see [signal-and-terminal](signal-and-terminal.md).

## File map

- `src/main.rs:48-152` — `make_targets`, `format_row`, `gather_target_data`.
- `src/main.rs:154-197` — `LazyLock` static widgets.
- `src/main.rs:199-457` — `render_frame` and `render_popups`.
- `src/main.rs:461-526` — the main loop.
- `src/ui/tui.rs:111-330` — `AppLayout` and `maybe_update`.
- `src/ui/tui.rs` (rest) — `TuiState`, `MutableLine`, headers, terminal
  guard. See related design docs for guard / dialog details.
