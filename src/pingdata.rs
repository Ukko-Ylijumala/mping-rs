// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{latencywin::LatencyWindow, strings::*};
use itertools::Itertools;
use parking_lot::RwLock;
use std::{
    collections::VecDeque,
    fmt::Display,
    net::IpAddr,
    ops::Index,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use surge_ping::{IcmpPacket, SurgeError};
use tokio_util::sync::CancellationToken;

const MICROS_PER_MILLI: f64 = 1e3;
const DEFAULT_WIN: usize = 10; // Window size (N packets) for recent history analysis
const FLAP_THRESH: usize = 4; // Number of up/down transitions to consider "flappy"
const LOSSY_THRESH: f64 = 0.5; // Packet loss % to consider "lossy"
const LAGGY_FACTOR: f64 = 2.0; // Multiplier over historical mean to consider "laggy"

#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) enum PingStatus {
    Ok,
    Timeout,
    NotReachable,
    Error(String),
    Laggy,
    Lossy,
    Flappy,
    Paused,
    Resuming,
    Stopped,
    #[default]
    None,
}

impl Display for PingStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PingStatus::Ok => write!(f, "OK"),
            PingStatus::Timeout => write!(f, "timeout"),
            PingStatus::NotReachable => write!(f, "unreach"),
            PingStatus::Error(_) => write!(f, "error"),
            PingStatus::Laggy => write!(f, "laggy"),
            PingStatus::Lossy => write!(f, "lossy"),
            PingStatus::Flappy => write!(f, "flapping"),
            PingStatus::Paused => write!(f, "paused"),
            PingStatus::Resuming => write!(f, "resuming"),
            PingStatus::Stopped => write!(f, "stopped"),
            PingStatus::None => write!(f, "{MISSING}"),
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
    /// Raw last known status from pinging. Can only be one of:
    /// - [PingStatus::Ok]
    /// - [PingStatus::Timeout]
    /// - [PingStatus::Error]
    ///
    /// For derived statuses like [PingStatus::Laggy], [PingStatus::Lossy],
    /// etc, use [PingTarget::effective_status] instead.
    raw_status: PingStatus,
    /// Authoritative last sent sequence number
    pub last_seq: u16,
    /// Authoritative last sent timestamp. Will be slightly before actual send time. The
    /// difference can be calculated from [PacketRecord] (with the same sequence number).
    pub last_sent: Option<Instant>,
}

impl PingTargetInner {
    /// Whether recent packet loss of las N packets exceeds the specified % threshold.
    #[inline]
    pub fn is_lossy(&self, n: usize, threshold: f64) -> bool {
        self.recent.recent_losses(n) as f64 / n as f64 >= threshold
    }

    /// Whether last N results show at least `threshold` transitions between up/down.
    #[inline]
    pub fn is_flappy(&self, n: usize, threshold: usize) -> bool {
        self.recent.recent_transitions(n) >= threshold
    }

    /// Whether recent N RTTs are significantly above historical mean.
    /// `factor` is a multiplier (e.g. `2.0` means "twice as high").
    #[inline]
    pub fn is_laggy(&self, n: usize, factor: f64) -> Result<bool, String> {
        let long_mean: f64 = self.rtts.mean().unwrap_or(0.0);
        let recent_mean: Duration = self.recent.mean(Some(n))?;
        Ok(recent_mean.as_micros() as f64 > long_mean * factor)
    }

    /// Whether this target is (currently) considered unreachable. Logic:
    /// - If [DEFAULT_WIN] packets have been sent and none received -> unreachable
    /// - If last [DEFAULT_WIN] * 5 packets were all lost -> unreachable
    /// - Errors are NOT considered as unreachable.
    #[inline]
    pub fn is_unreachable(&self) -> bool {
        if matches!(self.raw_status, PingStatus::Error(_)) {
            return false;
        }
        if self.sent as usize > DEFAULT_WIN && self.recv == 0 {
            return true;
        } else if self.recent.len() > DEFAULT_WIN {
            let num: usize = (DEFAULT_WIN * 5).min(self.recent.len());
            if self.recent.recent_losses(num) == num {
                return true;
            }
        }
        false
    }

    /// Determine the effective status of this target based on recent history analysis.
    /// Does NOT return "paused" or "stopped" states (as that requires access to parent).
    #[inline]
    pub fn effective_status(&self) -> PingStatus {
        if &self.raw_status == &PingStatus::Timeout && self.is_unreachable() {
            return PingStatus::NotReachable;
        }

        match &self.raw_status {
            PingStatus::Ok | PingStatus::Timeout => {
                if self.is_flappy(DEFAULT_WIN, FLAP_THRESH) {
                    PingStatus::Flappy
                } else if self.is_lossy(DEFAULT_WIN, LOSSY_THRESH) {
                    PingStatus::Lossy
                } else if self.is_laggy(DEFAULT_WIN, LAGGY_FACTOR).unwrap_or(false) {
                    PingStatus::Laggy
                } else {
                    self.raw_status.clone()
                }
            }
            _ => self.raw_status.clone(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct PingTarget {
    pub addr: IpAddr,
    pub data: RwLock<PingTargetInner>,
    paused: AtomicBool,
    cancel: CancellationToken,
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
            paused: AtomicBool::new(false),
            cancel: CancellationToken::new(),
        }
    }

    /// Update statistics based on the result of a ping attempt and the associated packet record.
    pub async fn update_stats(
        &self,
        res: Result<(IcmpPacket, Duration), SurgeError>,
        mut rec: PacketRecord,
    ) {
        let mut inner = self.data.write();
        inner.raw_status = match res {
            Ok((_, dur)) => {
                inner.recv += 1;
                inner.rtts.push(dur.as_micros() as u32);
                rec.set_rtt(dur);
                PingStatus::Ok
            }
            Err(e) => match e {
                SurgeError::Timeout { .. } => PingStatus::Timeout,
                _ => {
                    inner.sent -= 1; // don't count errors, as the packet was never sent
                    PingStatus::Error(e.to_string())
                }
            },
        };
        inner.recent.push(rec);
    }

    /// Reset all statistics for this target as if it was never pinged.
    pub fn reset_stats(&self) {
        let mut data = self.data.write();
        data.sent = 0;
        data.recv = 0;
        data.rtts.clear();
        data.recent.clear();
        data.raw_status = PingStatus::None;
        data.last_seq = 0;
        data.last_sent = None;
    }

    /// Whether pinging currently paused for this target is.
    #[inline]
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Pause pinging unconditionally.
    pub fn pause(&self) {
        if !self.is_stopped() {
            self.data.write().raw_status = PingStatus::Paused;
            self.paused.store(true, Ordering::Relaxed);
        }
    }

    /// Resume pinging unconditionally.
    pub fn resume(&self) {
        if !self.is_stopped() {
            self.data.write().raw_status = PingStatus::Resuming;
            self.paused.store(false, Ordering::Relaxed);
        }
    }

    /// Toggle paused state for this target.
    pub fn toggle_pause(&self) {
        if !self.is_stopped() {
            let was_paused: bool = self.paused.fetch_xor(true, Ordering::Relaxed);
            if !was_paused {
                self.data.write().raw_status = PingStatus::Paused;
            }
        }
    }

    /// Whether pinging this target has been stopped.
    #[inline]
    pub fn is_stopped(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Whether pinging this target has been stopped.
    /// Async version for tokio::select! to `await` on it.
    pub async fn is_stopped_async(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Permanently stop pinging this target. Ping task will abort.
    pub fn stop(&self) {
        self.cancel.cancel();
        self.data.write().raw_status = PingStatus::Stopped;
    }

    /// Whether this target is (currently) considered unreachable.
    #[inline]
    pub fn is_unreachable(&self) -> bool {
        self.data.read().is_unreachable()
    }

    /// Whether recent packet loss is above the default threshold.
    pub fn is_lossy(&self) -> bool {
        self.data.read().is_lossy(DEFAULT_WIN, LOSSY_THRESH)
    }

    /// Whether recent packet history shows flappiness (frequent up/down transitions)
    pub fn is_flappy(&self) -> bool {
        self.data.read().is_flappy(DEFAULT_WIN, FLAP_THRESH)
    }

    /// Whether recent RTTs are significantly above historical mean.
    pub fn is_laggy(&self) -> bool {
        match self.data.read().is_laggy(DEFAULT_WIN, LAGGY_FACTOR) {
            Ok(v) => v,
            Err(_) => false,
        }
    }

    /// Determine the effective status of this target, considering pauses,
    /// stops, and recent history analysis. Can return all states.
    pub fn effective_status(&self) -> PingStatus {
        if self.is_stopped() {
            return PingStatus::Stopped;
        }
        if self.is_paused() {
            return PingStatus::Paused;
        }
        self.data.read().effective_status()
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
            None => Err(ERR_NO_RESP.into()),
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
    #[inline]
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
            return Err(ERR_NOTENOUGH.into());
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

    #[inline]
    fn no_records_check(&self) -> Result<(), String> {
        if self.is_empty() {
            return Err(ERR_NORECORDS.into());
        }
        Ok(())
    }

    /// Determine the minimum RTT in the history.
    #[inline]
    pub fn min(&self) -> Result<Duration, String> {
        self.no_records_check()?;

        match self
            .iter()
            .filter_map(|rec: &PacketRecord| rec.rtt().ok())
            .min()
        {
            Some(v) => Ok(v),
            None => Err(ERR_NO_RTT.into()),
        }
    }

    /// Determine the maximum RTT in the history.
    #[inline]
    pub fn max(&self) -> Result<Duration, String> {
        self.no_records_check()?;

        match self
            .iter()
            .filter_map(|rec: &PacketRecord| rec.rtt().ok())
            .max()
        {
            Some(v) => Ok(v),
            None => Err(ERR_NO_RTT.into()),
        }
    }

    /// Calculate the mean (average) RTT in the history (or given N-sized window).
    pub fn mean(&self, n: Option<usize>) -> Result<Duration, String> {
        self.no_records_check()?;

        let skip: usize = match n {
            None => 0,
            Some(ws) if ws <= self.len() => self.len() - ws,
            Some(_) => return Err(ERR_LARGE_WIN.into()),
        };

        let (sum, count) = self
            .iter()
            .skip(skip)
            .filter_map(|rec| rec.rtt().ok())
            .fold((Duration::ZERO, 0u32), |(s, c), rtt| (s + rtt, c + 1));

        if count == 0 {
            return Err(ERR_NOTENOUGH.into());
        }

        Ok(sum / count)
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
    /// Starting sequence number from historical data
    pub start_seq: u16,
    /// Ending sequence number from historical data
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
        let gaps_in_seqs: bool = {
            let mut expected_seq: Option<u16> = None;
            let mut gaps: bool = false;
            for rec in data.iter().rev().take(DEFAULT_WIN) {
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
            let delta: u16 = last.wrapping_sub(second_last);
            // If delta > 32768, it wrapped the "wrong way" -> out of order.
            // I'm told this is the "standard" way to check for u16 wraparound
            // in sequence numbers, so let's go with it.
            delta > 32768
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
            recent_losses: data.recent_losses(DEFAULT_WIN),
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
    /// Current effective status of the target. DOES NOT contain "paused" or "stopped" states.
    pub status: PingStatus,
    /// History of recent sent/received packets
    pub hist: HistorySnapshot,
    /// Timestamp of this snapshot.
    pub when: Instant,
    /// The latest sequence number from master data AT THE TIME OF THIS SNAPSHOT
    pub latest_seq: u16,
    /// The instant when the latest packet (latest_seq) was sent
    pub latest_sent: Instant,
    timeout: Duration,
}

impl StatsSnapshot {
    /// Extract a [StatsSnapshot] from [PingTargetInner]
    ///
    /// - `timeout` is the overall ping timeout duration.
    pub fn new_from(tgt: &Arc<PingTarget>, timeout: Duration) -> Self {
        let data = tgt.data.read();
        let now: Instant = Instant::now();
        let (mean, min, max) = match data.rtts.mean_min_max() {
            Ok((mean, mi, ma)) => (Some(mean), Some(mi), Some(ma)),
            Err(_) => (None, None, None),
        };
        Self {
            when: now,
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
            status: data.effective_status(),
            hist: HistorySnapshot::new_from(&data.recent),
            latest_seq: data.last_seq,
            latest_sent: data.last_sent.unwrap_or(now),
            timeout,
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

    /// Whether the latest sent packet is still considered "in flight" (not yet timed out).
    ///
    /// NOTE: This is based on this snapshot's creation timestamp (`now`), not the current
    /// time, so it may be slightly out of date. Sufficient for display purposes.
    #[inline]
    fn is_latest_inflight(&self) -> bool {
        self.timeout > self.when.duration_since(self.latest_sent)
    }

    /// Packet loss as formatted string.
    #[inline]
    pub fn loss_str(&self) -> String {
        if self.sent == 0 {
            MISSING.to_string()
        } else if ((self.sent - self.recv) == 1) && self.is_latest_inflight() {
            // catch the common case of one receive missing (still in transit)
            "0.0%".to_string()
        } else {
            format!("{:.1}%", 1e2 * self.loss())
        }
    }

    /// Minimum RTT as formatted string (as milliseconds).
    #[inline]
    pub fn min_str(&self) -> String {
        match self.min {
            Some(v) => format!("{:.2}", v as f64 / MICROS_PER_MILLI),
            None => MISSING.to_string(),
        }
    }

    /// Maximum RTT as formatted string (as milliseconds).
    #[inline]
    pub fn max_str(&self) -> String {
        match self.max {
            Some(v) => format!("{:.2}", v as f64 / MICROS_PER_MILLI),
            None => MISSING.to_string(),
        }
    }

    /// Last RTT as formatted string (as milliseconds).
    #[inline]
    pub fn last_str(&self) -> String {
        match self.last {
            Some(v) => format!("{:.2}", v as f64 / MICROS_PER_MILLI),
            None => MISSING.to_string(),
        }
    }

    /// Mean RTT as formatted string (as milliseconds).
    #[inline]
    pub fn mean_str(&self) -> String {
        match self.mean {
            Some(v) => format!("{:.2}", v / MICROS_PER_MILLI),
            None => MISSING.to_string(),
        }
    }

    /// Standard deviation as formatted string (as milliseconds).
    #[inline]
    pub fn stdev_str(&self) -> String {
        match self.stdev {
            Some(v) => format!("{:.2}", v / MICROS_PER_MILLI),
            None => MISSING.to_string(),
        }
    }
}
