// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{
    pinger,
    structs::{AppState, Command, CmdResult},
    ui::*,
};
use tui_input::Input;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
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
        if let Event::Key(e) = event::read()? {
            if tui.layout.read().input_visible {
                // route key event to input dialog handler
                return handle_input_dialog(app, tui, e);
            }

            match (e.code, e.modifiers) {
                // Quit the application
                (KeyCode::Char('q'), _) => { app.execute(Command::Quit); },

                // terminal in raw mode -> ctrl-c has to be processed manually
                (KeyCode::Char('c'), KeyModifiers::CONTROL) => { app.execute(Command::Quit); },

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
                        let step: u16 = lo.popup_usable_rows() as u16 * 2 - 1;
                        lo.liststate.scroll_up_by(step);
                    } else {
                        let step: u16 = match m {
                            KeyModifiers::SHIFT => SHIFT_PAGE_ROWS,
                            _ => lo.tbl_usable_rows() as u16 - 1,
                        };
                        lo.tablestate.scroll_up_by(step);
                    }
                }
                (KeyCode::PageDown, m) => {
                    let mut lo = tui.layout.write();
                    if lo.popup_visible && m != KeyModifiers::SHIFT {
                        let step: u16 = lo.popup_usable_rows() as u16 * 2 - 1;
                        lo.liststate.scroll_down_by(step);
                    } else {
                        let step: u16 = match m {
                            KeyModifiers::SHIFT => SHIFT_PAGE_ROWS,
                            _ => lo.tbl_usable_rows() as u16 - 1,
                        };
                        lo.tablestate.scroll_down_by(step);
                    }
                }

                // Clear table selections
                (KeyCode::Backspace, _) => {
                    let mut lo = tui.layout.write();
                    lo.tablestate.select(None);
                    lo.tablestate.select_column(None);
                }

                // Pause/resume the selected target if it's not stopped
                (KeyCode::Char(' '), _) => {
                    if let Some(idx) = tui.layout.read().tablestate.selected() {
                        app.execute(Command::TogglePause(idx));
                    }
                }

                // Pause/resume all targets
                (KeyCode::Char('p'), _) => { app.execute(Command::PauseAll); },
                (KeyCode::Char('P'), _) => { app.execute(Command::ResumeAll); },

                // Stop (cancel) the selected target's pinging for good
                (KeyCode::Char('S'), _) => {
                    if let Some(idx) = tui.layout.read().tablestate.selected() {
                        app.execute(Command::StopTarget(idx));
                    }
                }

                // Reset the selected target's statistics
                (KeyCode::Char('R'), _) => {
                    if let Some(idx) = tui.layout.read().tablestate.selected() {
                        app.execute(Command::ResetTgtStats(idx));
                    }
                }

                // Update information for the selected target
                (KeyCode::Enter, _) => {
                    if let Some(idx) = tui.layout.read().tablestate.selected() {
                        app.execute(Command::UpdateTgtInfo(idx));
                    }
                }

                // Fully remove a target or targets from the list
                (KeyCode::Delete, m) => {
                    let mut lo = tui.layout.write();
                    match m {
                        // Stop and remove all unreachable targets from the list
                        KeyModifiers::CONTROL => {
                            match app.execute(Command::RemoveAllUnreach) {
                                CmdResult::Count(_) => {
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
                            if let Some(idx) = lo.tablestate.selected() {
                                app.execute(Command::RemoveTarget(idx));

                                // fix row selection after removal
                                let len: usize = app.len();
                                if len == 0 {
                                    lo.tablestate.select(None);
                                } else if idx == len - 1 {
                                    lo.tablestate.select(Some(idx.saturating_sub(1)));
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
                (KeyCode::Esc, _) => {
                    let mut lo = tui.layout.write();
                    if lo.help_visible {
                        lo.help_visible = false;
                    } else if lo.input_visible {
                        lo.input_visible = false;
                    } else if lo.popup_visible {
                        *tui.popup_contents.write() = PopupContents::None;
                        lo.popup_visible = false;
                        lo.liststate.select(None);
                    }
                }

                // Show/hide the help popup
                (KeyCode::F(1), _) => {
                    let mut lo = tui.layout.write();
                    match lo.help_visible {
                        false => {
                            lo.help_visible = true;
                        }
                        true => {
                            lo.help_visible = false;
                        }
                    }
                }

                // Toggle "performance" mode
                (KeyCode::F(10), _) => { app.execute(Command::TogglePerf); }

                // Show/hide the log message buffer
                (KeyCode::F(12), _) => {
                    let mut lo = tui.layout.write();
                    match lo.popup_visible {
                        false => {
                            *tui.popup_contents.write() = PopupContents::Buffer(app.logger.clone());
                            lo.popup_visible = true;
                        }
                        true => {
                            *tui.popup_contents.write() = PopupContents::None;
                            lo.popup_visible = false;
                        }
                    }
                }

                // Don't signal an unhandled key event
                _ => return Ok(false),
            }

            // Clear the event queue after handling a key event to avoid a backlog.
            while event::poll(Duration::from_millis(0))? {
                let _ = event::read()?;
            }

            // since we're here, a key event was handled
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

/// Input dialog key event handler
fn handle_input_dialog(app: &Arc<AppState>, tui: &Arc<TuiState>, e: KeyEvent) -> Result<bool> {
    let action = tui.input_state.write().on_key(e);
    match action {
        DialogAction::None => Ok(false),
        DialogAction::Redraw => Ok(true),

        DialogAction::Cancel => {
            // help dialog could be open on top of input dialog
            if tui.layout.read().help_visible {
                tui.layout.write().help_visible = false;
            } else {
                tui.add_tgt_dialog_close()
            };
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
