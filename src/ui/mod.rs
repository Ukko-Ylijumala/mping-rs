// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

//! User interface module containing TUI components and input handling.

mod input;
pub(crate) mod keyboard;
mod tui;

pub(crate) use input::AddTargetDialogState;
pub(crate) use tui::{AppLayout, MutableLine, PopupContents, TableRow, eprintln_safe};

pub use tui::TerminalGuard;
