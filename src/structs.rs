// Copyright (c) 2025-2026 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{
    args::MpConfig,
    logging::{Logger, MessageBuffer},
    pingdata::{PingStatus, PingTarget},
    strings::*,
    utils::nice_permission_error,
};
use hickory_resolver::TokioResolver;
use miniutils::{ProcessInfo, inject, templater};
use parking_lot::RwLock;
use std::{
    collections::{HashMap, HashSet},
    fmt::{self, Display},
    future::Future,
    net::IpAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use surge_ping::{Client, Config, ICMP};
use tokio::{sync::Notify, task::JoinHandle};

pub(crate) const DEFAULT_PAYLOAD_SIZE: usize = 48;
pub(crate) const DEFAULT_HISTSIZE: usize = 3600; // one hour of per-second history
pub(crate) const DEFAULT_DETAILED: usize = 100; // detailed RTT history size
const PROCINFO_INTERVAL: u64 = 1000; // CPU+RAM update interval in ms (1 Hz is plenty for us)
const UPDATE_TASK_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(target_os = "linux")]
pub static SYSTEM_TTL: u8 = 64;
#[cfg(target_os = "macos")]
pub static SYSTEM_TTL: u8 = 64;
#[cfg(target_os = "windows")]
pub static SYSTEM_TTL: u8 = 128;

/// Main application state structure. Holds shared state across threads and tasks.
/// Needs to be put into an Arc after intialization, which `build()` conveniently does.
pub(crate) struct AppState {
    pub pi: ProcessInfo,
    pub c_v4: Option<Arc<Client>>,
    pub c_v6: Option<Arc<Client>>,
    pub targets: RwLock<Vec<Arc<PingTarget>>>,
    pub tasks: RwLock<Vec<tokio::task::JoinHandle<()>>>,
    pub defaults: TargetDefaults,
    pub debug: bool,
    pub quit: Arc<AtomicBool>,
    pub payload: Arc<[u8]>,
    pub internal_tick: Duration,
    pub key_event: Notify,
    pub resolver: Arc<TokioResolver>,
    pub logger: Arc<MessageBuffer>,
    pub distance_stretch_factor: f64,
    pub resolved: RwLock<Resolved>,
    runtime: tokio::runtime::Handle,
    spawned_tasks: AtomicU64,
    perf: AtomicBool,
}

impl AppState {
    /**
    Initialize base application state from the commandline options. Sets up:
    - [ProcessInfo] tracking
    - intervals: pinging, internal tick
    - flags: debug, randomize, perf
    - events: key event notifier, quit flag
    - default payload
    - [TargetDefaults] as per config
    - other default fields
    */
    #[must_use = "AppState must be built using .build()"]
    pub fn from_conf(conf: &MpConfig) -> Self {
        // gather target defaults from config
        let tgt_defaults = TargetDefaults {
            histsize: conf.histsize as usize,
            detailed: conf.detailed as usize,
            interval: conf.interval,
            timeout: conf.timeout,
            paused: conf.paused,
            randomize: conf.randomize,
        };

        Self {
            pi: ProcessInfo::new().with_min_interval(PROCINFO_INTERVAL),
            c_v4: None,
            c_v6: None,
            targets: vec![].into(),
            tasks: vec![].into(),
            defaults: tgt_defaults,
            debug: conf.debug,
            quit: AtomicBool::new(false).into(),
            payload: vec![0u8; conf.size as usize].into(), // 48 bytes -> 56-byte packet
            // adjust internal tick (delay) lower if it's higher than ping
            // interval, othwerwise we'd send out fewer pings than intended
            internal_tick: Duration::from_millis(100).min(conf.interval),
            key_event: Notify::new(),
            resolver: conf.resolver.as_ref().unwrap().clone(),
            logger: conf.buf.clone(),
            distance_stretch_factor: conf.stretch_factor,
            resolved: conf.resolved.clone().into(),
            runtime: tokio::runtime::Handle::current(),
            spawned_tasks: AtomicU64::new(0),
            perf: conf.perf.into(),
        }
    }

    /**
    Build the final application state from initial targets.
    - set up [surge_ping::Client] instances for IPv4 and IPv6
    - add provided ping targets (if any)

    Returns an error if client creation fails.

    NOTE: sharing a client is (async) safe and allows socket reuse.
    */
    pub fn build(
        mut self,
        targets: Vec<PingTarget>,
    ) -> Result<Arc<Self>, Box<dyn std::error::Error>> {
        // IPv4 & IPv6 clients
        self.c_v4 = {
            match Client::new(&Config::default()) {
                Ok(c) => Arc::new(c).into(),
                Err(e) => return Err(nice_permission_error(&e, 4)),
            }
        };
        self.c_v6 = {
            match Client::new(&Config::builder().kind(ICMP::V6).build()) {
                Ok(c) => Arc::new(c).into(),
                Err(e) => return Err(nice_permission_error(&e, 6)),
            }
        };

        self.add_targets(targets);
        self.logger.trace(INFO_STATE_INIT);
        Ok(self.into())
    }

    /// The number of ping targets in the application state.
    pub fn len(&self) -> usize {
        self.targets.read().len()
    }

    /// Execute/dispatch a [Command] sent from the UI.
    pub fn execute(&self, cmd: Command) -> CmdResult {
        match cmd {
            Command::Quit => self.quit(),
            Command::PauseAll => self.pause_all_targets(),
            Command::ResumeAll => self.resume_all_targets(),
            Command::TogglePause(idx) => self.toggle_target_pause(idx),
            Command::StopTarget(idx) => self.stop_target(idx),
            Command::RemoveTarget(idx) => self.remove_target(idx),
            Command::UpdateTgtInfo(idx) => self.update_target_info(idx),
            Command::ResetTgtStats(idx) => self.reset_target_stats(idx),
            Command::TogglePerf => self.toggle_perf(),
            Command::RemoveAllUnreach => self.remove_all_unreachables(),
            Command::Sort(col, desc) => self.sort_targets(col, desc),
            Command::SortReset => self.sort_targets_reset(),
        }
    }

    /// Passthrough method to [tokio::runtime::Handle::spawn].
    pub fn spawn<F>(&self, fut: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.inc_spawned_tasks();
        self.runtime.spawn(fut)
    }

    /// Passthrough method to [tokio::runtime::Handle::spawn_blocking].
    pub fn spawn_blocking<F, R>(&self, func: F) -> JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        self.inc_spawned_tasks();
        self.runtime.spawn_blocking(func)
    }

    /// The number of spawned ping tasks so far.
    pub fn spawned_tasks(&self) -> u64 {
        self.spawned_tasks.load(Ordering::Relaxed)
    }

    /// Increment the spawned tasks counter by one.
    #[inline]
    fn inc_spawned_tasks(&self) {
        self.spawned_tasks.fetch_add(1, Ordering::Relaxed);
    }

    /// Whether perf mode is enabled (aka. reduce task spawn overhead).
    #[inline]
    pub fn perf(&self) -> bool {
        self.perf.load(Ordering::Relaxed)
    }

    /// Toggle perf mode on/off.
    fn toggle_perf(&self) -> CmdResult {
        self.perf.fetch_xor(true, Ordering::SeqCst);
        self.logger.debug(templater!(
            INFO_PERF,
            if self.perf() { ENABLED } else { DISABLED }
        ));
        CmdResult::Done
    }

    /// Whether the quit flag has been toggled.
    pub fn is_quitting(&self) -> bool {
        self.quit.load(Ordering::Relaxed)
    }

    /// Whether the quit flag has been toggled. Async version for `tokio::select!` to `await` on it.
    pub async fn is_quitting_async(&self) -> bool {
        self.quit.load(Ordering::Relaxed)
    }

    /// Set the quit flag to true. This triggers a graceful shutdown in a short order.
    fn quit(&self) -> CmdResult {
        self.quit.store(true, Ordering::Relaxed);
        CmdResult::ByeBye
    }

    /**
    Add new ping targets to the application state. Skips any whose IP is already
    being pinged. Sets hostnames from [Self::resolved] for the ones that survive.

    Returns a tuple of:
    - the [Arc]'d handles to the newly-added targets (caller spawns their ping
      loops, see [crate::pinger::spawn_ping_loops]),
    - the number of duplicate targets that were skipped.

    NOTE: locks `targets` for writing and `resolved` for reading.
    */
    pub fn add_targets<I: IntoIterator<Item = PingTarget>>(
        &self,
        targets: I,
    ) -> (Vec<Arc<PingTarget>>, usize) {
        let mut tgts = self.targets.write();
        let existing: HashSet<IpAddr> = tgts.iter().map(|t| t.addr).collect();
        let names = self.resolved.read();
        let orig_len: usize = tgts.len();

        let mut added: Vec<Arc<PingTarget>> = Vec::new();
        let mut skipped: usize = 0;
        for t in targets {
            if !existing.contains(&t.addr) && added.iter().all(|a| a.addr != t.addr) {
                if let Some(name) = names.get_name(&t.addr) {
                    t.set_name(name);
                }
                let arc = Arc::new(t);
                tgts.push(arc.clone());
                added.push(arc);
            } else {
                skipped += 1;
            }
        }
        let new_len: usize = tgts.len();
        drop(names);
        drop(tgts);

        if !added.is_empty() {
            self.logger
                .log(templater!(INFO_NEW, added.len(), orig_len, new_len));
        }
        if skipped > 0 {
            self.logger
                .notice(format!("skipped {skipped} duplicate target(s)"));
        }
        (added, skipped)
    }

    /// Pause pinging for the target at the specified index.
    fn toggle_target_pause(&self, index: usize) -> CmdResult {
        if let Some(tgt) = self.targets.read().get(index) {
            tgt.toggle_pause();
            self.logger.debug(templater!(
                INFO_PING,
                tgt,
                if tgt.is_paused() { PAUSED } else { RESUMED }
            ));
            return CmdResult::Done;
        }
        CmdResult::NotFound
    }

    /// Pause pinging for all targets.
    fn pause_all_targets(&self) -> CmdResult {
        self.logger.log(INFO_P_ALL);
        self.targets.read().iter().for_each(|tgt| tgt.pause());
        CmdResult::Done
    }

    /// Resume pinging for all targets.
    fn resume_all_targets(&self) -> CmdResult {
        self.logger.log(INFO_R_ALL);
        self.targets.read().iter().for_each(|tgt| tgt.resume());
        CmdResult::Done
    }

    /// Update information for the target at the specified index. Nonblocking.
    fn update_target_info(&self, index: usize) -> CmdResult {
        if let Some(tgt) = self.targets.read().get(index) {
            self.logger.log(templater!(INFO_UPD, tgt));
            let tgt_ptr1 = tgt.clone();
            let tgt_ptr2 = tgt.clone();
            let resolver = self.resolver.clone();
            /*
            `determine_hops` is blocking, so spawn a thread for it to not block the caller.

            NOTE: we specifically can't use `tokio::spawn` here because `determine_hops`
            will eventually acquire a write lock to one/some of its fields, and there's a
            very good change that the caller will deadlock (or panic) on the same if it's
            scheduled in the same runtime thread.
            */
            let logger = self.logger.clone();
            self.spawn_blocking(move || {
                let now: Instant = Instant::now();
                tgt_ptr1.determine_hops(UPDATE_TASK_TIMEOUT);
                logger.debug(templater!(
                    INFO_HOPS,
                    tgt_ptr1.addr,
                    format!("{:.2}", now.elapsed().as_secs_f32() * 1e3)
                ));
            });

            /*
            Because this function can be called from the keyboard event handler thread
            (which is not inside the tokio runtime context), we must use the stored
            runtime handle to spawn the task, or we will panic that thread.
            */
            let logger = self.logger.clone();
            self.spawn(async move {
                let now: Instant = Instant::now();
                tgt_ptr2.resolve_ptr(&resolver).await;
                logger.debug(templater!(
                    INFO_PTR,
                    tgt_ptr2.addr,
                    format!("{:.2}", now.elapsed().as_secs_f32() * 1e3)
                ));
            });
            return CmdResult::Done;
        }
        CmdResult::NotFound
    }

    /// Reset statistics for the target at the specified index.
    fn reset_target_stats(&self, index: usize) -> CmdResult {
        if let Some(tgt) = self.targets.read().get(index) {
            self.logger.log(templater!(INFO_RESET, tgt));
            tgt.reset_stats();
            return CmdResult::Done;
        }
        CmdResult::NotFound
    }

    /// Whether pinging is permanently stopped for the target at the specified index.
    pub fn is_target_stopped(&self, index: usize) -> bool {
        if let Some(tgt) = self.targets.read().get(index) {
            tgt.is_stopped()
        } else {
            false
        }
    }

    /// Stop pinging the target at the specified index. The ping task will abort permanently.
    fn stop_target(&self, index: usize) -> CmdResult {
        if let Some(tgt) = self.targets.read().get(index) {
            if !tgt.is_stopped() {
                self.logger.notice(templater!(INFO_STOP, tgt));
            }
            tgt.stop();
            return CmdResult::Done;
        }
        CmdResult::NotFound
    }

    /// Stop pinging the target at the specified index and remove it from the target list.
    fn remove_target(&self, index: usize) -> CmdResult {
        let mut targets = self.targets.write();
        if let Some(tgt) = targets.get(index) {
            self.logger.notice(templater!(INFO_REMOVE, tgt));
            tgt.stop();
            targets.remove(index);
            return CmdResult::Done;
        }
        CmdResult::NotFound
    }

    /// Remove all unreachable targets and return the count if any were nuked.
    fn remove_all_unreachables(&self) -> CmdResult {
        let mut targets = self.targets.write();
        let orig_len: usize = targets.len();
        let now: Instant = Instant::now();
        targets.retain(|tgt: &Arc<PingTarget>| match tgt.is_unreachable() {
            true => {
                tgt.stop();
                false
            }
            false => true,
        });
        let num: usize = orig_len.saturating_sub(targets.len());
        if num > 0 {
            self.logger.notice(templater!(
                INFO_UNR_REM,
                num,
                format!("{:.2}", now.elapsed().as_secs_f32() * 1e3),
                orig_len,
                orig_len - num
            ));
            return CmdResult::Count(num);
        }
        CmdResult::None
    }

    /**
    Sort the target list by the given column index (see `COL_*` in
    [crate::strings]). One-shot: the list keeps the new order until the next
    sort or [Self::sort_targets_reset] - data changing afterwards does NOT
    re-sort (rows jumping around under the cursor would be maddening).

    The sort is stable (ties keep their current relative order) and targets
    with no data for the column always sort last, regardless of direction.

    NOTE: locks `targets` for writing, and each target's inner `data` for reading.
    */
    fn sort_targets(&self, col: usize, descending: bool) -> CmdResult {
        if col > COL_SEQ || (col == COL_SEQ && !self.debug) {
            return CmdResult::NotFound;
        }

        // decorate-sort-undecorate so each target's data lock is taken
        // exactly once instead of O(n log n) times inside the comparator
        let mut targets = self.targets.write();
        let mut keyed: Vec<((bool, SortVal), Arc<PingTarget>)> = targets
            .iter()
            .map(|t| (sort_key(t, col), t.clone()))
            .collect();
        keyed.sort_by(|(a, _), (b, _)| {
            // missing-data flag first (always ascending), then the value
            a.0.cmp(&b.0).then_with(|| {
                let ord = a.1.cmp(&b.1);
                if descending { ord.reverse() } else { ord }
            })
        });
        *targets = keyed.into_iter().map(|(_, t)| t).collect();
        drop(targets);

        self.logger.debug(templater!(
            INFO_SORTED,
            HEADERS.get(col).copied().unwrap_or(HDR_SEQ),
            if descending { SORT_DESC } else { SORT_ASC }
        ));
        CmdResult::Done
    }

    /// Restore the original (insertion) order of the target list by sorting
    /// on [PingTarget::added_order].
    ///
    /// NOTE: locks `targets` for writing.
    fn sort_targets_reset(&self) -> CmdResult {
        self.targets.write().sort_by_key(|t| t.added_order);
        self.logger.debug(INFO_SORT_RESET);
        CmdResult::Done
    }
}

/* ---------------------------------- */

/// Comparable per-column sort key. The same column always yields the same
/// variant, so cross-variant comparisons cannot happen in practice.
enum SortVal {
    Ip(IpAddr),
    Num(f64),
}

impl SortVal {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (SortVal::Ip(a), SortVal::Ip(b)) => a.cmp(b),
            (SortVal::Num(a), SortVal::Num(b)) => a.total_cmp(b),
            _ => std::cmp::Ordering::Equal, // unreachable (see above)
        }
    }
}

/// Rank effective statuses from best (0) to worst for sorting purposes.
fn status_rank(s: &PingStatus) -> f64 {
    match s {
        PingStatus::Ok => 0.0,
        PingStatus::Resuming => 1.0,
        PingStatus::Laggy => 2.0,
        PingStatus::Flappy => 3.0,
        PingStatus::Lossy => 4.0,
        PingStatus::Timeout => 5.0,
        PingStatus::NotReachable => 6.0,
        PingStatus::Error(_) => 7.0,
        PingStatus::Paused => 8.0,
        PingStatus::Stopped => 9.0,
        PingStatus::None => 10.0,
    }
}

/// Extract a `(missing, key)` sort pair from a target for the given column.
/// `missing` entries are sorted last by [AppState::sort_targets].
///
/// NOTE: may lock the target's inner `data` for reading.
fn sort_key(tgt: &Arc<PingTarget>, col: usize) -> (bool, SortVal) {
    use SortVal::*;

    /// Helper for the Option-valued RTT columns.
    fn opt(v: Option<f64>) -> (bool, SortVal) {
        match v {
            Some(v) => (false, Num(v)),
            None => (true, Num(0.0)),
        }
    }

    // these two don't need the data lock
    match col {
        COL_ADDRESS => return (false, Ip(tgt.addr)),
        COL_STATUS => return (false, Num(status_rank(&tgt.effective_status()))),
        _ => {}
    }

    let data = tgt.data.read();
    match col {
        COL_SENT => (false, Num(data.sent as f64)),
        COL_RECV => (false, Num(data.recv as f64)),
        COL_LOSS => match data.sent {
            0 => (true, Num(0.0)),
            s => (false, Num(s.saturating_sub(data.recv) as f64 / s as f64)),
        },
        COL_LAST => opt(data.rtts.last().ok().map(|v| v as f64)),
        COL_MEAN => opt(data.rtts.mean().ok()),
        COL_MIN => opt(data.rtts.min().ok().map(|v| v as f64)),
        COL_MAX => opt(data.rtts.max().ok().map(|v| v as f64)),
        COL_STDEV => opt(data.rtts.stdev().ok()),
        COL_SEQ => (false, Num(data.last_seq as f64)),
        _ => (true, Num(0.0)), // out of range - sorts last, caller filtered already
    }
}

/* ---------------------------------- */

/// Default settings for new ping targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TargetDefaults {
    pub histsize: usize,
    pub detailed: usize,
    pub interval: Duration,
    pub timeout: Duration,
    pub paused: bool,
    pub randomize: bool,
}

impl Default for TargetDefaults {
    fn default() -> Self {
        Self {
            histsize: DEFAULT_HISTSIZE,
            detailed: DEFAULT_DETAILED,
            interval: Duration::from_secs(1),
            timeout: Duration::from_secs(2),
            paused: false,
            randomize: false,
        }
    }
}

/* -------------------------------------------------------------------------- */

/**
Map resolved IP addresses by name and by address.

NOTE: in the current implementation, if the same IP address is added
from multiple names, the last addition "wins" the IP -> name mapping.
*/
#[derive(Debug, Default, Clone)]
pub(crate) struct Resolved {
    by_name: HashMap<Arc<str>, HashSet<IpAddr>>,
    by_addr: HashMap<IpAddr, Arc<str>>,
}

impl Resolved {
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Add a resolved name and its associated IP address(es).
    pub fn add<S: AsRef<str>>(&mut self, name: S, ips: &[IpAddr]) {
        let name: Arc<str> = self
            .by_name
            .get_key_value(name.as_ref())
            .map(|(k, _)| k.clone())
            .unwrap_or_else(|| Arc::from(name.as_ref()));

        // This would work too, but let's go with a simple for-loop for clarity.
        // ips.iter().for_each(|a| {self.by_addr.insert(*a, name.clone());});
        for ip in ips {
            self.by_addr.insert(*ip, name.clone());
        }

        self.by_name
            .entry(name)
            .or_default()
            .extend(ips.iter().copied());
    }

    pub fn contains_addr(&self, ip: &IpAddr) -> bool {
        self.by_addr.contains_key(ip)
    }

    pub fn contains_name<S: AsRef<str>>(&self, name: S) -> bool {
        self.by_name.contains_key(name.as_ref())
    }

    /// All resolved IP addresses as a Vec in no particular order.
    pub fn addresses(&self) -> Vec<IpAddr> {
        self.by_addr.keys().cloned().collect()
    }

    /// All resolved IP addresses as a Vec grouped by name.
    pub fn addrs_by_name(&self) -> Vec<IpAddr> {
        self.by_name
            .iter()
            .flat_map(|(_, ips)| ips.iter().cloned())
            .collect()
    }

    /// Get the resolved name for the given IP address, if any.
    pub fn get_name(&self, ip: &IpAddr) -> Option<Arc<str>> {
        self.by_addr.get(ip).cloned()
    }

    /// Get the resolved IP addresses for the given name, if any.
    pub fn get_addrs<S: AsRef<str>>(&self, name: S) -> Option<&HashSet<IpAddr>> {
        self.by_name.get(name.as_ref())
    }
}

/* -------------------------------------------------------------------------- */

/// An enum representing different types of remote query results.
#[derive(Default, Debug, Clone)]
pub(crate) enum QueryResponse {
    IpAddr(IpAddr),
    Count(u64),
    Float(f64),
    Text(String),
    Error(String),
    Empty,
    #[default]
    None,
    TextStr(&'static str),
    ErrorStr(&'static str),
    MultiIp(Vec<IpAddr>),
}

impl QueryResponse {
    /// Whether the response is empty or none.
    pub fn is_empty(&self) -> bool {
        matches!(self, QueryResponse::Empty | QueryResponse::None)
    }

    /// Whether the response is an error.
    pub fn is_err(&self) -> bool {
        matches!(self, QueryResponse::Error(_) | QueryResponse::ErrorStr(_))
    }
}

impl Display for QueryResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueryResponse::IpAddr(ip) => write!(f, "{ip}"),
            QueryResponse::Count(c) => write!(f, "{c}"),
            QueryResponse::Float(v) => write!(f, "{v}"),
            QueryResponse::Text(s) => write!(f, "{s}"),
            QueryResponse::Error(e) => write!(f, "E: {e}"),
            QueryResponse::Empty => write!(f, "<{EMPTY_RESP}>"),
            QueryResponse::None => write!(f, "<{UNKNOWN}>"),
            QueryResponse::TextStr(s) => write!(f, "{s}"),
            QueryResponse::ErrorStr(e) => write!(f, "E: {e}"),
            QueryResponse::MultiIp(ips) => write!(
                f,
                "{}",
                ips.iter()
                    .map(|ip| ip.to_string())
                    .collect::<Vec<String>>()
                    .join(", ")
            ),
        }
    }
}

/* -------------------------------------------------------------------------- */

/// Commands sent from the UI to the main application logic.
pub(crate) enum Command {
    Quit,
    /// Pause pinging for all targets.
    PauseAll,
    /// Resume pinging for all targets.
    ResumeAll,
    /// Toggle pinging for the given target.
    TogglePause(usize),
    /// Permanently stop pinging the given target.
    StopTarget(usize),
    /// Permanently remove the given target.
    RemoveTarget(usize),
    /// Update information for the given target (hops, DNS query etc.).
    UpdateTgtInfo(usize),
    /// Reset statistics for the given target.
    ResetTgtStats(usize),
    /// Toggle performance mode on/off.
    TogglePerf,
    /// Remove all unreachable targets.
    RemoveAllUnreach,
    /// Sort the target list by column index (`true` = descending). One-shot.
    Sort(usize, bool),
    /// Restore the original (insertion) order of the target list.
    SortReset,
}

/// Result of a [Command] execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CmdResult {
    None,
    Done,
    Count(usize),
    NotFound,
    Error,
    ByeBye,
}
