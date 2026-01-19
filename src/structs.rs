// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{
    args::MpConfig,
    logging::{Logger, MessageBuffer},
    pingdata::PingTarget,
    strings::*,
    utils::nice_permission_error,
};
use hickory_resolver::TokioResolver;
use miniutils::{ProcessInfo, inject, templater};
use parking_lot::RwLock;
use std::{
    collections::{HashMap, HashSet},
    fmt::{self, Display},
    net::IpAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use surge_ping::{Client, Config, ICMP};
use tokio::sync::Notify;

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
    runtime: tokio::runtime::Handle,
    spawned_tasks: AtomicU64,
    perf: AtomicBool,
    resolved: RwLock<Resolved>,
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
            runtime: tokio::runtime::Handle::current(),
            spawned_tasks: AtomicU64::new(0),
            perf: conf.perf.into(),
            resolved: conf.resolved.clone().into(),
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

    /// The number of spawned ping tasks so far.
    pub fn spawned_tasks(&self) -> u64 {
        self.spawned_tasks.load(Ordering::Relaxed)
    }

    /// Increment the spawned tasks counter by one.
    #[inline]
    pub fn inc_spawned_tasks(&self) {
        self.spawned_tasks.fetch_add(1, Ordering::Relaxed);
    }

    /// Whether perf mode is enabled (aka. reduce task spawn overhead).
    #[inline]
    pub fn perf(&self) -> bool {
        self.perf.load(Ordering::Relaxed)
    }

    /// Toggle perf mode on/off.
    pub fn toggle_perf(&self) {
        self.perf.fetch_xor(true, Ordering::SeqCst);
        self.logger.debug(templater!(
            INFO_PERF,
            if self.perf() { ENABLED } else { DISABLED }
        ));
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
    pub fn quit(&self) {
        self.quit.store(true, Ordering::Relaxed);
    }

    /**
    Add new ping targets to the application state. Will also set their names if resolved.

    NOTE: locks `targets` for writing and `resolved` for reading.
    */
    pub fn add_targets<I: IntoIterator<Item = PingTarget>>(&self, targets: I) {
        let orig_len: usize = self.len();
        let names = self.resolved.read();
        self.targets.write().extend(targets.into_iter().map(|t| {
            // set the hostname if we have one for this IP
            if let Some(name) = names.get_name(&t.addr) {
                t.set_name(name);
            }
            Arc::new(t)
        }));
        let new_len: usize = self.len();
        if new_len > orig_len {
            self.logger.log(templater!(
                INFO_NEW,
                new_len.saturating_sub(orig_len),
                orig_len,
                new_len
            ));
        }
    }

    /// Pause pinging for the target at the specified index.
    pub fn toggle_target_pause(&self, index: usize) {
        if let Some(tgt) = self.targets.read().get(index) {
            tgt.toggle_pause();
            self.logger.debug(templater!(
                INFO_PING,
                tgt,
                if tgt.is_paused() { PAUSED } else { RESUMED }
            ));
        }
    }

    /// Pause pinging for all targets.
    pub fn pause_all_targets(&self) {
        self.logger.log(INFO_P_ALL);
        for tgt in self.targets.read().iter() {
            tgt.pause();
        }
    }

    /// Resume pinging for all targets.
    pub fn resume_all_targets(&self) {
        self.logger.log(INFO_R_ALL);
        for tgt in self.targets.read().iter() {
            tgt.resume();
        }
    }

    /// Update information for the target at the specified index. Nonblocking.
    pub fn update_target_info(&self, index: usize) {
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
            self.runtime.spawn_blocking(move || {
                let now: Instant = Instant::now();
                tgt_ptr1.determine_hops(UPDATE_TASK_TIMEOUT);
                logger.debug(templater!(
                    INFO_HOPS,
                    tgt_ptr1.addr,
                    format!("{:.2}", now.elapsed().as_secs_f32() * 1e3)
                ));
            });
            self.inc_spawned_tasks();

            /*
            Because this function can be called from the keyboard event handler thread
            (which is not inside the tokio runtime context), we must use the stored
            runtime handle to spawn the task, or we will panic that thread.
            */
            let logger = self.logger.clone();
            self.runtime.spawn(async move {
                let now: Instant = Instant::now();
                tgt_ptr2.resolve_ptr(&resolver).await;
                logger.debug(templater!(
                    INFO_PTR,
                    tgt_ptr2.addr,
                    format!("{:.2}", now.elapsed().as_secs_f32() * 1e3)
                ));
            });
            self.inc_spawned_tasks();
        }
    }

    /// Reset statistics for the target at the specified index.
    pub fn reset_target_stats(&self, index: usize) {
        if let Some(tgt) = self.targets.read().get(index) {
            self.logger.log(templater!(INFO_RESET, tgt));
            tgt.reset_stats();
        }
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
    pub fn stop_target(&self, index: usize) {
        if let Some(tgt) = self.targets.read().get(index) {
            if !tgt.is_stopped() {
                self.logger.notice(templater!(INFO_STOP, tgt));
            }
            tgt.stop();
        }
    }

    /// Stop pinging the target at the specified index and remove it from the target list.
    pub fn remove_target(&self, index: usize) {
        let mut targets = self.targets.write();
        if let Some(tgt) = targets.get(index) {
            self.logger.notice(templater!(INFO_REMOVE, tgt));
            tgt.stop();
            targets.remove(index);
        }
    }

    /// Remove all unreachable targets and return the count if any were nuked.
    pub fn remove_all_unreachables(&self) -> usize {
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
        }
        num
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
