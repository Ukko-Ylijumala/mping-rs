// Copyright (c) 2025-2026 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::strings::*;
use crossterm::event::{Event, KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Color, Stylize},
    text::{Line, Span},
    widgets::*,
};
use std::{rc::Rc, sync::LazyLock};
use tui_input::{Input, backend::crossterm::EventHandler};

/// Currently active field in the add target dialog.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
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

/* -------------------------------------------------------------------------- */

// Static constraints
static CON_FILL: Constraint = Constraint::Fill(1);
static CON_LEN_1: Constraint = Constraint::Length(1);
static CON_LEN_3: Constraint = Constraint::Length(3);
static CON_LEN_10: Constraint = Constraint::Length(10);

// Static styled components
static NO_TRIM: Wrap = Wrap { trim: false };
static BORDERS: Block = Block::bordered();
static SUBMIT: LazyLock<Span> = LazyLock::new(|| Span::styled(INPUT_SUBMIT, Color::Green));
static CANCEL: LazyLock<Span> = LazyLock::new(|| Span::styled(INPUT_CANCEL, Color::Red));
static BLK_SUBMIT: LazyLock<Block> =
    LazyLock::new(|| Block::bordered().border_type(BorderType::Rounded).green());
static BLK_CANCEL: LazyLock<Block> =
    LazyLock::new(|| Block::bordered().border_type(BorderType::Rounded).red());
static LINE_PAUSED_ON: LazyLock<Line> =
    LazyLock::new(|| Line::from(vec![Span::raw(CHECK_OK), Span::raw(INPUT_PAUSED)]));
static LINE_PAUSED_OFF: LazyLock<Line> =
    LazyLock::new(|| Line::from(vec![Span::raw(CHECK_EMPTY), Span::raw(INPUT_PAUSED)]));

/// Layout for the add target input dialog.
#[derive(Debug)]
pub(crate) struct AddTgtDialog {
    pub area: Rect,
    addrs: Rect,
    excls: Rect,
    paused: Rect,
    block: Block<'static>,
    b_addrs: Block<'static>,
    b_excls: Block<'static>,
    btn_submit: Rect,
    btn_cancel: Rect,
    error_help: Rect,
}

impl AddTgtDialog {
    fn calc_layout(area: Rect, block: &Block) -> (Rc<[Rect]>, Rc<[Rect]>) {
        // split into rows
        let rows = Layout::vertical([
            CON_LEN_3, // addrs
            CON_LEN_3, // excls
            CON_LEN_3, // paused + buttons
            CON_LEN_1, // error/help
        ])
        .split(block.inner(area));

        // split button row into parts
        let btn_row = Layout::horizontal([
            Constraint::Length(LINE_PAUSED_OFF.width() as u16 + 2), // paused checkbox
            CON_FILL,                                               // spacer
            CON_LEN_10,                                             // submit
            CON_LEN_10,                                             // cancel
        ])
        .spacing(1)
        .split(rows[2]);
        (rows, btn_row)
    }

    /// Update the layout based on the given input area.
    pub fn update(&mut self, area: Rect) {
        let (rows, btn_row) = Self::calc_layout(area, &self.block);
        self.area = area;
        self.addrs = rows[0];
        self.excls = rows[1];
        self.paused = btn_row[0];
        self.btn_submit = btn_row[2];
        self.btn_cancel = btn_row[3];
        self.error_help = rows[3];
    }

    /// Cursor placement: only when editing a text field.
    pub fn cursor_position(&self, state: &AddTargetDialogState) -> Option<(u16, u16)> {
        match state.active {
            ActiveField::Addresses => {
                let inner = self.b_addrs.inner(self.addrs);
                // cursor: left border + cursor position within input
                Some((
                    inner.x + state.addrs.cursor() as u16,
                    inner.y, // single-line in a 1-row inner area
                ))
            }
            ActiveField::Exclusions => {
                let inner = self.b_excls.inner(self.excls);
                Some((inner.x + state.excls.cursor() as u16, inner.y))
            }
            _ => None, /* not in an input field -> cursor stays hidden */
        }
    }
}

impl Default for AddTgtDialog {
    fn default() -> Self {
        Self {
            area: Rect::default(),
            block: Block::bordered()
                .border_type(BorderType::QuadrantOutside)
                .padding(Padding::proportional(1))
                .title(INPUT_TITLE),
            b_addrs: Block::bordered().title(INPUT_ADDRS),
            b_excls: Block::bordered().title(INPUT_EXCLS),
            addrs: Rect::default(),
            excls: Rect::default(),
            paused: Rect::default(),
            btn_submit: Rect::default(),
            btn_cancel: Rect::default(),
            error_help: Rect::default(),
        }
    }
}

impl StatefulWidget for &AddTgtDialog {
    type State = AddTargetDialogState;

    #[rustfmt::skip]
    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        Clear.render(area, buf);
        (&self.block).render(area, buf);

        // render blocks
        if state.active == ActiveField::Addresses {
            self.b_addrs.clone().border_type(BorderType::Double).render(self.addrs, buf);
        } else {
            (&self.b_addrs).render(self.addrs, buf);
        }
        if state.active == ActiveField::Exclusions {
            self.b_excls.clone().border_type(BorderType::Double).render(self.excls, buf);
        } else {
            (&self.b_excls).render(self.excls, buf);
        }
        if state.active == ActiveField::Paused {
            BORDERS.clone().border_type(BorderType::Double).render(self.paused, buf);
        } else {
            (&BORDERS).render(self.paused, buf);
        };
        (&*BLK_SUBMIT).render(self.btn_submit, buf);
        (&*BLK_CANCEL).render(self.btn_cancel, buf);

        // addresses/exclusions input
        Paragraph::new(state.addrs.value()).wrap(NO_TRIM).render((&self.b_addrs).inner(self.addrs), buf);
        Paragraph::new(state.excls.value()).wrap(NO_TRIM).render((&self.b_excls).inner(self.excls), buf);

        // checkbox area
        if state.paused {
            (&*LINE_PAUSED_ON).render(BORDERS.inner(self.paused), buf);
        } else {
            (&*LINE_PAUSED_OFF).render(BORDERS.inner(self.paused), buf);
        };

        // buttons
        let submit_inner = BLK_SUBMIT.inner(self.btn_submit);
        let cancel_inner = BLK_CANCEL.inner(self.btn_cancel);
        if state.active == ActiveField::Submit {
            SUBMIT.clone().reversed().render(submit_inner, buf)
        } else {
            (&*SUBMIT).render(submit_inner, buf)
        };
        if state.active == ActiveField::Cancel {
            CANCEL.clone().reversed().render(cancel_inner, buf)
        } else {
            (&*CANCEL).render(cancel_inner, buf)
        };

        // error/help
        if let Some(err) = &state.error {
            Paragraph::new(err.as_str()).wrap(NO_TRIM).render(self.error_help, buf);
        }
    }
}
