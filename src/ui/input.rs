// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use crossterm::event::{Event, KeyCode, KeyEvent};
use tui_input::{Input, backend::crossterm::EventHandler};

/// Currently active field in the add target dialog.
#[derive(Debug, Default, Clone)]
pub(crate) enum ActiveField {
    #[default]
    Addresses,
    Exclusions,
    Paused,
    Submit,
    Cancel,
}

#[derive(Debug, Clone)]
pub(crate) enum DialogAction {
    None,
    Cancel,
    Redraw,
    #[rustfmt::skip]
    Submit { addrs: String, excls: String, paused: bool },
}

/// State for the add target dialog input overlay.
#[derive(Debug, Default, Clone)]
pub(crate) struct AddTargetDialogState {
    pub addrs: Input,          // Text input for addresses
    pub excls: Input,          // Text input for exclusions
    pub paused: bool,          // Checkbox for paused status
    pub active: ActiveField,   // Currently active field
    pub error: Option<String>, // Optional error display
}

impl AddTargetDialogState {
    pub fn on_key(&mut self, key: KeyEvent) -> DialogAction {
        match key.code {
            KeyCode::Esc => return DialogAction::Cancel,
            KeyCode::Tab => {
                self.focus_next();
                return DialogAction::Redraw;
            }
            KeyCode::BackTab => {
                self.focus_prev();
                return DialogAction::Redraw;
            }

            KeyCode::Enter => {
                return match self.active {
                    ActiveField::Submit => DialogAction::Submit {
                        addrs: self.addrs.value().to_string(),
                        excls: self.excls.value().to_string(),
                        paused: self.paused,
                    },
                    ActiveField::Cancel => DialogAction::Cancel,
                    ActiveField::Paused => {
                        self.paused = !self.paused;
                        DialogAction::Redraw
                    }
                    // enter in a text field could be treated as submit or ignored
                    ActiveField::Addresses | ActiveField::Exclusions => DialogAction::None,
                };
            }

            KeyCode::Char(' ') => {
                if matches!(self.active, ActiveField::Paused) {
                    self.paused = !self.paused;
                    return DialogAction::Redraw;
                }
            }

            _ => {}
        }

        // Route editing keys to the active input
        match self.active {
            ActiveField::Addresses => {
                // map crossterm KeyEvent -> tui-input editing ops (arrows, backspace, char insert...)
                if self.addrs.handle_event(&Event::Key(key)).is_some() {
                    return DialogAction::Redraw;
                }
            }
            ActiveField::Exclusions => {
                if self.excls.handle_event(&Event::Key(key)).is_some() {
                    return DialogAction::Redraw;
                }
            }
            _ => {}
        }

        // no state change
        DialogAction::None
    }

    fn focus_next(&mut self) {
        self.active = match self.active {
            ActiveField::Addresses => ActiveField::Exclusions,
            ActiveField::Exclusions => ActiveField::Paused,
            ActiveField::Paused => ActiveField::Submit,
            ActiveField::Submit => ActiveField::Cancel,
            ActiveField::Cancel => ActiveField::Addresses,
        };
    }

    fn focus_prev(&mut self) {
        self.active = match self.active {
            ActiveField::Addresses => ActiveField::Cancel,
            ActiveField::Exclusions => ActiveField::Addresses,
            ActiveField::Paused => ActiveField::Exclusions,
            ActiveField::Submit => ActiveField::Paused,
            ActiveField::Cancel => ActiveField::Submit,
        };
    }
}
