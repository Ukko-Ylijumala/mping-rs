// Copyright (c) 2025-2026 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

#![allow(dead_code)]

mod args;
mod hopcount;
mod input;
pub mod latencywin;
mod logging;
mod macros;
mod pingdata;
mod strings;
mod structs;
mod tui;
mod utils;

pub use hopcount::determine_hops;
pub use utils::parse_float_into_duration;
pub use utils::parse_ip_addresses;
