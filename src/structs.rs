// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{collections::VecDeque, fmt::Display, net::IpAddr};
use surge_ping::SurgeError;
use tokio::sync::Mutex;

#[derive(Debug)]
pub(crate) enum PingStatus {
    Ok,
    Timeout,
    Error(SurgeError),
    //Laggy,
    None,
}

impl Display for PingStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PingStatus::Ok => write!(f, "OK"),
            PingStatus::Timeout => write!(f, "timeout"),
            PingStatus::Error(e) => write!(f, "{e}"),
            //PingStatus::Laggy => write!(f, "laggy"),
            PingStatus::None => write!(f, "-"),
        }
    }
}

#[derive(Debug)]
pub(crate) struct PingTargetInner {
    pub sent: u64,
    pub recv: u64,
    pub rtts: StatsWindow, // RTTs in microseconds (rolling window)
    pub status: PingStatus,
}

#[derive(Debug)]
pub(crate) struct PingTarget {
    pub addr: IpAddr,
    pub data: Mutex<PingTargetInner>,
}

/// O(1) amortized rolling statistics window over the last N samples.
#[derive(Debug)]
pub(crate) struct StatsWindow {
    cap: usize,
    buf: Vec<u32>,                  // ring buffer of µs
    head: usize,                    // next write position
    len: usize,
    sum: f64,                       // running sum in µs as f64
    minq: VecDeque<(u32, usize)>,   // monotonic increasing (value, index)
    maxq: VecDeque<(u32, usize)>,   // monotonic decreasing (value, index)
    index: usize,                   // monotonically increasing sample index
}

impl StatsWindow {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            buf: vec![0; cap],
            head: 0,
            len: 0,
            sum: 0.0,
            minq: VecDeque::new(),
            maxq: VecDeque::new(),
            index: 0,
        }
    }

    /// Push a new µs value, evicting oldest if full.
    pub fn push(&mut self, val: u32) {
        let idx: usize = self.index;
        self.index = self.index.wrapping_add(1);

        if self.len < self.cap {
            // Growing
            self.buf[self.head] = val;
            self.head = (self.head + 1) % self.cap;
            self.len += 1;
            self.sum += val as f64;
        } else {
            // Evict oldest at head
            let tail_pos: usize = self.head;
            let old: u32 = self.buf[tail_pos] as u32;
            self.buf[tail_pos] = val;
            self.head = (self.head + 1) % self.cap;
            self.sum += val as f64 - old as f64;

            // The global “logical index” of the evicted element is idx - cap,
            // but we only track indices of pushed elements in queues;
            // we’ll drop out-of-range by age below.
        }

        // Update min deque (pop larger tails)
        while let Some(&(v, _)) = self.minq.back() {
            if v > val {
                self.minq.pop_back();
            } else {
                break;
            }
        }
        self.minq.push_back((val, idx));

        // Update max deque (pop smaller tails)
        while let Some(&(v, _)) = self.maxq.back() {
            if v < val {
                self.maxq.pop_back();
            } else {
                break;
            }
        }
        self.maxq.push_back((val, idx));

        // Drop aged-out heads
        let cutoff: usize = idx.saturating_sub(self.cap);
        while let Some(&(_, i)) = self.minq.front() {
            if i <= cutoff {
                self.minq.pop_front();
            } else {
                break;
            }
        }
        while let Some(&(_, i)) = self.maxq.front() {
            if i <= cutoff {
                self.maxq.pop_front();
            } else {
                break;
            }
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Latest sample in ms (string-ready with 2 decimals).
    pub fn latest_ms(&self) -> Option<f64> {
        if self.is_empty() {
            return None;
        }
        let last_idx = (self.head + self.cap - 1) % self.cap;
        Some(self.buf[last_idx] as f64 / 1e3)
    }

    /// Mean/min/max in ms.
    pub fn mean_min_max_ms(&self) -> Option<(f64, f64, f64)> {
        if self.is_empty() {
            return None;
        }
        let mean: f64 = (self.sum / self.len as f64) / 1e3;
        let min: f64 = self.minq.front().map(|(v, _)| *v as f64 / 1e3).unwrap_or_default();
        let max: f64 = self.maxq.front().map(|(v, _)| *v as f64 / 1e3).unwrap_or_default();
        Some((mean, min, max))
    }
}
