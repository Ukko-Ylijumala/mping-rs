// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{
    args::MpConfig,
    pingdata::PingTarget,
    strings::{APP_TITLE, HEADERS},
    tui::{AppLayout, MutableLine, TableRow},
    utils::nice_permission_error,
};
use miniutils::ProcessInfo;
use parking_lot::RwLock;
use ratatui::{prelude::Stylize, style::Style, text::Line, widgets::Paragraph};
use std::{
    fmt::Display,
    net::IpAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use surge_ping::{Client, Config, ICMP};
use tokio::sync::Notify;

pub const DEFAULT_PAYLOAD_SIZE: usize = 48;
const PROCINFO_INTERVAL: u64 = 1000; // CPU+RAM update interval in ms (1 Hz is plenty for us)
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
    pub layout: RwLock<AppLayout>,
    pub title: Line<'static>,
    /// Table headers
    pub headers: TableRow,
    /// UI refresh interval
    pub ui_interval: Duration,
    /// Next scheduled UI refresh time
    pub ui_next_refresh: RwLock<tokio::time::Instant>,
    pub verbose: bool,
    pub debug: bool,
    pub quit: Arc<AtomicBool>,
    pub ping_interval: Duration,
    pub ping_timeout: Duration,
    pub randomize: bool,
    pub payload: Arc<[u8]>,
    pub internal_tick: Duration,
    pub key_event: Notify,
    /// Status line is the last line at the bottom left-side of the UI.
    pub status_line: MutableLine<'static>,
    pub popup_contents: RwLock<Option<PopupContents>>,
    spawned_tasks: AtomicU64,
    perf: AtomicBool,
}

impl AppState {
    /**
    Build the application state based on the provided configuration.
    - set up UI refresh interval
    - set up [surge_ping::Client] instances for IPv4 and IPv6 as needed
    - add provided ping targets

    NOTE: sharing a client across multiple targets is (async) safe
    and allows socket reuse.
    */
    pub fn build(
        mut self,
        conf: &MpConfig,
        targets: Vec<PingTarget>,
    ) -> Result<Arc<Self>, Box<dyn std::error::Error>> {
        self.debug = conf.debug;
        self.verbose = conf.verbose;
        self.randomize = conf.randomize;
        self.perf = conf.perf.into();

        // setup app title row with version and styling
        self.title.push_span(format!(" v{}", conf.ver));
        self.title = self.title.centered().bold().red().on_green();

        // setup header styling and add debug column if needed
        // also update layout info with header widths and column spacing
        self.headers.set_style_all(Style::new().bold().yellow());
        if self.debug {
            self.headers.add_item("Seq");
        }
        {
            let mut layout = self.layout.write();
            layout.tbl_hdr_widths = self.headers.widths();
            layout.tbl_colspacing = 2;
        }

        self.ui_interval = Duration::from_millis(conf.refresh);
        self.ping_interval = conf.interval;
        self.ping_timeout = conf.timeout;
        if conf.size as usize != DEFAULT_PAYLOAD_SIZE {
            self.payload = vec![0u8; conf.size as usize].into();
        }

        // adjust internal tick (delay) lower if it's higher than ping
        // interval, othwerwise we'd send out fewer pings than intended
        self.internal_tick = self.internal_tick.min(conf.interval);

        // IPv4 & IPv6 clients
        self.c_v4 = if conf.addrs.iter().any(|a: &IpAddr| a.is_ipv4()) {
            match Client::new(&Config::default()) {
                Ok(c) => Arc::new(c).into(),
                Err(e) => return Err(nice_permission_error(&e, "v4")),
            }
        } else {
            None
        };
        self.c_v6 = if conf.addrs.iter().any(|a: &IpAddr| a.is_ipv6()) {
            match Client::new(&Config::builder().kind(ICMP::V6).build()) {
                Ok(c) => Arc::new(c).into(),
                Err(e) => return Err(nice_permission_error(&e, "v6")),
            }
        } else {
            None
        };

        self.add_targets(targets);
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

    /// Schedule the next UI refresh tick.
    pub fn ui_schedule_next_refresh(&self) {
        *self.ui_next_refresh.write() += self.ui_interval;
    }

    /// Whether it's time for the next UI refresh.
    pub async fn ui_refresh_elapsed_async(&self) -> bool {
        tokio::time::Instant::now() >= *self.ui_next_refresh.read()
    }

    /// Add new ping targets to the application state.
    pub fn add_targets<I: IntoIterator<Item = PingTarget>>(&self, targets: I) {
        self.targets
            .write()
            .extend(targets.into_iter().map(|t| Arc::new(t)));
    }

    /// Pause pinging for the target at the specified index.
    pub fn toggle_target_pause(&self, index: usize) {
        if let Some(tgt) = self.targets.read().get(index) {
            tgt.toggle_pause();
        }
    }

    /// Pause pinging for all targets.
    pub fn pause_all_targets(&self) {
        for tgt in self.targets.read().iter() {
            tgt.pause();
        }
    }

    /// Resume pinging for all targets.
    pub fn resume_all_targets(&self) {
        for tgt in self.targets.read().iter() {
            tgt.resume();
        }
    }

    /// Update information for the target at the specified index.
    pub fn update_target_info(&self, index: usize) {
        if let Some(tgt) = self.targets.read().get(index) {
            tgt.determine_hops(Duration::from_secs(2));
        }
    }

    /// Reset statistics for the target at the specified index.
    pub fn reset_target_stats(&self, index: usize) {
        if let Some(tgt) = self.targets.read().get(index) {
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
            tgt.stop();
        }
    }

    /// Stop pinging the target at the specified index and remove it from the target list.
    pub fn remove_target(&self, index: usize) {
        let mut targets = self.targets.write();
        if let Some(tgt) = targets.get(index) {
            tgt.stop();
            targets.remove(index);
        }
    }

    /// Remove all unreachable targets and return the count if any were nuked.
    pub fn remove_all_unreachables(&self) -> usize {
        let mut targets = self.targets.write();
        let orig_len: usize = targets.len();
        targets.retain(|tgt: &Arc<PingTarget>| match tgt.is_unreachable() {
            true => {
                tgt.stop();
                false
            }
            false => true,
        });
        orig_len.saturating_sub(targets.len())
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            pi: ProcessInfo::new().with_min_interval(PROCINFO_INTERVAL),
            c_v4: None,
            c_v6: None,
            targets: vec![].into(),
            tasks: vec![].into(),
            layout: AppLayout::default().into(),
            title: Line::from(APP_TITLE),
            headers: TableRow::from_iter(HEADERS),
            ui_interval: Duration::from_millis(250),
            ui_next_refresh: tokio::time::Instant::now().into(),
            verbose: false,
            debug: false,
            quit: AtomicBool::new(false).into(),
            ping_interval: Duration::from_secs(1),
            ping_timeout: Duration::from_secs(2),
            randomize: false,
            payload: vec![0u8; DEFAULT_PAYLOAD_SIZE].into(), // 48 bytes -> 56-byte packet
            internal_tick: Duration::from_millis(100),       // 10 Hz default, might be overridden
            key_event: Notify::new(),
            status_line: MutableLine::new_from(""),
            popup_contents: None.into(),
            spawned_tasks: AtomicU64::new(0),
            perf: AtomicBool::new(false),
        }
    }
}

////////////////////////////////////////////////////////////////////////////////

/// Contents for popup dialog in the UI.
#[derive(Debug)]
pub(crate) enum PopupContents {
    Table(Vec<String>),
    Paragraph(String),
    Line(String),
}

impl PopupContents {
    pub fn to_para(&self) -> Paragraph<'static> {
        match self {
            PopupContents::Paragraph(s) | PopupContents::Line(s) => Paragraph::new(s.clone()),
            PopupContents::Table(s) => Paragraph::new(s.join("\n")),
        }
    }
}

////////////////////////////////////////////////////////////////////////////////

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
}

impl Display for QueryResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueryResponse::IpAddr(ip) => write!(f, "{ip}"),
            QueryResponse::Count(c) => write!(f, "{c}"),
            QueryResponse::Float(v) => write!(f, "{v}"),
            QueryResponse::Text(s) => write!(f, "{s}"),
            QueryResponse::Error(e) => write!(f, "Error: {e}"),
            QueryResponse::Empty => write!(f, "<empty response>"),
            QueryResponse::None => write!(f, "<unknown>"),
        }
    }
}
