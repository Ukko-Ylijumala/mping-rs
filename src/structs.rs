// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::latencywin::LatencyWindow;
use std::{fmt::Display, net::IpAddr, time::Instant};
use surge_ping::SurgeError;
use tokio::sync::Mutex;

const MICRO_TO_MILLI: f64 = 1e3;

#[derive(Debug)]
pub(crate) enum PingStatus {
    Ok,
    Timeout,
    NotReachable,
    Error(SurgeError),
    //Laggy,
    None,
}

impl Display for PingStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PingStatus::Ok => write!(f, "OK"),
            PingStatus::Timeout => write!(f, "timeout"),
            PingStatus::NotReachable => write!(f, "unreachable"),
            PingStatus::Error(_) => write!(f, "error"),
            //PingStatus::Laggy => write!(f, "laggy"),
            PingStatus::None => write!(f, "-"),
        }
    }
}

#[derive(Debug)]
pub(crate) struct PingTargetInner {
    pub sent: u64,
    pub recv: u64,
    pub rtts: LatencyWindow, // RTTs in microseconds (rolling window)
    pub status: PingStatus,
}

#[derive(Debug)]
pub(crate) struct PingTarget {
    pub addr: IpAddr,
    pub data: Mutex<PingTargetInner>,
}

/// Snapshot of ping statistics at a point in time.
///
/// Includes stringifying methods for display purposes.
/// Unless otherwise noted, RTT values are stored as
/// microseconds and displayed as milliseconds.
#[derive(Debug)]
pub(crate) struct StatsSnapshot {
    pub sent: u64,
    pub recv: u64,
    pub min: Option<u32>,
    pub max: Option<u32>,
    pub mean: Option<f64>,
    pub last: Option<u32>,
    pub stdev: Option<f64>,
    pub error: Option<String>,
    pub when: Instant,
}

impl StatsSnapshot {
    /// Extract a [StatsSnapshot] from [PingTargetInner]
    pub fn new_from(data: &PingTargetInner) -> Self {
        let (mean, min, max) = match data.rtts.mean_min_max() {
            Ok((mean, mi, ma)) => (Some(mean), Some(mi), Some(ma)),
            Err(_) => (None, None, None),
        };
        StatsSnapshot {
            when: Instant::now(),
            sent: data.sent,
            recv: data.recv,
            mean,
            min,
            max,
            last: match data.rtts.last() {
                Ok(v) => Some(v),
                Err(_) => None,
            },
            stdev: match data.rtts.stdev() {
                Ok(v) => Some(v),
                Err(_) => None,
            },
            error: match &data.status {
                PingStatus::Error(e) => Some(e.to_string()),
                _ => None,
            },
        }
    }

    /// Packet loss percentage as a float `[0.0 .. 1.0]`.
    pub fn loss(&self) -> f64 {
        if self.sent == 0 {
            0.0
        } else {
            (self.sent - self.recv) as f64 / self.sent as f64
        }
    }

    /// Packet loss as formatted string.
    pub fn loss_str(&self) -> String {
        if self.sent == 0 {
            "-".to_string()
        } else if (self.sent - self.recv) == 1 {
            // catch the common case of one receive missing (probably in transit)
            "0.0%".to_string()
        } else {
            format!("{:.1}%", 1e2 * self.loss())
        }
    }

    /// Minimum RTT as formatted string (as milliseconds).
    pub fn min_str(&self) -> String {
        match self.min {
            Some(v) => format!("{:.2}", v as f64 / MICRO_TO_MILLI),
            None => "-".to_string(),
        }
    }

    /// Maximum RTT as formatted string (as milliseconds).
    pub fn max_str(&self) -> String {
        match self.max {
            Some(v) => format!("{:.2}", v as f64 / MICRO_TO_MILLI),
            None => "-".to_string(),
        }
    }

    /// Last RTT as formatted string (as milliseconds).
    pub fn last_str(&self) -> String {
        match self.last {
            Some(v) => format!("{:.2}", v as f64 / MICRO_TO_MILLI),
            None => "-".to_string(),
        }
    }

    /// Mean RTT as formatted string (as milliseconds).
    pub fn mean_str(&self) -> String {
        match self.mean {
            Some(v) => format!("{:.2}", v / MICRO_TO_MILLI),
            None => "-".to_string(),
        }
    }

    /// Standard deviation as formatted string (as milliseconds).
    pub fn stdev_str(&self) -> String {
        match self.stdev {
            Some(v) => format!("{:.2}", v / MICRO_TO_MILLI),
            None => "-".to_string(),
        }
    }
}
