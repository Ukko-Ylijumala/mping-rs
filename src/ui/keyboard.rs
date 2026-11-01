// Copyright (c) 2025-2026 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{
    pinger,
    structs::{AppState, Command, CmdResult},
    ui::{tui::AppLayout, *},
};
use tui_input::Input;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use std::{io::Result, sync::Arc, time::Duration};

const SHIFT_PAGE_ROWS: u16 = 10; // rows to shift on page up/down
const POLL_WAIT_MS: u64 = 50; // key event poll wait time in milliseconds

/// This key event handler loop is intended to be run in a dedicated thread.
/// It listens for keyboard events and updates the application state accordingly.
pub(crate) fn key_event_handler(state: Arc<AppState>, tui: Arc<TuiState>) {
    state.logger.debug(crate::strings::KEV_START);
    while !state.is_quitting() {
        if key_event_poll(POLL_WAIT_MS, &state, &tui).is_ok_and(|e| e) {
            // notify the main loop about the key event for immediate refresh
            state.key_event.notify_one();
        }
    }
}

/**
Crossterm key event polling helper
### Arguments
- `wait_ms`: milliseconds to wait for an event (before returning `Ok(false)`)
- `app`: Application state reference

### Returns
- `Ok(bool)` indicating whether a handled key event occurred
*/
fn key_event_poll(wait_ms: u64, app: &Arc<AppState>, tui: &Arc<TuiState>) -> Result<bool> {
    if event::poll(Duration::from_millis(wait_ms))? {
        let ev: Event = event::read()?;
        if let Event::Key(e) = ev {
            let (input_visible, popup_visible) = {
                let lo = tui.layout.read();
                (lo.input_visible, lo.popup_visible)
            };

            match (e.code, e.modifiers) {
                /*
                Global keys: these work over the add-target dialog too. NOTE:
                terminal in raw mode -> ctrl-c has to be processed manually.
                */
                (KeyCode::Char('c'), KeyModifiers::CONTROL) => { app.execute(Command::Quit); },
                (KeyCode::F(1), _) => toggle_help(tui),
                (KeyCode::F(12), _) => toggle_log_popup(app, tui),

                // The rest goes to the dialog, except paging a popup opened over it
                (KeyCode::PageUp | KeyCode::PageDown, _) if input_visible && popup_visible => {
                    scroll_popup(tui, &mut tui.layout.write(), e.code == KeyCode::PageDown);
                }
                _ if input_visible => return handle_input_dialog(app, tui, e),

                // Quit the application - or, if a popup is open, just close it
                // (a reflexive 'q' to dismiss help shouldn't end the session)
                (KeyCode::Char('q'), KeyModifiers::NONE) => {
                    if !close_top_overlay(tui) {
                        app.execute(Command::Quit);
                    }
                },

                /*
                Sort by the selected column: Shift+Up ascending, Shift+Down
                descending; the same direction again resets to original order.
                NOTE: must come before the plain Up/Down arms, which match
                any modifier.
                */
                (KeyCode::Up, KeyModifiers::SHIFT) => handle_sort(app, tui, false),
                (KeyCode::Down, KeyModifiers::SHIFT) => handle_sort(app, tui, true),

                // Table navigation: up/down
                (KeyCode::Up, _) => tui.layout.write().tablestate.select_previous(),
                (KeyCode::Down, _) => tui.layout.write().tablestate.select_next(),

                // Table navigation: left/right (columns)
                (KeyCode::Left, _) => tui.layout.write().tablestate.select_previous_column(),
                (KeyCode::Right, _) => tui.layout.write().tablestate.select_next_column(),

                // Table navigation: home/end
                (KeyCode::Home, _) => tui.layout.write().tablestate.select_first(),
                (KeyCode::End, _) => tui.layout.write().tablestate.select_last(),

                // Table navigation: page up/down
                // When popup is visible, scroll the popup content instead.
                (KeyCode::PageUp, m) => {
                    let mut lo = tui.layout.write();
                    if lo.popup_visible && m != KeyModifiers::SHIFT {
                        scroll_popup(tui, &mut lo, false);
                    } else {
                        let step: u16 = match m {
                            KeyModifiers::SHIFT => SHIFT_PAGE_ROWS,
                            _ => (lo.tbl_usable_rows() as u16).saturating_sub(1),
                        };
                        lo.tablestate.scroll_up_by(step);
                    }
                }
                (KeyCode::PageDown, m) => {
                    let mut lo = tui.layout.write();
                    if lo.popup_visible && m != KeyModifiers::SHIFT {
                        scroll_popup(tui, &mut lo, true);
                    } else {
                        let step: u16 = match m {
                            KeyModifiers::SHIFT => SHIFT_PAGE_ROWS,
                            _ => (lo.tbl_usable_rows() as u16).saturating_sub(1),
                        };
                        lo.tablestate.scroll_down_by(step);
                    }
                }

                // Clear table selections and sorting
                (KeyCode::Backspace, _) => {
                    // restore original target order first, if sorted
                    // (lock-order: execute before taking the layout lock)
                    if tui.layout.read().sort_state.is_some() {
                        app.execute(Command::SortReset);
                    }
                    let mut lo = tui.layout.write();
                    lo.sort_state = None;
                    lo.tablestate.select(None);
                    lo.tablestate.select_column(None);
                }

                /*
                Lock-order invariant: never call `app.execute` (which may lock
                `targets`) while holding the `layout` lock — the render path
                takes the same locks and a cross-thread ABBA deadlock follows.
                Hence the selected index is read out (dropping the guard at the
                end of the statement) before dispatching the command.
                */

                // Pause/resume the selected target if it's not stopped
                (KeyCode::Char(' '), _) => {
                    let selected = tui.layout.read().tablestate.selected();
                    if let Some(idx) = selected {
                        app.execute(Command::TogglePause(idx));
                    }
                }

                // Pause/resume all targets
                (KeyCode::Char('p'), _) => { app.execute(Command::PauseAll); },
                (KeyCode::Char('P'), _) => { app.execute(Command::ResumeAll); },

                // Stop (cancel) the selected target's pinging for good
                (KeyCode::Char('S'), _) => {
                    let selected = tui.layout.read().tablestate.selected();
                    if let Some(idx) = selected {
                        app.execute(Command::StopTarget(idx));
                    }
                }

                // Reset the selected target's statistics
                (KeyCode::Char('R'), _) => {
                    let selected = tui.layout.read().tablestate.selected();
                    if let Some(idx) = selected {
                        app.execute(Command::ResetTgtStats(idx));
                    }
                }

                // Update information for the selected target
                (KeyCode::Enter, _) => {
                    let selected = tui.layout.read().tablestate.selected();
                    if let Some(idx) = selected {
                        app.execute(Command::UpdateTgtInfo(idx));
                    }
                }

                // Show the selected target's event timeline (outages etc.) in a popup.
                // Pure UI: reads target data and fills the popup, no Command needed.
                (KeyCode::Char('E'), _) => {
                    let selected = tui.layout.read().tablestate.selected();
                    let info = selected.and_then(|idx| {
                        app.targets
                            .read()
                            .get(idx)
                            .map(|t| (t.to_string(), t.recent_events()))
                    });
                    if let Some((name, events)) = info {
                        tui.popup_show(&mut tui.layout.write(), events_popup(name, &events));
                    }
                }

                // Traceroute to the selected target: start it (no-op if running) and
                // show it in a popup that re-renders from the live trace state.
                (KeyCode::Char('T'), _) => {
                    let selected = tui.layout.read().tablestate.selected();
                    if let Some(idx) = selected {
                        app.execute(Command::Traceroute(idx));
                        let tgt = app.targets.read().get(idx).cloned();
                        if let Some(tgt) = tgt {
                            tui.popup_show(&mut tui.layout.write(), PopupContents::Trace(tgt));
                        }
                    }
                }

                // Fully remove a target or targets from the list
                (KeyCode::Delete, m) => {
                    match m {
                        // Stop and remove all unreachable targets from the list
                        KeyModifiers::CONTROL => {
                            match app.execute(Command::RemoveAllUnreach) {
                                CmdResult::Count(_) => {
                                    let mut lo = tui.layout.write();
                                    // clear row selection after removal
                                    lo.tablestate.select(None);
                                    // reset table width since it could be compressed
                                    lo.reset_table_widths();
                                }
                                _ => { /* nothing was removed */ }
                            }
                        }
                        // Stop and remove the selected target
                        KeyModifiers::NONE => {
                            let selected = tui.layout.read().tablestate.selected();
                            if let Some(idx) = selected {
                                app.execute(Command::RemoveTarget(idx));
                                let len: usize = app.len();

                                // fix row selection after removal
                                let mut lo = tui.layout.write();
                                if len == 0 {
                                    lo.tablestate.select(None);
                                } else if idx >= len {
                                    // removed the last row -> select the new last one
                                    lo.tablestate.select(Some(len - 1));
                                }

                                lo.reset_table_widths();
                            }
                        }
                        _ => {}
                    }
                }

                // Open the add target input dialog
                (KeyCode::Char('a'), _) => { tui.add_tgt_dialog_open(); }

                // Close active popup/help/input overlays
                (KeyCode::Esc, _) => { close_top_overlay(tui); }

                // Toggle "performance" mode
                (KeyCode::F(10), _) => { app.execute(Command::TogglePerf); }

                // Don't signal an unhandled key event
                _ => return Ok(false),
            }

            // Clear the event queue after handling a key event to avoid a backlog.
            while event::poll(Duration::from_millis(0))? {
                let _ = event::read()?;
            }

            // since we're here, a key event was handled
            Ok(true)
        } else if let Event::Resize(w, h) = ev {
            /*
            Ratatui only autoresizes inside `draw`, and the row data is gathered
            against the layout before that: update it now and redraw right away,
            so the frame doesn't wait for the next refresh or format a stale
            viewport. No queue drain here - that would eat keystrokes, and the
            `Notify` coalesces a burst of resizes anyway.
            */
            tui.layout.write().update(Rect::new(0, 0, w, h));
            Ok(true)
        } else {
            // wasn't a key event
            Ok(false)
        }
    } else {
        // nothing was polled during the wait time
        Ok(false)
    }
}

/**
Close the topmost visible overlay (help, then the text popup, then the input
dialog - the render z-order). Returns `false` if nothing was open.

NOTE: lock-order - takes `layout` then `popup_contents`, same as render.
*/
fn close_top_overlay(tui: &TuiState) -> bool {
    let mut lo = tui.layout.write();
    if lo.help_visible {
        lo.help_visible = false;
    } else if lo.popup_visible {
        tui.popup_hide(&mut lo);
    } else if lo.input_visible {
        lo.input_visible = false;
    } else {
        return false;
    }
    true
}

/// Show/hide the help popup.
fn toggle_help(tui: &TuiState) {
    let mut lo = tui.layout.write();
    lo.help_visible = !lo.help_visible;
}

/// Show the log message buffer, or hide whichever text popup is open.
fn toggle_log_popup(app: &AppState, tui: &TuiState) {
    let mut lo = tui.layout.write();
    match lo.popup_visible {
        false => tui.popup_show(&mut lo, PopupContents::Buffer(app.logger.clone())),
        true => tui.popup_hide(&mut lo),
    }
}

/**
Scroll the text popup by one page (minus a line of overlap) via the list
offset. The selection stays `None`, so ratatui honours the offset as-is;
scrolling by selection instead would land it at the bottom edge and skip
most of a page on every press after the first.

NOTE: lock-order - caller holds `layout`, `popup_contents` is taken after
it, same as render.
*/
fn scroll_popup(tui: &TuiState, lo: &mut AppLayout, down: bool) {
    let len: usize = tui.popup_contents.read().len();
    let rows: usize = lo.popup_usable_rows();
    let offset: &mut usize = lo.liststate.offset_mut();
    *offset = popup_page_offset(*offset, rows, len, down);
}

/// New popup list offset after paging up/down, clamped so the last page stays full.
fn popup_page_offset(offset: usize, rows: usize, len: usize, down: bool) -> usize {
    let step: usize = rows.saturating_sub(1).max(1);
    let max_offset: usize = len.saturating_sub(rows);
    match down {
        true => offset.saturating_add(step),
        false => offset.saturating_sub(step),
    }
    .min(max_offset)
}

/**
Sort handler for Shift+Up/Down. Sorts by the currently selected column
(no-op when no column is selected); pressing the same direction again
restores the original (insertion) order. The row selection follows the
previously selected target to its new position.

NOTE: lock-order - `layout` is never held while calling [AppState::execute].
*/
fn handle_sort(app: &Arc<AppState>, tui: &Arc<TuiState>, descending: bool) {
    let (col, current) = {
        let lo = tui.layout.read();
        (lo.tablestate.selected_column(), lo.sort_state)
    };
    let Some(col) = col else { return };

    // same direction on the same column again -> back to original order
    if current == Some((col, descending)) {
        if app.execute(Command::SortReset) == CmdResult::Done {
            tui.layout.write().sort_state = None;
        }
        return;
    }

    // remember the selected target so the selection can follow it post-sort
    let selected = tui.layout.read().tablestate.selected();
    let selected_addr = selected.and_then(|i| app.targets.read().get(i).map(|t| t.addr));

    if app.execute(Command::Sort(col, descending)) == CmdResult::Done {
        let new_idx =
            selected_addr.and_then(|a| app.targets.read().iter().position(|t| t.addr == a));
        let mut lo = tui.layout.write();
        lo.sort_state = Some((col, descending));
        if new_idx.is_some() {
            lo.tablestate.select(new_idx);
        }
    }
}

/// Input dialog key event handler
fn handle_input_dialog(app: &Arc<AppState>, tui: &Arc<TuiState>, e: KeyEvent) -> Result<bool> {
    let action = tui.input_state.write().on_key(e);
    match action {
        DialogAction::None => Ok(false),
        DialogAction::Redraw => Ok(true),

        DialogAction::Cancel => {
            // help or the log popup could be open on top of the dialog: close
            // the topmost overlay first, the dialog itself last
            close_top_overlay(tui);
            Ok(true)
        }

        DialogAction::Submit { addrs, excls, paused } => {
            let addr_strs = split_dialog_input(&addrs);
            if addr_strs.is_empty() {
                tui.input_state.write().feedback = DialogFeedback::Error(
                    "Enter at least one address (IP, range, CIDR or DNS name).".into(),
                );
                return Ok(true);
            }
            let excl_strs = split_dialog_input(&excls);
            /*
            The dialog stays open across submissions. Mark it as Working,
            clear the input fields so the user can start typing the next
            batch immediately, and refocus the addresses field. The Working
            state blocks re-submit until the spawned task writes back its
            summary. Paused checkbox state is intentionally preserved.
            */
            {
                let mut state = tui.input_state.write();
                state.feedback = DialogFeedback::Working;
                state.addrs = Input::default();
                state.excls = Input::default();
                state.active = ActiveField::Addresses;
            }

            let app_clone = app.clone();
            let tui_clone = tui.clone();
            /*
            Parsing + DNS resolution happens off-thread on the Tokio runtime.
            Per-target progress goes to the log; the summary is written back
            into the dialog's feedback area when the work completes.
            */
            app.spawn(async move {
                let outcome = pinger::collect_and_spawn(
                    &app_clone,
                    &addr_strs,
                    if excl_strs.is_empty() { None } else { Some(&excl_strs) },
                    paused,
                )
                .await;
                let summary = format_add_summary(&outcome, paused);
                tui_clone.input_state.write().feedback = DialogFeedback::Summary(summary);
                app_clone.key_event.notify_one();
            });
            Ok(true)
        }
    }
}

/// Split a free-form dialog input field into individual target/exclusion strings.
/// Accepts whitespace and/or commas as separators (matches the CLI conventions).
fn split_dialog_input(s: &str) -> Vec<String> {
    s.split([' ', ',', '\t', '\n', '\r'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/* ===== tests ===== */

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn popup_paging_is_contiguous_and_symmetric() {
        // 100 lines, 20 visible rows -> pages of 19 with one line of overlap
        let (rows, len) = (20, 100);
        let mut off: usize = 0;
        let mut downs: Vec<usize> = Vec::new();
        for _ in 0..6 {
            off = popup_page_offset(off, rows, len, true);
            downs.push(off);
        }
        // no skipped lines, clamped to the last full page
        assert_eq!(downs, [19, 38, 57, 76, 80, 80]);
        assert_eq!(popup_page_offset(80, rows, len, false), 61);
        assert_eq!(popup_page_offset(61, rows, len, false), 42);
        assert_eq!(popup_page_offset(5, rows, len, false), 0, "saturates");
    }

    #[test]
    fn popup_paging_degenerate_sizes() {
        assert_eq!(popup_page_offset(0, 20, 10, true), 0, "content fits");
        assert_eq!(popup_page_offset(50, 20, 10, false), 0, "stale offset");
        assert_eq!(popup_page_offset(0, 1, 5, true), 1, "one-row popup");
        assert_eq!(popup_page_offset(0, 0, 5, true), 1, "zero-row popup");
    }
}
