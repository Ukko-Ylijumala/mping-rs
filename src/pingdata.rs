// Copyright (c) 2025-2026 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{
    hopcount::determine_hops,
    latencywin::LatencyWindow,
    strings::*,
    structs::QueryResponse,
    utils::{HistogramBucket, make_histogram_buckets, reverse_name},
};
use hickory_resolver::{Resolver, name_server::TokioConnectionProvider};
use itertools::Itertools;
use parking_lot::RwLock;
use std::{
    collections::VecDeque,
    fmt,
    net::IpAddr,
    ops::Index,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
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
const SPEED_KM_S: f64 = 204e3; // Approx speed of light in fiber (204 000 km/s)
const STRETCH_FACTOR: f64 = 1.3; // Inflation factor to account for non-direct paths etc
const LATENCY_FLOOR: f64 = 2e-4; // 0.2 ms baseline latency floor (non-propagation)
const BAND_SIZE_KM: f64 = 100.0; // Quantize to nearest 100km

/// Global creation counter behind [PingTarget::added_order].
static ADD_ORDER: AtomicU64 = AtomicU64::new(0);

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

impl fmt::Display for PingStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PingStatus::Ok => write!(f, "OK"),
            PingStatus::Timeout => write!(f, "{TIMEOUT}"),
            PingStatus::NotReachable => write!(f, "{UNREACH}"),
            PingStatus::Error(_) => write!(f, "error"),
            PingStatus::Laggy => write!(f, "laggy"),
            PingStatus::Lossy => write!(f, "lossy"),
            PingStatus::Flappy => write!(f, "flapping"),
            PingStatus::Paused => write!(f, "{PAUSED}"),
            PingStatus::Resuming => write!(f, "{RESUMED}"),
            PingStatus::Stopped => write!(f, "{STOPPED}"),
            PingStatus::None => write!(f, "{MISSING}"),
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct PingTargetInner {
    pub sent: u64,
    pub recv: u64,
    /// RTTs in microseconds (rolling window)
    pub rtts: LatencyWindow,
    /// Detailed history of recent sent/received packets
    pub recent: PacketHistory,
    /**
    Raw last known status from pinging. Can only be one of:
    - [PingStatus::Ok]
    - [PingStatus::Timeout]
    - [PingStatus::Error]

    For derived statuses like [PingStatus::Laggy], [PingStatus::Lossy],
    etc, use [PingTarget::effective_status] instead.
    */
    raw_status: PingStatus,
    /// Authoritative last sent sequence number
    pub last_seq: u16,
    /// Authoritative last sent timestamp. Will be slightly before actual send time. The
    /// difference can be calculated from [PacketRecord] (with the same sequence number).
    pub last_sent: Option<Instant>,
    /**
    Next ICMP sequence number to use. Deliberately decoupled from `sent`:
    `sent` is decremented on send errors, so deriving seq from it could hand
    out a sequence number that is still in flight, which surge-ping rejects
    as an identical request.
    */
    pub next_seq: u16,
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

    /**
    Whether this target is (currently) considered unreachable. Logic:
    - If [DEFAULT_WIN] packets have been sent and none received -> unreachable
    - If last [DEFAULT_WIN] * 5 packets were all lost -> unreachable
    - Errors are NOT considered as unreachable.
    */
    #[inline]
    pub fn is_unreachable(&self) -> bool {
        if matches!(
            self.raw_status,
            PingStatus::Error(_) | PingStatus::Paused | PingStatus::Resuming
        ) {
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

/**
This struct represents a single ping target with its associated data and state.
### Field descriptions:
- `addr`: IP address of the target.
- `rev`: Reverse DNS name of the target.
- `data`: Inner data containing statistics and history (protected by a [RwLock]).
- `paused`: Atomic boolean indicating whether pinging is paused for this target.
- `cancel`: Cancellation token to signal a permanent stop. Setting this will
abort the (spawned) ping task, which currently is irreversible.
- `hops`: Last known hop count query response (protected by a [RwLock]).
- `ptr`: Last known PTR record query response (protected by a [RwLock]).
- `rev_ptr`: Last known reverse PTR record query response (protected by a [RwLock]).
- `hostname`: The host or DNS name this target was resolved from, if any.
- `added_order`: Monotonic creation stamp. The target list can be re-sorted
physically (column sorting in the UI); sorting by this restores the original
insertion order at any point, surviving runtime adds and removals.
*/
#[derive(Debug)]
pub(crate) struct PingTarget {
    pub addr: IpAddr,
    pub rev: String,
    pub added_order: u64,
    pub data: RwLock<PingTargetInner>,
    paused: AtomicBool,
    cancel: CancellationToken,
    hops: RwLock<QueryResponse>,
    ptr: RwLock<QueryResponse>,
    rev_ptr: RwLock<QueryResponse>,
    hostname: OnceLock<Arc<str>>,
}

impl PingTarget {
    /**
    Create a new [PingTarget] for the specified IP address.

    ### Args:
    - `histsize` specifies the size of the full RTT latency window.
    - `detailed` specifies the number of recent more detailed packet stats to keep.
    - `paused` specifies whether the target should be created in paused state (meaning,
    the spawned ping task will sleep until `paused` is set to `false`).
    */
    pub fn new(addr: IpAddr, histsize: usize, detailed: usize, paused: bool) -> Self {
        let mut data = PingTargetInner {
            rtts: LatencyWindow::new(histsize),
            recent: PacketHistory::new(detailed),
            ..Default::default()
        };
        if paused {
            data.raw_status = PingStatus::Paused;
        }
        Self {
            addr,
            rev: reverse_name(&addr),
            added_order: ADD_ORDER.fetch_add(1, Ordering::Relaxed),
            data: data.into(),
            hops: QueryResponse::default().into(),
            ptr: QueryResponse::default().into(),
            rev_ptr: QueryResponse::default().into(),
            paused: AtomicBool::new(paused),
            cancel: CancellationToken::new(),
            hostname: OnceLock::new(),
        }
    }

    /**
    Update statistics based on the result of a ping attempt and the associated packet record.

    NOTE: locks the inner `data` for writing.
    */
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
                    // Don't count errors, as the packet was never sent. Saturating,
                    // because a stats reset may race with an in-flight ping.
                    inner.sent = inner.sent.saturating_sub(1);
                    PingStatus::Error(e.to_string())
                }
            },
        };
        inner.recent.push(rec);
    }

    /**
    Set the (DNS/host) name associated with this target.
    Returns `true` if set successfully, `false` if it was already set.

    NOTE: the name can only be set once.
    */
    pub fn set_name(&self, name: Arc<str>) -> bool {
        self.hostname.set(name).is_ok()
    }

    /// Get the (DNS/host) name associated with this target, if any.
    #[inline]
    pub fn name(&self) -> Option<Arc<str>> {
        self.hostname.get().cloned()
    }

    /// Try to determine the hop count (distance) to this target. Blocking.
    pub fn determine_hops(&self, timeout: Duration) {
        if self.is_stopped() {
            return;
        }
        match determine_hops(self.addr, timeout, false) {
            Ok((h, _)) => *self.hops.write() = QueryResponse::Count(h as u64),
            Err(e) => *self.hops.write() = QueryResponse::Error(e),
        };
    }

    /// Get the last known hop count query response for this target, if any.
    pub fn hops(&self) -> QueryResponse {
        self.hops.read().clone()
    }

    /**
    Try to resolve the PTR record and the reverse of it for this target.

    NOTE: locks the fields `ptr` and `rev_ptr` for writing.
    */
    pub async fn resolve_ptr(&self, res: &Resolver<TokioConnectionProvider>) {
        // First, resolve PTR
        match res.reverse_lookup(self.addr).await {
            Ok(resp) => {
                let names: Vec<String> = resp
                    .iter()
                    .map(|r| r.to_string().trim_end_matches('.').to_string())
                    .collect();
                match names.len() {
                    0 => {
                        *self.ptr.write() = QueryResponse::Empty;
                        *self.rev_ptr.write() = QueryResponse::ErrorStr(ERR_PTR_EMPTY);
                    }

                    1 => {
                        let name = &names[0];
                        *self.ptr.write() = QueryResponse::Text(name.to_string());

                        // Now resolve A/AAAA for the PTR name
                        match res.lookup_ip(name).await {
                            Ok(ip_resp) => {
                                let ips: Vec<IpAddr> = ip_resp.iter().collect();
                                *self.rev_ptr.write() = if ips.is_empty() {
                                    QueryResponse::Empty
                                } else if ips.len() == 1 {
                                    QueryResponse::IpAddr(ips[0])
                                } else {
                                    QueryResponse::MultiIp(ips)
                                };
                            }

                            Err(e) => {
                                *self.rev_ptr.write() = QueryResponse::Error(e.to_string());
                            }
                        }
                    }

                    _ => {
                        *self.ptr.write() = QueryResponse::Text(names.join(", "));
                        *self.rev_ptr.write() = QueryResponse::TextStr(WARN_PTR_MANY); // TODO: handle multiple PTRs
                    }
                }
            }

            Err(e) => {
                *self.ptr.write() = QueryResponse::Error(e.to_string());
                *self.rev_ptr.write() = QueryResponse::ErrorStr(ERR_PTR_FAILED);
            }
        }
    }

    /**
    Get the last known PTR query response for this target, if any.

    NOTE: locks the field `ptr` for reading.
    */
    pub fn ptr(&self) -> QueryResponse {
        self.ptr.read().clone()
    }

    /**
    Get the last known reverse-of-PTR query response for this target, if any.

    NOTE: locks the field `rev_ptr` for reading.
    */
    pub fn rev_ptr(&self) -> QueryResponse {
        self.rev_ptr.read().clone()
    }

    /**
    Reset all statistics for this target as if it was never pinged.

    NOTE: locks the inner `data` for writing.
    */
    pub fn reset_stats(&self) {
        let mut data = self.data.write();
        data.sent = 0;
        data.recv = 0;
        data.rtts.clear();
        data.recent.clear();
        data.raw_status = PingStatus::None;
        data.last_seq = 0;
        data.last_sent = None;
        data.next_seq = 0;
    }

    /// Whether pinging currently paused for this target is.
    #[inline]
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /**
    Pause pinging for this target.

    NOTE: locks the inner `data` for writing.
    */
    pub fn pause(&self) {
        if !self.is_stopped() && !self.is_paused() {
            self.paused.store(true, Ordering::Relaxed);
            self.data.write().raw_status = PingStatus::Paused;
        }
    }

    /**
    Resume pinging for this target.

    NOTE: locks the inner `data` for writing.
    */
    pub fn resume(&self) {
        if !self.is_stopped() && self.is_paused() {
            self.paused.store(false, Ordering::Relaxed);
            self.data.write().raw_status = PingStatus::Resuming;
        }
    }

    /**
    Toggle paused state for this target.

    NOTE: locks the inner `data` for writing.
    */
    pub fn toggle_pause(&self) {
        if !self.is_stopped() {
            let was_paused: bool = self.paused.fetch_xor(true, Ordering::Relaxed);
            if was_paused {
                self.data.write().raw_status = PingStatus::Resuming;
            } else {
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

    /**
    Permanently stop pinging this target. Ping task will abort.

    NOTE: locks the inner `data` for writing.
    */
    pub fn stop(&self) {
        self.cancel.cancel();
        self.data.write().raw_status = PingStatus::Stopped;
    }

    /**
    Whether this target is (currently) considered unreachable.

    NOTE: a stopped target is never "unreachable" in this context.

    NOTE: may lock the inner `data` for reading.
    */
    #[inline]
    pub fn is_unreachable(&self) -> bool {
        if self.is_stopped() {
            return false;
        }
        self.data.read().is_unreachable()
    }

    /**
    Whether recent packet loss is above the default threshold.

    NOTE: locks the inner `data` for reading.
    */
    pub fn is_lossy(&self) -> bool {
        self.data.read().is_lossy(DEFAULT_WIN, LOSSY_THRESH)
    }

    /**
    Whether recent packet history shows flappiness (frequent up/down transitions)

    NOTE: locks the inner `data` for reading.
    */
    pub fn is_flappy(&self) -> bool {
        self.data.read().is_flappy(DEFAULT_WIN, FLAP_THRESH)
    }

    /**
    Whether recent RTTs are significantly above historical mean.

    NOTE: locks the inner `data` for reading.
    */
    pub fn is_laggy(&self) -> bool {
        match self.data.read().is_laggy(DEFAULT_WIN, LAGGY_FACTOR) {
            Ok(v) => v,
            Err(_) => false,
        }
    }

    /**
    Determine the effective status of this target, considering pauses,
    stops, and recent history analysis. Can return all states.

    NOTE: may lock the inner `data` for reading.
    */
    pub fn effective_status(&self) -> PingStatus {
        if self.is_stopped() {
            return PingStatus::Stopped;
        }
        if self.is_paused() {
            return PingStatus::Paused;
        }
        self.data.read().effective_status()
    }

    /**
    Get recent RTT samples as `(index, value)` pairs for graphing.
    Values are in milliseconds.

    `n` specifies the maximum number of samples to return, but
    less than `n` may be returned if fewer samples are available.

    NOTE: locks the inner `data` for reading.
    */
    pub fn get_recent_rtts(&self, n: usize) -> Vec<(f64, f64)> {
        let rtts: Vec<u32> = self
            .data
            .read()
            .rtts
            .recent_samples(n)
            .unwrap_or_else(|_| vec![]);
        rtts.iter()
            .enumerate()
            .map(|(i, &rtt)| (i as f64, rtt as f64 / MICROS_PER_MILLI)) // x: index, y: ms
            .collect()
    }

    /**
    Get RTT histogram buckets for recent N samples.

    `n` specifies the maximum number of samples to return, but
    less than `n` may be returned if fewer samples are available.

    This will call [PingTarget::get_recent_rtts] internally, so if you
    already have the RTT data, consider using [make_histogram_buckets]
    directly to avoid double work.

    NOTE: locks the inner `data` for reading.
    */
    pub fn get_rtt_histogram(&self, bins: usize, n: usize) -> Vec<HistogramBucket> {
        let rtts: Vec<f64> = self.get_recent_rtts(n).iter().map(|&(_, s)| s).collect();
        make_histogram_buckets(rtts, bins)
    }

    /**
    Return the (estimated!) distance to this target in kilometers, based on the
    *all-time* minimum RTT. The windowed minimum would expire with eviction and
    let the estimate drift upwards; physical distance doesn't change, so the
    best sample ever seen is the right basis. A stats reset (`R`) starts the
    measurement over - the escape hatch for anycast / network moves.

    Returns an error if minimum RTT is not available.

    ### Formula:
    - `L_geodesic ​≈ (RTT_min ​− t0​) ⋅ v​ / (2 ⋅ s ⋅ factor)`

    where:
    - `L_geodesic` is the estimated one-way distance to the target (km)
    - `RTT_min` is the minimum observed round-trip time (s)
    - `t0` is a latency floor to account for non-propagation delays (s)
    - `v` is the speed of light in fiber (km/s)
    - `s` is a (default) stretch factor to account for non-direct paths, routing, etc.
    - `factor` is an additional user-defined stretch/compression factor
      (clamped to >0.1 to avoid div by zero)

    Further, we assume we can't go below the latency floor (`t0`) and the
    distance is at least 1 meter (0.001 km).

    NOTE: this is a very rough estimate and should not be relied upon.

    NOTE: locks the inner `data` for reading.
    */
    #[inline]
    pub fn est_distance_km(&self, factor: f64) -> Result<f64, String> {
        self.data.read().rtts.min_ever().map(|micros| {
            let rtt_min: f64 = (micros as f64 / 1e6).max(LATENCY_FLOOR); // assume at least t0
            let factor: f64 = factor.max(0.1); // avoid div by zero
            let l_geodesic: f64 =
                ((rtt_min - LATENCY_FLOOR) * SPEED_KM_S) / (2.0 * STRETCH_FACTOR * factor);
            l_geodesic.max(1e-3) // at least 1 meter
        })
    }

    /**
    String form of the (estimated!) distance to this target, derived from minimum RTT.

    ### Args:
    - `factor`: Stretch/compression factor (>0.0) to adjust distance estimates.

    NOTE: this is a very rough estimate and should be taken with a grain of salt.
    Many things can affect RTT that have nothing to do with physical distance,
    such as routing, congestion, peering, etc. Even though over the long term minimum
    RTT converges to smoot out most of the noise, it's not a reliable measure by any means.

    NOTE: locks the inner `data` for reading.
    */
    #[inline]
    pub fn est_distance_str(&self, factor: f64) -> String {
        match self.est_distance_km(factor) {
            Ok(dist) if dist > 0.0 => {
                match dist {
                    d if d < 2.0 => return INFO_LOCAL.to_string(),
                    d if (d < 30.0 && d >= 2.0) => return INFO_NEARBY.to_string(),
                    d if (d < BAND_SIZE_KM && d >= 30.0) => {
                        return format!("< {:.0} km", BAND_SIZE_KM);
                    }
                    d if (d < BAND_SIZE_KM * 2.0 && d >= BAND_SIZE_KM) => {
                        return format!("< {:.0} km", BAND_SIZE_KM * 2.0);
                    }
                    d if (d < BAND_SIZE_KM * 5.0 && d >= BAND_SIZE_KM * 2.0) => {
                        return format!("< {:.0} km", BAND_SIZE_KM * 5.0);
                    }
                    d if (d < BAND_SIZE_KM * 10.0 && d >= BAND_SIZE_KM * 5.0) => {
                        return format!("< {:.0} km", BAND_SIZE_KM * 10.0);
                    }
                    d if d > SPEED_KM_S / 5.0 => return INFO_INTERPLANETARY.to_string(),
                    _ => {
                        // Quantize to nearest lower band
                        let banded = (dist / BAND_SIZE_KM).floor() * BAND_SIZE_KM;
                        format!("≈ {:.0}+ km", banded)
                    }
                }
            }
            _ => MISSING.to_string(),
        }
    }
}

impl fmt::Display for PingTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(name) = self.name() {
            write!(f, "{} ({})", self.addr, &*name)
        } else {
            write!(f, "{}", self.addr)
        }
    }
}

/* -------------------------------------------------------------------------- */

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

/* ---------------------------------- */

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

/* ---------------------------------- */

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

/* -------------------------------------------------------------------------- */

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
            /*
            If delta > 32768, it wrapped the "wrong way" -> out of order.
            I'm told this is the "standard" way to check for u16 wraparound
            in sequence numbers, so let's go with it.
            */
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

/* -------------------------------------------------------------------------- */

/**
Snapshot of ping statistics at a point in time.

Includes stringifying methods for display purposes.
Unless otherwise noted, RTT values are stored as
microseconds and displayed as milliseconds.
*/
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
    /**
    Extract a [StatsSnapshot] from [PingTargetInner]

    - `timeout` is the overall ping timeout duration.
    */
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
            // Saturating: a stats reset racing an in-flight reply can leave recv > sent.
            self.sent.saturating_sub(self.recv) as f64 / self.sent as f64
        }
    }

    /**
    Whether the latest sent packet is still considered "in flight" (not yet timed out).

    NOTE: This is based on this snapshot's creation timestamp (`now`), not the current
    time, so it may be slightly out of date. Sufficient for display purposes.
    */
    #[inline]
    fn is_latest_inflight(&self) -> bool {
        self.timeout > self.when.duration_since(self.latest_sent)
    }

    /// Packet loss as formatted string.
    #[inline]
    pub fn loss_str(&self) -> String {
        if self.sent == 0 {
            MISSING.to_string()
        } else if (self.sent.saturating_sub(self.recv) == 1) && self.is_latest_inflight() {
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
