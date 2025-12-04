// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use crossterm::{
    cursor::{Hide, Show},
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::Cell,
};
use std::{
    fmt,
    io::{Result, Stdout, stdout},
    panic,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering::Relaxed},
    },
    time::Duration,
};

#[derive(Debug, Default)]
/// Layout structure for Ratatui frames.
///
/// Create the initial layout with [AppLayout::default()], then call
/// [AppLayout::update()] on each frame render to adjust to any terminal
/// size changes. Update is a no-op if the size hasn't changed.
///
/// Current layout:
///
/// ```text
/// |   title   |  (1 line)
/// +-----------+
/// |           |
/// |  middle   |
/// |           |
/// +-----------+
/// |   status  |  (1 line)
///
/// Middle is further divided into two sections with table having priority:
/// +----------------------+
/// |               |      |
/// |  table        | info |
/// |               |      |
/// +----------------------+
/// ```
pub(crate) struct AppLayout {
    /// Full frame area
    pub frame: Rect,
    /// Title bar - top line
    pub title: Rect,
    /// Main table area
    pub table: Rect,
    /// Info area (right side)
    pub info: Rect,
    /// Status bar area - bottom line
    pub status: Rect,
    /// Precomputed visible widths of table headers
    pub tbl_hdr_widths: Vec<usize>,
    /// Spacing between table columns
    pub tbl_colspacing: u16,
    /// Current column width Constraints
    pub tbl_constraints: Vec<Constraint>,
    tbl_width: u16,
}

impl AppLayout {
    /// Update the layout based on the full frame area (if it has changed),
    /// and the table size (if needed). Updated column [Constraint]s are available
    /// after this call in `tbl_constraints`.
    pub fn update(&mut self, frame: Rect, data: &[TableRow]) {
        // No need to recalculate if frame size and table size are unchanged
        let tblsize: u16 = self.update_col_widths(data);
        if frame == self.frame && tblsize == self.tbl_width {
            return;
        };

        // Ensure the table area does not shrink from its previous size.
        // Constant resizes are annoying and distracting.
        self.tbl_width = self.tbl_width.max(tblsize);

        // Create vertical layout
        let full: Rc<[Rect]> = Layout::vertical([
            Constraint::Length(1), // title - 1 line
            Constraint::Min(1),    // table
            Constraint::Length(1), // status - 1 line
        ])
        .split(frame);
        let (title, middle, status) = (full[0], full[1], full[2]);

        // split middle into table and info areas with table size being fixed
        let spacing: u16 = self.tbl_colspacing * (self.tbl_hdr_widths.len() as u16 - 1);
        let middle: Rc<[Rect]> = Layout::horizontal([
            Constraint::Min(self.tbl_width + spacing + 2), // table + borders
            Constraint::Fill(1),                           // info
        ])
        .split(middle);
        let (table, info) = (middle[0], middle[1]);

        // Update layout rectangles
        self.frame = frame;
        self.title = title;
        self.table = table;
        self.info = info;
        self.status = status;
    }

    /// Update column widths based on data.
    fn update_col_widths(&mut self, data: &[TableRow]) -> u16 {
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
                sum_widths += *w;
                Constraint::Length(*w as u16)
            })
            .collect();

        sum_widths as u16
    }
}

////////////////////////////////////////////////////////////////////////////////

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
#[derive(Debug, Default)]
pub(crate) struct TableRow {
    items: Vec<TableItem>,
}

impl TableRow {
    pub fn from_iter<I, S>(iter: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut items: Vec<TableItem> = Vec::new();
        for i in iter {
            items.push(TableItem::new(i));
        }
        Self { items }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.items.len()
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

    /// Yield the cells for all items in this row.
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

////////////////////////////////////////////////////////////////////////////////

/// RAII guard object for TUI console using [ratatui] and [crossterm].
/// - sets up a panic handler to restore normal terminal on panic
/// - initializes a full-screen TUI on creation (the RAII part)
/// - restores the normal terminal on drop (automatic cleanup)
pub struct TerminalGuard {
    pub term: Terminal<CrosstermBackend<Stdout>>,
    verbose: bool,
}

impl TerminalGuard {
    pub fn new(interval_ms: u128, verbose: bool) -> Result<Self> {
        if verbose {
            let hz: f64 = 1e3 / interval_ms as f64;
            eprintln!("Initializing terminal UI (display refresh rate: {hz:.1} Hz)...");
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
        eprintln!("Terminal UI was terminated.");
    }
}

/// Panic handler to restore the console to a sane state if a panic occurs
pub(crate) fn panic_handler(info: &panic::PanicHookInfo) {
    terminal_teardown(true);
    eprintln!("Application panic: {}", info);
}

////////////////////////////////////////////////////////////////////////////////

/// Crossterm key event polling helper
pub(crate) fn key_event_poll(wait_ms: u64, quit: &Arc<AtomicBool>) -> Result<()> {
    if event::poll(Duration::from_millis(wait_ms))? {
        if let Event::Key(e) = event::read()? {
            match (e.code, e.modifiers) {
                (KeyCode::Char('q'), _) => Ok(quit.store(true, Relaxed)),
                // terminal in raw mode -> ctrl-c has to be processed manually
                (KeyCode::Char('c'), KeyModifiers::CONTROL) => Ok(quit.store(true, Relaxed)),
                _ => Ok(()),
            }
        } else {
            Ok(())
        }
    } else {
        Ok(())
    }
}
