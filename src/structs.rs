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
use hickory_resolver::TokioResolver;
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
    pub resolver: Arc<TokioResolver>,
    pub distance_stretch_factor: f64,
    runtime: tokio::runtime::Handle,
    spawned_tasks: AtomicU64,
    perf: AtomicBool,
}

impl AppState {
    /**
    Initialize base application state from the commandline options. Sets up:
    - title, headers and styling
    - initial [AppLayout]
    - [ProcessInfo] tracking
    - intervals: UI refresh, pinging, internal tick
    - flags: verbose, debug, randomize, perf
    - events: key event notifier, quit flag
    - default payload
    - other default fields
    */
    pub fn from_conf(conf: &MpConfig) -> Self {
        // setup app title row with version and styling
        let mut title = Line::from(APP_TITLE).centered().bold().red().on_green();
        title.push_span(format!(" v{}", conf.ver));

        // setup header styling and add debug column if needed
        let mut headers = TableRow::from_iter(HEADERS);
        headers.set_style_all(Style::new().bold().yellow());
        if conf.debug {
            headers.add_item("Seq");
        }

        // update layout info with header widths and column spacing
        let mut layout: AppLayout = AppLayout::default();
        layout.tbl_hdr_widths = headers.widths();
        layout.tbl_colspacing = 2;

        Self {
            pi: ProcessInfo::new().with_min_interval(PROCINFO_INTERVAL),
            c_v4: None,
            c_v6: None,
            targets: vec![].into(),
            tasks: vec![].into(),
            layout: layout.into(),
            title,
            headers,
            ui_interval: Duration::from_millis(conf.refresh),
            ui_next_refresh: tokio::time::Instant::now().into(),
            verbose: conf.verbose,
            debug: conf.debug,
            quit: AtomicBool::new(false).into(),
            ping_interval: conf.interval,
            ping_timeout: conf.timeout,
            randomize: conf.randomize,
            payload: vec![0u8; conf.size as usize].into(), // 48 bytes -> 56-byte packet
            // adjust internal tick (delay) lower if it's higher than ping
            // interval, othwerwise we'd send out fewer pings than intended
            internal_tick: Duration::from_millis(100).min(conf.interval),
            key_event: Notify::new(),
            status_line: MutableLine::new_from(""),
            popup_contents: None.into(),
            resolver: conf.resolver.as_ref().unwrap().clone(),
            distance_stretch_factor: conf.stretch_factor,
            runtime: tokio::runtime::Handle::current(),
            spawned_tasks: AtomicU64::new(0),
            perf: conf.perf.into(),
        }
    }

    /**
    Build the final application state from initial targets.
    - set up [surge_ping::Client] instances for IPv4 and IPv6
    - add provided ping targets (if any)

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
                Err(e) => return Err(nice_permission_error(&e, "v4")),
            }
        };
        self.c_v6 = {
            match Client::new(&Config::builder().kind(ICMP::V6).build()) {
                Ok(c) => Arc::new(c).into(),
                Err(e) => return Err(nice_permission_error(&e, "v6")),
            }
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

    /// Update information for the target at the specified index. Nonblocking.
    pub fn update_target_info(&self, index: usize) {
        if let Some(tgt) = self.targets.read().get(index) {
            let tgt_ptr1 = tgt.clone();
            let tgt_ptr2 = tgt.clone();
            let resolver = self.resolver.clone();
            // `determine_hops` is blocking, so spawn a thread for it to not block the caller.
            //
            // NOTE: we specifically can't use `tokio::spawn` here because `determine_hops`
            // will eventually acquire a write lock to one/some of its fields, and there's a
            // very good change that the caller will deadlock (or panic) on the same if it's
            // scheduled in the same runtime thread.
            std::thread::spawn(move || tgt_ptr1.determine_hops(UPDATE_TASK_TIMEOUT));
            self.inc_spawned_tasks();

            // Because this function can be called from the keyboard event handler thread,
            // which is not inside the tokio runtime context, we must use the stored
            // runtime handle to spawn the task, or we will panic that thread.
            self.runtime
                .spawn(async move { tgt_ptr2.resolve_ptr(&resolver).await });
            self.inc_spawned_tasks();
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
            QueryResponse::Error(e) => write!(f, "E: {e}"),
            QueryResponse::Empty => write!(f, "<empty response>"),
            QueryResponse::None => write!(f, "<unknown>"),
        }
    }
}
