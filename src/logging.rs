// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::macros::eprintln_nomangle;
use parking_lot::RwLock;
use ratatui::{
    style::Stylize,
    text::{Line, Span},
    widgets::{List, Paragraph},
};
use std::{
    collections::VecDeque,
    fmt::{self, Display},
    ops::Deref,
    sync::atomic::{AtomicU32, Ordering},
};
use timesince::TimeSinceEpoch;

/**
Log level for messages. Follows Linux syslog convention, but
adds "trace" at 15. Default is "info". "Emergency" and "alert"
are expected to not be used, as we should not be system critical.
*/
#[derive(Debug, Default, Clone, PartialEq, Eq, PartialOrd)]
pub enum LogLevel {
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
    #[rustfmt::skip]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s: &str = match self {
            LogLevel::Emergency => "EMERG",
            LogLevel::Alert     => "ALERT",
            LogLevel::Critical  => "CRIT",
            LogLevel::Error     => "ERROR",
            LogLevel::Warn      => "WARN",
            LogLevel::Notice    => "NOTE",
            LogLevel::Info      => "INFO",
            LogLevel::Debug     => "DEBUG",
            LogLevel::Trace     => "TRACE",
        };
        write!(f, "{s}")
    }
}

/* ---------------------------------- */

// Function name => Level variant mapper.
#[rustfmt::skip]
macro_rules! level_variant {
    (crit)   => { LogLevel::Critical };
    (error)  => { LogLevel::Error };
    (warn)   => { LogLevel::Warn };
    (notice) => { LogLevel::Notice };
    (info)   => { LogLevel::Info };
    (debug)  => { LogLevel::Debug };
    (trace)  => { LogLevel::Trace };
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
            /// Log a message at specified level and return its representation.
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
pub struct Message {
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

    /// Return the message as a styled [Line] for displaying with Ratatui.
    /// The styling depends on the log level.
    #[inline]
    pub fn as_line(&self) -> Line<'static> {
        // Clone the string so the Span owns it and doesn't borrow from self, allowing
        // the produced Line to outlive any read lock used to access the message.
        let msg = Span::raw(self.msg.clone());
        Line::from_iter([
            Span::raw(format!("{} {}: ", self.when, self.lvl)),
            match self.lvl {
                LogLevel::Critical => msg.bold().on_light_red(),
                LogLevel::Error => msg.bold().light_red(),
                LogLevel::Warn => msg.light_yellow(),
                LogLevel::Notice => msg.cyan(),
                LogLevel::Debug => msg.dim(),
                LogLevel::Trace => msg.dim().italic(),
                _ => msg,
            },
        ])
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

/**
A fixed-size buffer for storing recent in-app (log) messages with timestamps.
These can be displayed in a popup window in the UI if desired f.ex.

Log level `to_stderr` controls which messages (with same or higher priority)
get printed to stderr when added (if TUI alternate screen is not active).
Default is [LogLevel::Notice].
*/
#[derive(Debug)]
pub struct MessageBuffer {
    buf: RwLock<VecDeque<Message>>,
    cap: usize,
    /// Total number of messages ever added to the buffer.
    total: AtomicU32,
    to_stderr: LogLevel,
    /// Styled [Line]s for `to_list`, cached with the `total` count they were
    /// built at. Rendering the log popup calls `to_list` on every frame;
    /// without this every frame would re-format and re-style the full buffer.
    line_cache: RwLock<(u32, Vec<Line<'static>>)>,
}

impl MessageBuffer {
    pub fn new(capacity: usize, to_stderr: LogLevel) -> Self {
        Self {
            buf: VecDeque::with_capacity(capacity).into(),
            cap: capacity,
            total: AtomicU32::new(0),
            to_stderr,
            line_cache: RwLock::new((0, Vec::new())),
        }
    }

    /// Set the log level threshold for printing messages to stderr.
    pub fn to_stderr(&mut self, lvl: LogLevel) {
        self.to_stderr = lvl;
    }

    /// Internal - add a new message to the buffer.
    #[inline]
    fn add(&self, msg: &Message) {
        {
            let mut buf = self.buf.write();
            if buf.len() >= self.cap {
                buf.pop_front();
            }
            buf.push_back(msg.clone());
        }
        self.total.fetch_add(1, Ordering::Relaxed);
        // stderr I/O can block (e.g. a full pipe); do it outside the lock
        // so it can't stall every other logger caller.
        if msg.lvl <= self.to_stderr {
            eprintln_nomangle!("{}", msg.as_timestamped());
        }
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
    /// Each message is styled according to its log level.
    ///
    /// The styled lines are cached and only rebuilt when `total` has changed
    /// since the last call; an unchanged buffer costs just the [Line] clones.
    /// (A push between the `total` load and the buffer read can serve one
    /// frame of slightly stale lines — the next frame rebuilds. Fine for
    /// display purposes.)
    pub fn to_list(&self) -> List<'_> {
        let total: u32 = self.total.load(Ordering::Relaxed);
        {
            let cache = self.line_cache.read();
            if cache.0 == total && !cache.1.is_empty() {
                return List::new(cache.1.clone());
            }
        }

        let lines: Vec<Line<'static>> =
            self.with(|msgs| msgs.iter().map(|m| m.as_line()).collect());
        *self.line_cache.write() = (total, lines.clone());
        List::new(lines)
    }
}

impl Clone for MessageBuffer {
    fn clone(&self) -> Self {
        Self {
            buf: self.buf.read().clone().into(),
            cap: self.cap,
            total: AtomicU32::new(self.total.load(Ordering::Relaxed)),
            to_stderr: self.to_stderr.clone(),
            // start cold; the clone rebuilds its own cache on first to_list
            line_cache: RwLock::new((0, Vec::new())),
        }
    }
}

impl Default for MessageBuffer {
    fn default() -> Self {
        Self::new(1024, LogLevel::Notice)
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
