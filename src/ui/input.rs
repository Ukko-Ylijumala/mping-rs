// Copyright (c) 2025-2026 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{pinger::AddOutcome, strings::*};
use crossterm::event::{Event, KeyCode, KeyEvent};
use miniutils::{inject, templater};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span, Text},
    widgets::*,
};
use std::{fmt::Display, net::IpAddr, rc::Rc, sync::LazyLock};
use tui_input::{Input, backend::crossterm::EventHandler};

/// Per-section cap for the post-submit summary in the dialog feedback area.
const SUMMARY_SECTION_CAP: usize = 12;

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

/**
What's currently showing in the dialog's feedback area below the input fields.

The dialog stays open across submissions; this enum drives what - if anything -
is rendered between the field rows and the bottom of the dialog. The variants
are mutually exclusive: a validation error replaces any earlier summary, a
new submission replaces the prior error or summary, etc.
*/
#[derive(Debug, Default, Clone)]
pub(crate) enum DialogFeedback {
    #[default]
    None,
    /// Pre-submit validation error (e.g. empty addresses field).
    Error(String),
    /// A submission is currently being parsed / DNS-resolved off-thread.
    Working,
    /// Submission complete; rendered as styled multi-line text.
    Summary(Text<'static>),
}

impl DialogFeedback {
    /// Is a submission currently in flight? Used to block re-submits.
    #[inline]
    pub fn is_working(&self) -> bool {
        matches!(self, Self::Working)
    }
}

/// State for the add target dialog input overlay.
#[derive(Debug, Default, Clone)]
pub(crate) struct AddTargetDialogState {
    pub addrs: Input,          // Text input for addresses
    pub excls: Input,          // Text input for exclusions
    pub paused: bool,          // Checkbox for paused status
    pub active: ActiveField,   // Currently active field
    pub feedback: DialogFeedback, // Validation error / Working / Summary
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
                    ActiveField::Submit => {
                        // Block re-submit while a previous one is still resolving.
                        if self.feedback.is_working() {
                            DialogAction::None
                        } else {
                            DialogAction::Submit {
                                addrs: self.addrs.value().to_string(),
                                excls: self.excls.value().to_string(),
                                paused: self.paused,
                            }
                        }
                    }
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
    feedback: Rect,
}

impl AddTgtDialog {
    fn calc_layout(area: Rect, block: &Block) -> (Rc<[Rect]>, Rc<[Rect]>) {
        // split into rows: three fixed-height input rows then everything left
        // over goes to the feedback area (errors or post-submit summary).
        let rows = Layout::vertical([
            CON_LEN_3, // addrs
            CON_LEN_3, // excls
            CON_LEN_3, // paused + buttons
            CON_FILL,  // feedback (error / Working / Summary)
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
        self.feedback = rows[3];
    }

    /// Cursor placement: only when editing a text field.
    pub fn cursor_position(&self, state: &AddTargetDialogState) -> Option<(u16, u16)> {
        match state.active {
            ActiveField::Addresses => {
                let inner = self.b_addrs.inner(self.addrs);
                Some(field_cursor_pos(&state.addrs, inner))
            }
            ActiveField::Exclusions => {
                let inner = self.b_excls.inner(self.excls);
                Some(field_cursor_pos(&state.excls, inner))
            }
            _ => None, /* not in an input field -> cursor stays hidden */
        }
    }
}

/// Usable width of a single-line input field (one column is reserved for the cursor).
#[inline]
fn field_width(inner: Rect) -> usize {
    inner.width.saturating_sub(1) as usize
}

/// Cursor position for a (possibly horizontally scrolled) single-line input field.
/// The input scrolls when the value is wider than the field, so the cursor is
/// offset by the scroll amount and clamped to stay inside the field.
#[inline]
fn field_cursor_pos(input: &Input, inner: Rect) -> (u16, u16) {
    let width: usize = field_width(inner);
    let scroll: usize = input.visual_scroll(width);
    let x: u16 = (input.visual_cursor().saturating_sub(scroll)).min(width) as u16;
    (inner.x + x, inner.y) // single-line in a 1-row inner area
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
            feedback: Rect::default(),
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

        // addresses/exclusions input: scroll horizontally (in sync with the
        // cursor math in `field_cursor_pos`) instead of wrapping
        let addrs_inner = (&self.b_addrs).inner(self.addrs);
        let addrs_scroll = state.addrs.visual_scroll(field_width(addrs_inner)) as u16;
        Paragraph::new(state.addrs.value()).scroll((0, addrs_scroll)).render(addrs_inner, buf);
        let excls_inner = (&self.b_excls).inner(self.excls);
        let excls_scroll = state.excls.visual_scroll(field_width(excls_inner)) as u16;
        Paragraph::new(state.excls.value()).scroll((0, excls_scroll)).render(excls_inner, buf);

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

        // feedback area: validation error / "Working..." / post-submit summary
        match &state.feedback {
            DialogFeedback::None => {}
            DialogFeedback::Error(msg) => {
                Paragraph::new(msg.as_str())
                    .wrap(NO_TRIM)
                    .style(Style::new().light_red().bold())
                    .render(self.feedback, buf);
            }
            DialogFeedback::Working => {
                Paragraph::new(INPUT_WORKING)
                    .wrap(NO_TRIM)
                    .style(Style::new().yellow().italic())
                    .render(self.feedback, buf);
            }
            DialogFeedback::Summary(text) => {
                Paragraph::new(text.clone())
                    .wrap(NO_TRIM)
                    .render(self.feedback, buf);
            }
        }
    }
}

/* -------------------------------------------------------------------------- */

/**
Build the dialog's post-submit feedback text from a completed [AddOutcome].

Output: one status header line, an optional `(start paused)` annotation
when applicable, then up to four single-line per-category breakdowns
(added, skipped, resolved-names, unresolved), a blank line, and the
"press Esc to close..." hint. Each per-category list is truncated to
[SUMMARY_SECTION_CAP] entries with a "…and N more" tail.
*/
pub(crate) fn format_add_summary(outcome: &AddOutcome, paused: bool) -> Text<'static> {
    let added = outcome.added.len();
    let skipped = outcome.skipped;
    let resolved = outcome.collected.resolved.len();
    let unresolved = outcome.collected.unresolved.len();
    let excluded = outcome.collected.excluded.len();

    let mut lines: Vec<Line<'static>> = Vec::new();

    // Header line: a comma-joined run of "<label>: <count>" segments,
    // each segment coloured by category.
    let mut header: Vec<Span<'static>> = Vec::new();
    push_count(&mut header, SUMMARY_ADDED, added, Color::Green);
    push_count(&mut header, SUMMARY_SKIPPED, skipped, Color::Yellow);
    push_count(&mut header, SUMMARY_RESOLVED, resolved, Color::Cyan);
    push_count(&mut header, SUMMARY_UNRESOLVED, unresolved, Color::LightRed);
    push_count(&mut header, SUMMARY_EXCLUDED, excluded, Color::DarkGray);
    if header.is_empty() {
        lines.push(Line::from(Span::styled(
            INPUT_NOTHING,
            Style::new().yellow().italic(),
        )));
    } else {
        lines.push(Line::from(header));
        if paused && added > 0 {
            lines.push(Line::from(Span::styled(
                "  (started paused)",
                Style::new().dim().italic(),
            )));
        }
    }

    // Per-category one-liners. Only emit when the category is non-empty.
    if added > 0 {
        let entries: Vec<String> = outcome
            .added
            .iter()
            .map(|tgt| match tgt.name() {
                Some(n) => format!("{} ({n})", tgt.addr),
                None => tgt.addr.to_string(),
            })
            .collect();
        lines.push(section_line(SUMMARY_ADDED, &entries, Color::Green));
    }
    if skipped > 0 {
        // Reconstruct the duplicates list from collected.addrs minus added.
        let added_set: std::collections::HashSet<IpAddr> =
            outcome.added.iter().map(|t| t.addr).collect();
        let dupes: Vec<String> = outcome
            .collected
            .addrs
            .iter()
            .filter(|ip| !added_set.contains(*ip))
            .map(IpAddr::to_string)
            .collect();
        if !dupes.is_empty() {
            lines.push(section_line(SUMMARY_SKIPPED, &dupes, Color::Yellow));
        }
    }
    if resolved > 0 {
        let entries: Vec<String> = outcome
            .collected
            .resolved
            .iter()
            .map(|(name, ips)| format!("{name} → {} addr", ips.len()))
            .collect();
        lines.push(section_line(SUMMARY_RESOLVED, &entries, Color::Cyan));
    }
    if unresolved > 0 {
        let entries: Vec<String> = outcome.collected.unresolved.iter().cloned().collect();
        lines.push(section_line(SUMMARY_UNRESOLVED, &entries, Color::LightRed));
    }

    // Trailing blank + close hint.
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        INPUT_HINT_CLOSE,
        Style::new().dim(),
    )));

    Text::from(lines)
}

/// Push `"<label>: <count>"` to `out` when `count > 0`, comma-separating
/// from any previous entry.
fn push_count(out: &mut Vec<Span<'static>>, label: &'static str, count: usize, colour: Color) {
    if count == 0 {
        return;
    }
    if !out.is_empty() {
        out.push(Span::raw(", "));
    }
    out.push(Span::styled(
        format!("{label}: {count}"),
        Style::new().fg(colour).bold(),
    ));
}

/// Build one summary section line: `"  <label>: a, b, c, …and N more"`.
fn section_line(label: &'static str, entries: &[String], colour: Color) -> Line<'static> {
    let visible: usize = entries.len().min(SUMMARY_SECTION_CAP);
    let mut body = entries[..visible].join(", ");
    let remainder: usize = entries.len().saturating_sub(visible);
    if remainder > 0 {
        body.push_str(", ");
        body.push_str(&templater!(SUMMARY_AND_MORE, remainder));
    }
    Line::from(vec![
        Span::styled(format!("  {label}: "), Style::new().fg(colour)),
        Span::raw(body),
    ])
}
