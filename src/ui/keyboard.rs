// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{
    structs::AppState,
    ui::{PopupContents, TuiState},
};
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
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
            match (e.code, e.modifiers) {
                // Quit the application
                (KeyCode::Char('q'), _) => app.quit(),

                // terminal in raw mode -> ctrl-c has to be processed manually
                (KeyCode::Char('c'), KeyModifiers::CONTROL) => app.quit(),

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
                        app.toggle_target_pause(idx);
                    }
                }

                // Pause/resume all targets
                (KeyCode::Char('p'), _) => app.pause_all_targets(),
                (KeyCode::Char('P'), _) => app.resume_all_targets(),

                // Stop (cancel) the selected target's pinging for good
                (KeyCode::Char('S'), _) => {
                    if let Some(idx) = tui.layout.read().tablestate.selected() {
                        app.stop_target(idx);
                    }
                }

                // Reset the selected target's statistics
                (KeyCode::Char('R'), _) => {
                    if let Some(idx) = tui.layout.read().tablestate.selected() {
                        app.reset_target_stats(idx);
                    }
                }

                // Update information for the selected target
                (KeyCode::Enter, _) => {
                    if let Some(idx) = tui.layout.read().tablestate.selected() {
                        app.update_target_info(idx);
                    }
                }

                // Fully remove a target or targets from the list
                (KeyCode::Delete, m) => {
                    let mut lo = tui.layout.write();
                    match m {
                        // Stop and remove all unreachable targets from the list
                        KeyModifiers::CONTROL => {
                            if app.remove_all_unreachables() > 0 {
                                // clear row selection after removal
                                lo.tablestate.select(None);
                                // reset table width since it could be compressed
                                lo.reset_table_widths();
                            }
                        }
                        // Stop and remove the selected target
                        KeyModifiers::NONE => {
                            if let Some(idx) = lo.tablestate.selected() {
                                app.remove_target(idx);

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
                (KeyCode::F(10), _) => {
                    app.toggle_perf();
                }

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

            Ok(true)
        } else {
            Ok(false)
        }
    } else {
        Ok(false)
    }
}
