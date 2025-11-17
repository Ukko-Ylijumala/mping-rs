// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{collections::VecDeque, fmt::Display, net::IpAddr};
use tokio::sync::Mutex;

#[derive(Debug)]
pub(crate) enum PingStatus {
    Ok,
    Timeout,
    Error,
    //Laggy,
    None,
}

impl Display for PingStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PingStatus::Ok => write!(f, "OK"),
            PingStatus::Timeout => write!(f, "timeout"),
            PingStatus::Error => write!(f, "err"),
            //PingStatus::Laggy => write!(f, "laggy"),
            PingStatus::None => write!(f, "-"),
        }
    }
}

#[derive(Debug)]
pub(crate) struct PingTargetInner {
    pub sent: u64,
    pub recv: u64,
    pub rtts: VecDeque<u32>, // RTTs in microseconds
    pub status: PingStatus,
}

#[derive(Debug)]
pub(crate) struct PingTarget {
    pub addr: IpAddr,
    pub data: Mutex<PingTargetInner>,
}
