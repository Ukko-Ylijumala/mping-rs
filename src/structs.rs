// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{args::MpConfig, latencywin::LatencyWindow, tui::AppLayout};
use itertools::Itertools;
use miniutils::ProcessInfo;
use parking_lot::RwLock;
use std::{
    collections::VecDeque,
    fmt::Display,
    net::IpAddr,
    ops::Index,
    sync::Arc,
    time::{Duration, Instant},
};
use surge_ping::{Client, SurgeError};

const MICRO_TO_MILLI: f64 = 1e3;
const DEFAULT_REFRESH: Duration = Duration::from_millis(250);

/// Main application state structure.
pub(crate) struct AppState<'a> {
    pub pi: miniutils::ProcessInfo,
    pub c_v4: Option<Arc<Client>>,
    pub c_v6: Option<Arc<Client>>,
    pub targets: Vec<Arc<PingTarget>>,
    pub tasks: Vec<tokio::task::JoinHandle<()>>,
    pub layout: RwLock<AppLayout>,
    pub title: Option<ratatui::widgets::Paragraph<'a>>,
    /// Table headers
    pub tbl_hdrs: Vec<&'static str>,
    /// Precomputed visible widths of table headers
    pub tbl_hdr_width: Vec<usize>,
    /// Spacing between table columns
    pub tbl_colsp: u16,
    /// UI refresh interval
    pub ui_interval: Duration,
    /// Next scheduled UI refresh time
    pub ui_next_refresh: tokio::time::Instant,
    pub verbose: bool,
    pub debug: bool,
}

impl AppState<'_> {
    /// Do any final calculations needed after initialization.
    pub fn build(mut self, conf: &Arc<MpConfig>) -> Self {
        self.debug = conf.debug;
        self.verbose = conf.verbose;
        if self.ui_interval != DEFAULT_REFRESH {
            self.ui_interval = Duration::from_millis(conf.refresh);
        }

        if self.debug {
            self.tbl_hdrs.push("Seq");
        }
        self.tbl_hdr_width = self.tbl_hdrs.iter().map(|h| h.len()).collect();

        self
    }
}

impl Default for AppState<'_> {
    fn default() -> Self {
        Self {
            pi: ProcessInfo::new(),
            c_v4: None,
            c_v6: None,
            targets: vec![],
            tasks: vec![],
            layout: AppLayout::default().into(),
            title: None,
            tbl_hdrs: vec![
                "Address", "Sent", "Recv", "Loss", "Last", "Mean", "Min", "Max", "Stdev", "Status",
            ],
            tbl_hdr_width: vec![],
            tbl_colsp: 2,
            ui_interval: DEFAULT_REFRESH,
            ui_next_refresh: tokio::time::Instant::now(),
            verbose: false,
            debug: false,
        }
    }
}

////////////////////////////////////////////////////////////////////////////////

#[derive(Debug, Default)]
pub(crate) enum PingStatus {
    Ok,
    Timeout,
    NotReachable,
    Error(SurgeError),
    Laggy,
    Lossy,
    Flappy,
    #[default]
    None,
}

impl Display for PingStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PingStatus::Ok => write!(f, "OK"),
            PingStatus::Timeout => write!(f, "timeout"),
            PingStatus::NotReachable => write!(f, "unreachable"),
            PingStatus::Error(_) => write!(f, "error"),
            PingStatus::Laggy => write!(f, "laggy"),
            PingStatus::Lossy => write!(f, "lossy"),
            PingStatus::Flappy => write!(f, "flapping"),
            PingStatus::None => write!(f, "-"),
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct PingTargetInner {
    pub sent: u64,
    pub recv: u64,
    pub rtts: LatencyWindow, // RTTs in microseconds (rolling window)
    /// Detailed history of recent sent/received packets
    pub recent: PacketHistory,
    pub status: PingStatus,
}

impl PingTargetInner {
    pub fn is_lossy(&self, n: usize, threshold: f64) -> bool {
        self.recent.recent_losses(n) as f64 / n as f64 >= threshold
    }

    pub fn is_flappy(&self, n: usize, threshold: usize) -> bool {
        self.recent.recent_transitions(n) >= threshold
    }

    pub fn is_laggy(&self, n: usize, threshold: f64) -> Result<bool, String> {
        let long_mean: f64 = self.rtts.mean().map(|m: f64| m).unwrap_or(0.0);
        let recent_mean: Duration = self.recent.mean(Some(n))?;
        Ok(recent_mean.as_micros() as f64 > long_mean * threshold)
    }
}

#[derive(Debug)]
pub(crate) struct PingTarget {
    pub addr: IpAddr,
    pub data: RwLock<PingTargetInner>,
}

impl PingTarget {
    /// Create a new [PingTarget] for the specified IP address.
    ///
    /// - `histsize` specifies the size of the full RTT latency window.
    /// - `detailed` specifies the number of more detailed recent packet stats to keep.
    pub fn new(addr: IpAddr, histsize: usize, detailed: usize) -> Self {
        Self {
            addr,
            data: PingTargetInner {
                rtts: LatencyWindow::new(histsize),
                recent: PacketHistory::new(detailed),
                ..Default::default()
            }
            .into(),
        }
    }

    /// Whether recent packet loss of las N packets exceeds the specified threshold.
    pub fn is_lossy(&self, n: usize, threshold: f64) -> bool {
        self.data.read().is_lossy(n, threshold)
    }

    /// Whether recent packet history shows flappiness (frequent up/down transitions)
    pub fn is_flappy(&self, n: usize, threshold: usize) -> bool {
        self.data.read().is_flappy(n, threshold)
    }

    /// Whether recent N RTTs are significantly above historical mean.
    pub fn is_laggy(&self, n: usize, threshold_factor: f64) -> bool {
        match self.data.read().is_laggy(n, threshold_factor) {
            Ok(v) => v,
            Err(_) => false,
        }
    }
}

////////////////////////////////////////////////////////////////////////////////

/// Record for a single sent/received packet.
#[derive(Debug, Clone)]
pub(crate) struct PacketRecord {
    pub seq: u16,
    pub sent: Instant,
    rtt: Option<Duration>,
}

impl PacketRecord {
    /// Create a new [PacketRecord] for a sent packet with the current timestamp.
    /// Receive timestamp must be set later when/if a response is received.
    pub fn new(seq: u16) -> Self {
        Self {
            seq,
            ..Default::default()
        }
    }

    /// Set [PacketRecord]'s RTT if it's already known. Intended for chaining with `new()`.
    pub fn with_rtt(mut self, rtt: Duration) -> Self {
        self.rtt = Some(rtt);
        self
    }

    /// Mark as "response received" as of the current timestamp.
    pub fn mark_received(&mut self) {
        self.rtt = Some(Instant::now().duration_since(self.sent));
    }

    /// Whether a response has been recorded for this packet.
    #[inline]
    pub fn has_response(&self) -> bool {
        self.rtt.is_some()
    }

    /// Return RTT if a response has been received.
    #[inline]
    pub fn rtt(&self) -> Result<Duration, String> {
        match self.rtt {
            Some(rtt) => Ok(rtt),
            None => Err("No response".to_string()),
        }
    }

    /// Set the RTT if known and/or `mark_received()` would introduce too much drift.
    pub fn set_rtt(&mut self, rtt: Duration) {
        self.rtt = Some(rtt);
    }
}

impl Default for PacketRecord {
    fn default() -> Self {
        Self {
            seq: 0,
            sent: Instant::now(),
            rtt: None,
        }
    }
}

/* ---------------------------------------- */

/// Recent history of sent/received packets for a ping target.
#[derive(Debug, Default, Clone)]
pub(crate) struct PacketHistory {
    capacity: usize,
    records: VecDeque<PacketRecord>,
}

impl PacketHistory {
    /// Create a new [PacketHistory] with the specified capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            records: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Add a new [PacketRecord] to the history, evicting the oldest if at capacity.
    pub fn push(&mut self, record: PacketRecord) {
        if self.records.len() == self.capacity {
            self.records.pop_front();
        }
        self.records.push_back(record);
    }

    /// Get the number of records in the history.
    #[inline]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Check if the history is empty.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Clear all records from the history.
    pub fn clear(&mut self) {
        self.records.clear();
    }

    /// Get the oldest [PacketRecord], if any.
    #[inline]
    pub fn first(&self) -> Option<&PacketRecord> {
        self.records.front()
    }

    /// Get the most recent [PacketRecord], if any.
    #[inline]
    pub fn last(&self) -> Option<&PacketRecord> {
        self.records.back()
    }

    /// Get an iterator over the records.
    #[inline]
    pub fn iter(&self) -> std::collections::vec_deque::Iter<'_, PacketRecord> {
        self.records.iter()
    }

    /// Calculate the total timespan covered by the history.
    pub fn timespan(&self) -> Result<Duration, String> {
        if self.len() < 2 {
            return Err("Not enough records to calculate timespan".to_string());
        }
        let (first, last) = (self.first().unwrap().sent, self.last().unwrap().sent);
        Ok(last.duration_since(first))
    }

    /// Calculate packet loss as a float `[0.0 .. 1.0]`.
    pub fn loss(&self) -> f64 {
        if self.records.is_empty() {
            return 0.0;
        }

        let sent: f64 = self.records.len() as f64;
        let recv: f64 = self
            .iter()
            .filter(|rec: &&PacketRecord| rec.has_response())
            .count() as f64;
        (sent - recv) / sent
    }

    /// Count packets without response in the last N records
    pub fn recent_losses(&self, n: usize) -> usize {
        self.iter()
            .rev()
            .take(n)
            .filter(|r: &&PacketRecord| !r.has_response())
            .count()
    }

    /// Transitions between "responding" and "not responding" in last N records
    pub fn recent_transitions(&self, n: usize) -> usize {
        self.iter()
            .rev()
            .take(n)
            .map(|r| r.has_response())
            .tuple_windows()
            .filter(|(a, b)| a != b)
            .count()
    }

    /// Determine the minimum RTT in the history.
    pub fn min(&self) -> Result<Duration, String> {
        if self.is_empty() {
            return Err("No records".to_string());
        }

        match self
            .iter()
            .filter_map(|rec: &PacketRecord| rec.rtt().ok())
            .min()
        {
            Some(v) => Ok(v),
            None => Err("Could not find min RTT".to_string()),
        }
    }

    /// Determine the maximum RTT in the history.
    pub fn max(&self) -> Result<Duration, String> {
        if self.is_empty() {
            return Err("No records".to_string());
        }

        match self
            .iter()
            .filter_map(|rec: &PacketRecord| rec.rtt().ok())
            .max()
        {
            Some(v) => Ok(v),
            None => Err("Could not find max RTT".to_string()),
        }
    }

    /// Calculate the mean (average) RTT in the history (or given N-sized window).
    pub fn mean(&self, n: Option<usize>) -> Result<Duration, String> {
        if self.is_empty() {
            return Err("No records to calculate mean RTT".to_string());
        }
        if n.is_some_and(|s: usize| s > self.len()) {
            return Err("Window size exceeds history length".to_string());
        }

        let rtts: Vec<Duration> = match n {
            None => self
                .iter()
                .filter_map(|rec: &PacketRecord| rec.rtt().ok())
                .collect(),
            Some(ws) => self
                .iter()
                .rev()
                .take(ws)
                .filter_map(|rec: &PacketRecord| rec.rtt().ok())
                .collect(),
        };

        if rtts.is_empty() {
            return Err("No valid RTTs to calculate mean".to_string());
        }

        Ok(rtts.iter().sum::<Duration>() / rtts.len() as u32)
    }
}

/* ---------------------------------------- */

// Implement conversions, iterators and indexing for PacketHistory
impl From<PacketHistory> for Vec<PacketRecord> {
    fn from(ph: PacketHistory) -> Vec<PacketRecord> {
        ph.into_iter().collect()
    }
}

impl<'a> From<&'a PacketHistory> for Vec<&'a PacketRecord> {
    fn from(ph: &'a PacketHistory) -> Vec<&'a PacketRecord> {
        ph.iter().collect()
    }
}

impl IntoIterator for PacketHistory {
    type Item = PacketRecord;
    type IntoIter = std::collections::vec_deque::IntoIter<PacketRecord>;

    fn into_iter(self) -> Self::IntoIter {
        self.records.into_iter()
    }
}

impl<'a> IntoIterator for &'a PacketHistory {
    type Item = &'a PacketRecord;
    type IntoIter = std::collections::vec_deque::Iter<'a, PacketRecord>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl Index<usize> for PacketHistory {
    type Output = PacketRecord;

    fn index(&self, index: usize) -> &Self::Output {
        &self.records[index]
    }
}

////////////////////////////////////////////////////////////////////////////////

/// Snapshot of recent detailed packet history statistics.
#[derive(Debug)]
pub(crate) struct HistorySnapshot {
    pub start_seq: u16,
    pub end_seq: u16,
    pub gaps_in_seqs: bool,
    pub last_out_of_order: bool,
    pub recent_losses: usize,
    pub loss_pct: f64,
    pub min: Option<Duration>,
    pub max: Option<Duration>,
    pub mean: Option<Duration>,
}

impl HistorySnapshot {
    /// Extract recent history statistics from [PacketHistory].
    fn new_from(data: &PacketHistory) -> Self {
        let inspect_win: usize = 10;

        let gaps_in_seqs: bool = {
            let mut expected_seq: Option<u16> = None;
            let mut gaps: bool = false;
            for rec in data.iter().rev().take(inspect_win) {
                if let Some(exp) = expected_seq {
                    if rec.seq.wrapping_add(1) != exp {
                        gaps = true;
                        break;
                    }
                }
                expected_seq = Some(rec.seq);
            }
            gaps
        };

        let last_out_of_order: bool = if data.len() >= 2 {
            let last: u16 = data.last().unwrap().seq;
            let second_last: u16 = data.iter().rev().nth(1).unwrap().seq;
            last < second_last
        } else {
            false
        };

        Self {
            start_seq: match data.first() {
                Some(pr) => pr.seq,
                None => 0,
            },
            end_seq: match data.last() {
                Some(pr) => pr.seq,
                None => 0,
            },

            gaps_in_seqs,
            last_out_of_order,
            recent_losses: data.recent_losses(inspect_win),
            loss_pct: data.loss(),

            min: match data.min() {
                Ok(v) => Some(v),
                Err(_) => None,
            },
            max: match data.max() {
                Ok(v) => Some(v),
                Err(_) => None,
            },
            mean: match data.mean(None) {
                Ok(v) => Some(v),
                Err(_) => None,
            },
        }
    }
}

////////////////////////////////////////////////////////////////////////////////

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
    /// History of recent sent/received packets
    pub hist: HistorySnapshot,
    pub when: Instant,
}

impl StatsSnapshot {
    /// Extract a [StatsSnapshot] from [PingTargetInner]
    pub fn new_from(data: &PingTargetInner) -> Self {
        let (mean, min, max) = match data.rtts.mean_min_max() {
            Ok((mean, mi, ma)) => (Some(mean), Some(mi), Some(ma)),
            Err(_) => (None, None, None),
        };
        Self {
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
            hist: HistorySnapshot::new_from(&data.recent),
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
