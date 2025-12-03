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
};
use std::{
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
/// Current layot:
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
    tblsize: u16,
}

impl AppLayout {
    /// Update the layout based on the full frame area if it has changed.
    pub fn update(&mut self, frame: Rect, tblsize: u16) {
        // No need to recalculate if frame size and table size are unchanged
        if frame == self.frame && tblsize == self.tblsize {
            return;
        };

        // Create vertical layout
        let full: Rc<[Rect]> = Layout::vertical([
            Constraint::Length(1), // title - 1 line
            Constraint::Min(1),    // table
            Constraint::Length(1), // status - 1 line
        ])
        .split(frame);
        let (title, middle, status) = (full[0], full[1], full[2]);

        // split middle into table and info areas with table size being fixed
        let middle: Rc<[Rect]> = Layout::horizontal([
            Constraint::Min(tblsize), // table
            Constraint::Fill(1),      // info
        ])
        .split(middle);
        let (table, info) = (middle[0], middle[1]);

        // Update layout rectangles
        self.frame = frame;
        self.title = title;
        self.table = table;
        self.info = info;
        self.status = status;
        self.tblsize = tblsize;
    }
}

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

/// Find the maximum width needed for each column. Returns a tuple of:
/// - Vec of [Constraint]s for each column
/// - total width sum
pub(crate) fn determine_widths(
    data: &[Vec<String>],
    header_widths: Option<&Vec<usize>>,
) -> (Vec<Constraint>, usize) {
    // Start with header widths as minimums if provided
    let mut widths: Vec<usize> = match header_widths {
        None => vec![0; data.iter().map(|row| row.len()).max().unwrap_or(1)],
        Some(hdrs) => hdrs.clone(),
    };

    for row in data {
        for (i, item) in row.iter().enumerate() {
            widths[i] = widths[i].max(item.len());
        }
    }

    let mut sum_widths: usize = 0;
    let constraints: Vec<Constraint> = widths
        .iter()
        .map(|w| {
            sum_widths += *w;
            Constraint::Length(*w as u16)
        })
        .collect();

    (constraints, sum_widths)
}
