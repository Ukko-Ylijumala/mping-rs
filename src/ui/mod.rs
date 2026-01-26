// Copyright (c) 2025-2026 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

//! User interface module containing TUI components and input handling.

pub(crate) mod input;
pub(crate) mod keyboard;
pub(crate) mod tui;

pub(crate) use input::{AddTargetDialogState, AddTgtDialog, DialogAction};
pub(crate) use tui::{PopupContents, TuiState, eprintln_safe};

pub use tui::TerminalGuard;

use crate::pingdata::{PingTarget, StatsSnapshot};

pub trait Presenter {
    type Row;
    fn format_target(&self, target: &PingTarget, debug: bool) -> Self::Row;
    fn format_stats(&self, snap: &StatsSnapshot) -> Self::Row;
}
