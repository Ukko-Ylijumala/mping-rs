// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{
    args::MpConfig,
    pingdata::PingTarget,
    strings::*,
    tabulator::simple_tabulate,
    tui::{AppLayout, MutableLine, TableRow},
    utils::nice_permission_error,
};
use hickory_resolver::TokioResolver;
use miniutils::{ProcessInfo, inject, templater};
use parking_lot::RwLock;
use ratatui::{
    prelude::Stylize,
    style::Style,
    text::Line,
    widgets::{List, Paragraph},
};
use std::{
    collections::VecDeque,
    fmt::{self, Display},
    net::IpAddr,
    ops::Deref,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use surge_ping::{Client, Config, ICMP};
use timesince::TimeSinceEpoch;
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
    pub help_contents: PopupContents,
    pub popup_contents: RwLock<PopupContents>,
    pub resolver: Arc<TokioResolver>,
    pub logger: Arc<MessageBuffer>,
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
            headers.add_item(HDR_SEQ);
        }

        // update layout info with header widths and column spacing
        let mut layout: AppLayout = AppLayout::default().widths(headers.widths()).spacing(2);
        layout.reset_table_widths();

        // prepare help contents and update layout accordingly
        let help: Vec<String> = simple_tabulate(HELP_KEYS, Some(&HELP_HDRS));
        let max_width: usize = help.iter().map(|s| s.len()).max().unwrap();
        layout.setup_help_area(help.len() as u16, max_width as u16);

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
            resolver: conf.resolver.as_ref().unwrap().clone(),
            logger: conf.buf.clone(),
            distance_stretch_factor: conf.stretch_factor,
            runtime: tokio::runtime::Handle::current(),
            spawned_tasks: AtomicU64::new(0),
            perf: conf.perf.into(),
            popup_contents: PopupContents::None.into(),
            help_contents: PopupContents::Multiline(help),
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
        self.logger.log(templater!(
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

    /// Schedule the next UI refresh tick.
    pub fn ui_schedule_next_refresh(&self) {
        *self.ui_next_refresh.write() += self.ui_interval;
    }

    /// Whether it's time for the next UI refresh.
    pub async fn ui_refresh_elapsed_async(&self) -> bool {
        tokio::time::Instant::now() >= *self.ui_next_refresh.read()
    }

    /**
    Get the current viewport (visibility info) of the target table as a [ViewPort].

    NOTE: locks both `layout` and `targets` for reading.
    */
    pub fn viewport(&self) -> ViewPort {
        ViewPort::new(self)
    }

    /// Add new ping targets to the application state.
    pub fn add_targets<I: IntoIterator<Item = PingTarget>>(&self, targets: I) {
        let orig_len: usize = self.len();
        self.targets
            .write()
            .extend(targets.into_iter().map(|t| Arc::new(t)));
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
            self.logger.log(templater!(
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
                logger.log(templater!(
                    INFO_HOPS,
                    tgt_ptr1,
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
                logger.log(templater!(
                    INFO_PTR,
                    tgt_ptr2,
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
                self.logger.log(templater!(INFO_STOP, tgt));
            }
            tgt.stop();
        }
    }

    /// Stop pinging the target at the specified index and remove it from the target list.
    pub fn remove_target(&self, index: usize) {
        let mut targets = self.targets.write();
        if let Some(tgt) = targets.get(index) {
            self.logger.log(templater!(INFO_REMOVE, tgt));
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
            self.logger.log(templater!(
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

/// Viewport information for the target table in the UI.
pub(crate) struct ViewPort {
    /// Total number of targets.
    pub targets: usize,
    /// Number of visible (usable) rows in the target table.
    pub rows: usize,
    /// Current offset (start index) of the visible rows.
    pub offset: usize,
    /// Current end position (exclusive) of the visible rows.
    pub end_pos: usize,
}

impl ViewPort {
    #[inline]
    pub fn new(app: &AppState) -> Self {
        let targets: usize = app.len();
        let layout = app.layout.read();
        let rows = layout.tbl_usable_rows();
        let offset: usize = layout.tablestate.offset();
        Self {
            targets,
            rows,
            offset,
            end_pos: (rows + offset).min(targets),
        }
    }

    /// Whether there are more targets than visible rows, i.e., paging is needed.
    #[inline]
    pub fn needs_paging(&self) -> bool {
        self.targets > self.rows
    }
}

/* -------------------------------------------------------------------------- */

/// Contents for popup dialog in the UI.
#[derive(Debug, Default)]
pub(crate) enum PopupContents {
    Multiline(Vec<String>),
    Paragraph(String),
    Line(String),
    Buffer(Arc<MessageBuffer>),
    #[default]
    None,
}

impl PopupContents {
    pub fn to_para(&self) -> Paragraph<'_> {
        match self {
            PopupContents::Paragraph(s) | PopupContents::Line(s) => Paragraph::new(s.clone()),
            PopupContents::Multiline(s) => Paragraph::new(s.join("\n")),
            PopupContents::Buffer(buf) => buf.to_paragraph(),
            PopupContents::None => Paragraph::new(""),
        }
    }

    pub fn to_list(&self) -> List<'_> {
        match self {
            PopupContents::Paragraph(s) => List::new(s.split("\n")),
            PopupContents::Line(s) => List::new(vec![s.clone()]),
            PopupContents::Multiline(s) => List::new(s.clone()),
            PopupContents::Buffer(buf) => buf.to_list(),
            PopupContents::None => List::new([""]),
        }
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, PopupContents::None)
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

/**
Log level for messages. Follows Linux syslog convention, but
adds "trace" at 15. Default is "info". "Emergency" and "alert"
are expected to not be used, as we should not be system critical.
*/
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) enum LogLevel {
    Emergency,
    Alert,
    Critical,
    Error,
    Warn,
    Notice,
    #[default]
    Info,
    Debug,
    Trace = 15,
}

impl Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s: &str = match self {
            LogLevel::Emergency => "EMERG",
            LogLevel::Alert => "ALERT",
            LogLevel::Critical => "CRIT",
            LogLevel::Error => "ERR",
            LogLevel::Warn => "WARN",
            LogLevel::Notice => "NOTICE",
            LogLevel::Info => "INFO",
            LogLevel::Debug => "DEBUG",
            LogLevel::Trace => "TRACE",
        };
        write!(f, "{s}")
    }
}

/* ---------------------------------- */

// Function name => Level variant mapper.
macro_rules! level_variant {
    (crit) => { LogLevel::Critical };
    (error) => { LogLevel::Error };
    (warn) => { LogLevel::Warn };
    (notice) => { LogLevel::Notice };
    (info) => { LogLevel::Info };
    (debug) => { LogLevel::Debug };
    (trace) => { LogLevel::Trace };
}

/// Template for [Message]-level methods.
macro_rules! gen_level_methods_msg {
    ($($fn:ident),+ $(,)?) => (
        $(
            #[inline]
            pub fn $fn<S: Into<String>>(msg: S) -> Message {
                Self::with_level(level_variant!($fn), msg)
            }
        )+
    );
}

/// Template for [MessageBuffer]-level methods.
macro_rules! gen_level_methods_buf {
     ($($fn:ident),+ $(,)?) => (
        $(
            #[inline]
            pub fn $fn(&self, msg: impl Into<String>) -> Message {
                self.push_level(level_variant!($fn), msg)
            }
        )+
     );
}

/// Template for [Logger] trait method blueprints.
macro_rules! gen_level_methods_trait {
    ($($fn:ident),+ $(,)?) => (
        $(
            /// Log a $fn-level message and return its representation.
            fn $fn<S: AsRef<str>>(&self, msg: S) -> String;
        )+
    );
}

/// Template for [Logger] trait method implementations.
macro_rules! gen_level_methods_impl {
    ($($fn:ident),+ $(,)?) => (
        $(
            fn $fn<S: AsRef<str>>(&self, msg: S) -> String {
                self.$fn(msg.as_ref()).as_timestamped()
            }
        )+
    );
}

/* ---------------------------------- */

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Message {
    pub when: TimeSinceEpoch,
    lvl: LogLevel,
    msg: String,
}

impl Message {
    /// Create a new info-level message with the current timestamp.
    pub fn new<S: Into<String>>(msg: S) -> Self {
        Self {
            when: TimeSinceEpoch::new(),
            lvl: LogLevel::Info,
            msg: msg.into(),
        }
    }

    /// Create a new log message with a given [LogLevel] and current timestamp.
    pub fn with_level<S: Into<String>>(lvl: LogLevel, msg: S) -> Self {
        Self {
            when: TimeSinceEpoch::new(),
            lvl,
            msg: msg.into(),
        }
    }

    // Helper methods for creating messages with specific log levels.
    gen_level_methods_msg!(crit, error, warn, notice, info, debug, trace);

    /// Return the message as a timestamped string.
    #[inline]
    pub fn as_timestamped(&self) -> String {
        format!("{} {}: {}", self.when, self.lvl, self.msg)
    }
}

impl Deref for Message {
    type Target = String;

    fn deref(&self) -> &Self::Target {
        &self.msg
    }
}

impl Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}: {}", self.when, self.lvl, self.msg)
    }
}

impl PartialOrd for Message {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.when.cmp(&other.when))
    }
}

impl Ord for Message {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.when.cmp(&other.when)
    }
}

/* ---------------------------------- */

/// A fixed-size buffer for storing recent in-app (log) messages with timestamps.
/// These can be displayed in a popup window in the UI if desired f.ex.
#[derive(Debug)]
pub(crate) struct MessageBuffer {
    buf: RwLock<VecDeque<Message>>,
    cap: usize,
    /// Total number of messages ever added to the buffer.
    total: AtomicU32,
}

impl MessageBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(capacity).into(),
            cap: capacity,
            total: AtomicU32::new(0),
        }
    }

    /// Internal - add a new message to the buffer.
    #[inline]
    fn add(&self, msg: &Message) {
        let mut buf = self.buf.write();
        if buf.len() >= self.cap {
            buf.pop_front();
        }
        buf.push_back(msg.clone());
        self.total.fetch_add(1, Ordering::Relaxed);
    }

    /// Add a new info-level message to the buffer and return a copy of it.
    pub fn push(&self, msg: impl Into<String>) -> Message {
        let msg = Message::new(msg);
        self.add(&msg);
        msg
    }

    /// Add a new message with given [LogLevel] to the buffer and return a copy of it.
    pub fn push_level(&self, lvl: LogLevel, msg: impl Into<String>) -> Message {
        let msg = Message::with_level(lvl, msg);
        self.add(&msg);
        msg
    }

    // Helper methods for adding messages with specific log levels.
    gen_level_methods_buf!(crit, error, warn, notice, info, debug, trace);

    /// Read access to the inner VecDeque via a closure.
    #[inline]
    pub fn with<R>(&self, f: impl FnOnce(&VecDeque<Message>) -> R) -> R {
        f(&*self.buf.read())
    }

    pub fn len(&self) -> usize {
        self.with(|msgs| msgs.len())
    }

    pub fn cloned(&self) -> Vec<Message> {
        self.with(|msgs| msgs.iter().cloned().collect())
    }

    pub fn iter(&self) -> impl Iterator<Item = Message> {
        self.cloned().into_iter()
    }

    /// Convert all messages to strings without timestamps.
    pub fn to_strings(&self) -> Vec<String> {
        self.with(|msgs| msgs.iter().map(|m| m.to_string()).collect())
    }

    /// Convert all messages to timestamped strings.
    pub fn to_timestamped(&self) -> Vec<String> {
        self.with(|msgs| msgs.iter().map(|m| m.as_timestamped()).collect())
    }

    /// Convert all messages (with timestamps) to a [Paragraph] for displaying with Ratatui.
    pub fn to_paragraph(&self) -> Paragraph<'_> {
        Paragraph::new(self.to_timestamped().join("\n"))
    }

    /// Convert all messages (with timestamps) to a [List] for displaying with Ratatui.
    pub fn to_list(&self) -> List<'_> {
        List::new(self.to_timestamped())
    }
}

impl Clone for MessageBuffer {
    fn clone(&self) -> Self {
        Self {
            buf: self.buf.read().clone().into(),
            cap: self.cap,
            total: AtomicU32::new(self.total.load(Ordering::Relaxed)),
        }
    }
}

impl Default for MessageBuffer {
    fn default() -> Self {
        Self::new(1024)
    }
}

/* ---------------------------------- */

/// A simple logger trait for logging messages and retrieving them.
pub trait Logger {
    /// Log a simple string message with the logger. Also return
    /// the full representation back to the caller for reuse.
    fn log<S: AsRef<str>>(&self, msg: S) -> String;

    /// Retrieve all logged messages as plain strings.
    fn messages(&self) -> Vec<String>;

    /// Retrieve all logged messages as timestamped strings.
    fn timestamped(&self) -> Vec<String>;

    /// The number of logged messages.
    fn len(&self) -> usize;

    // Define level-specific logging methods to be implemented.
    gen_level_methods_trait!(crit, error, warn, notice, info, debug, trace);
}

impl Logger for MessageBuffer {
    fn log<S: AsRef<str>>(&self, msg: S) -> String {
        self.push(msg.as_ref()).as_timestamped()
    }

    fn messages(&self) -> Vec<String> {
        self.to_strings()
    }

    fn timestamped(&self) -> Vec<String> {
        self.to_timestamped()
    }

    fn len(&self) -> usize {
        self.len()
    }

    // Implement level-specific logging methods.
    gen_level_methods_impl!(crit, error, warn, notice, info, debug, trace);
}
