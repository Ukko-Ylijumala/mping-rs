// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{
    macros::{delegate_read, delegate_write},
    strings::*,
};
use crossterm::{
    cursor::{Hide, Show},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use parking_lot::RwLock;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Cell, Row, TableState},
};
use std::{
    fmt,
    io::{Result, Stdout, stdout},
    ops::Index,
    panic,
};

const TBL_WASTED_ROWS: u16 = 3; // borders + header
const TBL_WASTED_COLS: u16 = 2; // borders

#[derive(Debug, Default)]
/**
Layout structure for Ratatui frames.

Create the initial layout with [AppLayout::default], then call
[AppLayout::update] on each frame render to adjust to terminal
size changes etc. Update is a no-op if the size hasn't changed.

Current layout:

```text
+-------------------------+
|        > title <        | (1 line)
+-------------------------+
|                         |
| middle                  |
|                         |
+-------------------------+
|> status    |  procinfo <| (1 line)
+-------------------------+

Middle is further divided into sections (table has priority):
+-----------------+-------+
|                 | info  |
|                 | upper |
|                 |       |
|  table          |       |
|                 |-------|
|                 | info  |
|                 | lower |
+-----------------+-------+

And finally the info upper section is divided into:
+-----------+
|           |
|   text    |
|           |
|-----------|
|           |
|   graph   |
|           |
|-----------|
|           |
| histogram |
|           |
+-----------+
```
We also define the following modal areas:
- `popup`: centered text box (for multiline text)
- `input`: centered input area (for modal text input)
*/
pub(crate) struct AppLayout {
    /// Full frame area
    pub frame: Rect,
    /// Title bar - top line
    pub title: Rect,
    /// Main table area
    pub table: Rect,
    /// Info area (right side) - upper part
    pub info_upper: Rect,
    /// Info area (right side) - upper part - text lines
    pub i_upper_text: Rect,
    /// Info area (right side) - upper part - graph
    pub i_upper_graph: Rect,
    /// Info area (right side) - upper part - histogram
    pub i_upper_histo: Rect,
    /// Info area (right side) - lower part
    pub info_lower: Rect,
    /// Status bar area - bottom line - left side
    pub status_l: Rect,
    /// Status bar area - bottom line - right side (process info)
    pub status_r: Rect,
    /// Popup area for help text
    pub help: Rect,
    /// Popup area for multiline text etc
    pub popup: Rect,
    /// Input area for text input etc
    pub input: Rect,
    /// Spacing between table columns
    pub tbl_colspacing: u16,
    /// Current column width [Constraint]s
    pub tbl_constraints: Vec<Constraint>,
    /// Stateful table state for managing selection, scrolling, etc.
    pub tablestate: TableState,
    pub help_visible: bool,
    pub popup_visible: bool,
    pub input_visible: bool,
    /// Middle area - not directly accessible, split into table and info
    middle: Rect,
    /// Info area - not directly accessible, split into upper and lower
    info: Rect,
    /// Status line area - not directly accessible, split into left and right
    status: Rect,
    /// Precomputed visible widths of table headers
    tbl_hdr_widths: Vec<usize>,
    /// Current total table width (without spacing)
    tbl_width: u16,
    help_rows: u16,
    help_cols: u16,
}

impl AppLayout {
    /// Set table column spacing.
    #[must_use = "builder pattern"]
    pub fn spacing(self, spacing: u16) -> Self {
        Self {
            tbl_colspacing: spacing,
            ..self
        }
    }

    /// Set table header widths.
    #[must_use = "builder pattern"]
    pub fn widths(self, widths: Vec<usize>) -> Self {
        Self {
            tbl_hdr_widths: widths,
            ..self
        }
    }

    /// Setup the initial help area with given rows and columns.
    pub fn setup_help_area(&mut self, rows: u16, cols: u16) {
        self.help_rows = rows + 2; // box
        self.help_cols = cols + 4; // box + margins
    }

    /// Reset the table widths to initial state (based on header widths).
    pub fn reset_table_widths(&mut self) {
        let mut sum_widths: usize = 0;
        self.tbl_constraints = self
            .tbl_hdr_widths
            .iter()
            .map(|w| {
                sum_widths += w;
                Constraint::Length(*w as u16)
            })
            .collect();
        self.tbl_width = sum_widths as u16;
    }

    /**
    Update the layout based on the full frame area (if it has changed),
    and the table size (if needed).

    Updated column [Constraint]s are available afterwards in `tbl_constraints`.
    */
    pub fn maybe_update(&mut self, frame: Rect, data: &[TableRow]) {
        let tbl_width: u16 = self.update_col_widths(data);

        if frame != self.frame {
            // must recalculate all if frame size has changed
            self.update(frame);
        } else if tbl_width != self.tbl_width {
            // Ensure the table area does not shrink from its previous size.
            // Constant resizes are annoying and distracting.
            self.tbl_width = self.tbl_width.max(tbl_width);

            // update table and info areas since table width changed
            self.update_middle();
            self.update_info_areas();
        }
    }

    /// Update top-level areas (title line, middle area, status line).
    fn update_main_areas(&mut self, frame: Rect) {
        // Create vertical layout
        (self.title, self.middle, self.status) = {
            let full = Layout::vertical([
                Constraint::Length(1), // title - 1 line
                Constraint::Min(1),    // table
                Constraint::Length(1), // status - 1 line
            ])
            .split(frame);
            (full[0], full[1], full[2])
        };

        // Store the current frame for future comparisons
        self.frame = frame;
    }

    /// Split status line between left and right parts.
    fn update_status_line(&mut self) {
        // split status into left and right sides
        (self.status_l, self.status_r) = {
            let status = Layout::horizontal([
                Constraint::Fill(1), // left side
                Constraint::Min(43), // right side (process info)
            ])
            .split(self.status);
            (status[0], status[1])
        };
    }

    /// Update middle area (contains table + info areas).
    #[inline]
    fn update_middle(&mut self) {
        // split middle into table and info areas with table size being fixed
        let spacing: u16 = self.tbl_colspacing * (self.tbl_hdr_widths.len() as u16 - 1);
        (self.table, self.info) = {
            let middle = Layout::horizontal([
                Constraint::Max(self.tbl_width + spacing + TBL_WASTED_COLS), // table + borders
                Constraint::Fill(1),                                         // info
            ])
            .split(self.middle);
            (middle[0], middle[1])
        };
    }

    /// Update info areas (nested in `self.info`).
    #[inline]
    fn update_info_areas(&mut self) {
        // split info into upper and lower parts
        (self.info_upper, self.info_lower) = {
            let info = Layout::vertical([
                Constraint::Fill(1),   // upper info
                Constraint::Length(5), // lower info
            ])
            .split(self.info);
            (info[0], info[1])
        };

        // Split info_upper into [text, graph, hist] areas
        (self.i_upper_text, self.i_upper_graph, self.i_upper_histo) = {
            let info_split = Layout::vertical([
                Constraint::Length(7),
                Constraint::Length(20),
                Constraint::Length(11),
            ])
            .split(self.info_upper);
            (info_split[0], info_split[1], info_split[2])
        };
    }

    /// Update popup and input areas (based on current full frame size).
    fn update_popup_areas(&mut self) {
        // centered help popup area
        self.help.x = self.frame.width.saturating_sub(self.help_cols) / 2;
        self.help.y = self.frame.height.saturating_sub(self.help_rows) / 2;
        self.help.width = self.help_cols.min(self.frame.width); // if this is larger than frame, Ratatui will panic
        self.help.height = self.help_rows.min(self.frame.height); // ditto

        // centered popup area (75% width/height)
        self.popup.x = self.frame.width / 8;
        self.popup.y = self.frame.height / 8;
        self.popup.width = self.frame.width * 3 / 4;
        self.popup.height = self.frame.height * 3 / 4;

        // centered input area (30% width, 5 lines height)
        self.input.x = self.frame.width * 35 / 100;
        self.input.y = self.frame.height / 2 - 1;
        self.input.width = self.frame.width * 30 / 100;
        self.input.height = 5;
    }

    /// Recalculate the layout areas regardless of if it's needed or not.
    pub fn update(&mut self, frame: Rect) {
        // update main areas first
        self.update_main_areas(frame);
        self.update_status_line();
        self.update_middle();
        self.update_info_areas();
        self.update_popup_areas();
    }

    /// Get the number of usable rows in the table area (excluding borders and header).
    pub fn tbl_usable_rows(&self) -> usize {
        self.table.height.saturating_sub(TBL_WASTED_ROWS) as usize
    }

    /// Update column widths based on data. Returns total table width without any spacing.
    pub fn update_col_widths(&mut self, data: &[TableRow]) -> u16 {
        // Start with header widths as minimums
        let mut widths: Vec<usize> = self.tbl_hdr_widths.clone();
        let mut sum_widths: usize = 0;

        for row in data {
            for (i, item) in row.iter().enumerate() {
                // Consider existing constraint as minimum (ie. columns can grow but won't shrink)
                let cur_constr: usize = match self.tbl_constraints.get(i) {
                    Some(Constraint::Min(n)) => *n as usize,
                    Some(Constraint::Max(n)) => *n as usize,
                    Some(Constraint::Length(n)) => *n as usize,
                    Some(Constraint::Percentage(n)) => *n as usize,
                    _ => 1,
                };
                widths[i] = widths[i].max(item.len().max(cur_constr));
            }
        }
        self.tbl_constraints = widths
            .iter()
            .map(|w| {
                sum_widths += w;
                Constraint::Length(*w as u16)
            })
            .collect();

        sum_widths as u16
    }
}

/* -------------------------------------------------------------------------- */

/// Mutable convenience line wrapper for Ratatui [Line]s. It allows
/// in-place modification of the line content and styling.
#[derive(Debug, Default)]
pub(crate) struct MutableLine<'a>(RwLock<Line<'a>>);

impl<'a> MutableLine<'a> {
    pub fn new() -> Self {
        Self {
            0: Line::default().into(),
        }
    }

    pub fn new_from<T: Into<Line<'a>>>(s: T) -> Self {
        Self { 0: s.into().into() }
    }

    /// Apply a [Style] to a (new) MutableLine using a (consuming) builder pattern.
    pub fn with_style(self, s: Style) -> Self {
        {
            // Take the Line out of the lock, apply the consuming style(...) which
            // returns a new Line, and put it back to avoid moving out of the guard.
            let mut lock = self.0.write();
            let updated = std::mem::take(&mut *lock).style(s);
            *lock = updated;
        }
        self
    }

    /// Read access to the inner Line via a closure.
    #[inline]
    pub fn with<R>(&self, f: impl FnOnce(&Line<'a>) -> R) -> R {
        f(&*self.0.read())
    }

    /// Write access to the inner Line via a closure.
    #[inline]
    pub fn with_mut<R>(&self, f: impl FnOnce(&mut Line<'a>) -> R) -> R {
        f(&mut *self.0.write())
    }

    /// Try read access to the inner Line via a closure that may fail.
    #[inline]
    pub fn try_with<R>(&self, f: impl FnOnce(&Line<'a>) -> Option<R>) -> Option<R> {
        f(&*self.0.read())
    }

    /// Replace the entire line
    pub fn replace<T: Into<Line<'a>>>(&self, l: T) {
        *self.0.write() = l.into();
    }

    /// Clear the line content.
    pub fn clear(&self) {
        *self.0.write() = Line::default();
    }

    /// Get a clone of the current inner [Line] (for rendering)
    #[inline]
    pub fn as_line(&self) -> Line<'a> {
        self.with(|l| l.clone())
    }

    // Pass-through methods to Line via macros
    delegate_read!(width -> usize);
    delegate_read!(to_string -> String);
    delegate_write!(bold);
    delegate_write!(italic);
    delegate_write!(underlined);
    delegate_write!(reset_style);
    delegate_write!(style, style: Style);
    delegate_write!(push_span, <T: Into<Span<'a>>>, span: T);
}

impl Clone for MutableLine<'_> {
    fn clone(&self) -> Self {
        Self {
            0: RwLock::new(self.0.read().clone()),
        }
    }
}

// Implement From for arbitrary text types for MutableLine
impl<'a, T: Into<Line<'a>>> From<T> for MutableLine<'a> {
    fn from(item: T) -> Self {
        MutableLine::new_from(item)
    }
}

impl fmt::Display for MutableLine<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for span in &self.0.read().spans {
            write!(f, "{span}")?;
        }
        Ok(())
    }
}

/* -------------------------------------------------------------------------- */

/// Single table item (think: [Cell]) with styling and constraints for Ratatui tables.
#[derive(Debug, Default, Clone)]
pub(crate) struct TableItem {
    txt: String,
    constr: Constraint,
    style: Style,
}

impl TableItem {
    pub fn new<S: AsRef<str>>(i: S) -> Self {
        let txt: String = i.as_ref().to_string();
        Self {
            constr: Constraint::Length(txt.len() as u16),
            style: Style::default(),
            txt,
        }
    }

    pub fn bold(mut self) -> Self {
        self.style = self.style.add_modifier(Modifier::BOLD);
        self
    }

    pub fn color(mut self, c: Color) -> Self {
        self.style = self.style.fg(c);
        self
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.txt.len()
    }

    pub fn set_style(&mut self, s: Style) {
        self.style = s;
    }

    #[inline]
    pub fn as_str(&self) -> &str {
        &self.txt
    }

    /// Convert to a Ratatui [Cell] with appropriate styling.
    #[inline]
    pub fn as_cell(&'_ self) -> Cell<'_> {
        Cell::from(self.as_str()).style(self.style)
    }
}

impl fmt::Display for TableItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.txt)
    }
}

/// Row of [TableItem]s for Ratatui tables. Each item carries its own styling already.
#[derive(Debug, Default, Clone)]
pub(crate) struct TableRow {
    items: Vec<TableItem>,
}

impl TableRow {
    /// Create a new empty [TableRow].
    pub fn new() -> Self {
        Self { items: Vec::new() }
    }

    pub fn new_with_capacity(cap: usize) -> Self {
        Self {
            items: Vec::with_capacity(cap),
        }
    }

    /// Create a row with given number of empty cells.
    pub fn empty(cells: usize) -> Self {
        Self {
            items: vec![TableItem::default(); cells],
        }
    }

    /// Create a [TableRow] from an iterator of items that can be converted to strings.
    pub fn from_iter<I, S>(iter: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self {
            items: iter.into_iter().map(TableItem::new).collect(),
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn add_item<I: AsRef<str>>(&mut self, item: I) -> usize {
        let idx: usize = self.len();
        self.items.push(TableItem::new(item));
        idx
    }

    /// Set the style for a specific item in this row.
    pub fn set_style(&mut self, idx: usize, style: Style) {
        if let Some(item) = self.items.get_mut(idx) {
            item.set_style(style);
        }
    }

    /// Set the same style for all items in this row.
    pub fn set_style_all(&mut self, style: Style) {
        for item in &mut self.items {
            item.set_style(style);
        }
    }

    /// Set different styles for each item in this row.
    pub fn set_styles(&mut self, styles: &[Style]) {
        for (idx, style) in styles.iter().enumerate() {
            if let Some(item) = self.items.get_mut(idx) {
                item.set_style(*style);
            }
        }
    }

    /// Get the total visible width of this row including spacing.
    pub fn width(&self, spacing: u16) -> u16 {
        let items: usize = self.iter().map(|i| i.len()).sum();
        let spacing: u16 = spacing * (self.len() as u16 - 1);
        items as u16 + spacing
    }

    #[inline]
    pub fn iter(&'_ self) -> std::slice::Iter<'_, TableItem> {
        self.items.iter()
    }

    /// Yield the [Cell]s for all items in this row.
    pub fn cells(&'_ self) -> impl Iterator<Item = Cell<'_>> {
        self.iter().map(|i| i.as_cell())
    }

    /// Get the visible widths of each item in this row.
    pub fn widths(&self) -> Vec<usize> {
        self.iter().map(|i| i.len()).collect()
    }

    pub fn strings(&self) -> Vec<&str> {
        self.iter().map(|i| i.as_str()).collect()
    }
}

impl<'a> IntoIterator for &'a TableRow {
    type Item = &'a TableItem;
    type IntoIter = std::slice::Iter<'a, TableItem>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.iter()
    }
}

impl<'a> From<&'a TableRow> for Row<'a> {
    fn from(tr: &'a TableRow) -> Self {
        if tr.is_empty() {
            // don't use Row::default() as Ratatui apparently skips these entirely
            return Row::new(Vec::<Cell>::new());
        }
        Row::new(tr.cells())
    }
}

impl Index<usize> for TableRow {
    type Output = TableItem;

    fn index(&self, index: usize) -> &Self::Output {
        &self.items[index]
    }
}

/* -------------------------------------------------------------------------- */

/**
RAII guard object for TUI console using [ratatui] and [crossterm].
- sets up a panic handler to restore normal terminal on panic
- initializes a full-screen TUI on creation (the RAII part)
- restores the normal terminal on drop (automatic cleanup)
*/
pub struct TerminalGuard {
    pub term: Terminal<CrosstermBackend<Stdout>>,
    verbose: bool,
}

impl TerminalGuard {
    pub fn new(interval_ms: u128, verbose: bool) -> Result<Self> {
        if verbose {
            let hz: f64 = 1e3 / interval_ms as f64;
            eprintln!("{TUI_INIT}: {hz:.1} Hz.");
        }

        // set up the ratatui/crossterm environment (panic hook first!)
        panic::set_hook(Box::new(panic_handler));
        enable_raw_mode()?;
        let mut stdout: Stdout = stdout();
        execute!(stdout, EnterAlternateScreen, Hide)?;

        Ok(Self {
            term: Terminal::new(CrosstermBackend::new(stdout))?,
            verbose,
        })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        terminal_teardown(self.verbose);
    }
}

/// Tear down the terminal environment cleanly. Restores terminal to a sane state.
fn terminal_teardown(verbose: bool) {
    let _ = disable_raw_mode();
    let _ = execute!(stdout(), LeaveAlternateScreen, Show);

    if verbose {
        eprintln!("{TUI_TERMINATE}");
    }
}

/// Panic handler to restore the console to a sane state if a panic occurs
pub(crate) fn panic_handler(info: &panic::PanicHookInfo) {
    terminal_teardown(true);
    eprintln!("{APP_PANIC}: {}", info);
}
