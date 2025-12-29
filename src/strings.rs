// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

pub static APP_TITLE: &str = "mping - Multi-pinger";
pub static HEADERS: [&str; 10] = ["Address", "Sent", "Recv", "Loss", "Last", "Mean", "Min", "Max", "Stdev", "Status"];

// missing value placeholder for textual output
pub static MISSING: &str = "-";

// main.rs
pub static ERR_V4_MISSING: &str = "IPv4 client missing";
pub static ERR_V6_MISSING: &str = "IPv6 client missing";
pub static ERR_KEV_JOIN: &str = "Error joining key event handler thread";
pub static APP_RUNNING: &str = " mping initialized. Press 'q' to quit, 'h' for help.";
pub static INFO_QUITTING: &str = "Main thread quitting. Waiting for tasks to terminate...";
pub static INFO_INFO: &str = " Info ";
pub static INFO_TGTS: &str = " Targets: {} ";
pub static INFO_NO_TGTS: &str = " No targets";
pub static INFO_TARGET: &str = " Target  : {}\n Reverse : {}\n PTR     : {}\n rev-PTR : {}\n Distance: {}\n Hops    : {}";
pub static INFO_SELECT: &str = " Select a target to see detailed info.";
pub static INFO_RTT_G: &str = " Round-Trip Time graph ";
pub static INFO_RTT_H: &str = " RTT Histogram (ms) ";
pub static INFO_RTT: &str = "RTT (ms)";
pub static INFO_NOW: &str = "Now";
pub static INFO_NO_RTT: &str = " No RTT data available.";
pub static INFO_STATE: &str = "Interval: {} ms\nTimeout : {} ms\nPayload : {} bytes{}\nTasks   : {}";
pub static INFO_RAND: &str = " (randomized)";
pub static INFO_CPU: &str = "CPU: {} | mem: {} | pid: {} ";
pub static INFO_DEBUG: &str = " Data: {}, offset: {}, idx: {}";

// tui.rs
pub static TUI_INIT: &str = "Initializing terminal UI. Display refresh rate";
pub static TUI_TERMINATE: &str = "Terminal UI was terminated.";
pub static APP_PANIC: &str = "Application panic";

// pingdata.rs
pub static ERR_NO_RESP: &str = "No Response";
pub static ERR_NORECORDS: &str = "No records";
pub static ERR_NOTENOUGH: &str = "Not enough records";
pub static ERR_NO_RTT: &str = "Could not find RTT";
pub static ERR_LARGE_WIN: &str = "Window size exceeds data length";

// utils.rs
pub static ERR_SOCKETS: &str = "ERROR: no permissions to create raw sockets for ICMP";
pub static ERR_CLIENT: &str = "ERROR: failed to create client for ICMP";
pub static ERR_SIGNALS: &str = "ERROR: setting up signal handlers failed";
pub static GOT_SIGINT: &str = "Received SIGINT (Ctrl-C), shutting down...";
pub static GOT_SIGTERM: &str = "Received SIGTERM (kill -15), shutting down...";
pub static GOT_SIGQUIT: &str = "Received SIGQUIT (Ctrl-\\), shutting down...";
pub static ERR_TIMEVAL: &str = "ERROR: invalid time value";
pub static ERR_PARSE_IP: &str = "ERROR: parsing failed for address";
pub static WARN_NO_MATCHES: &str = "WARN: exclusions did not match any addresses.";
pub static WARN_ALL_EXCLUDED: &str = "WARN: all target addresses were excluded.";
pub static INFO_EXPANDED: &str = "Number of addresses expanded from";
pub static INFO_EXCLUDE: &str = "Excluding addresses from target list";
pub static PTR_IPV4: &str = ".in-addr.arpa";
pub static PTR_IPV6: &str = ".ip6.arpa";

// args.rs
pub static ERR_RESOLVE: &str = "ERROR: failed to resolve hostname";
pub static WARN_NO_VALID_IPS: &str = "WARN: no valid IP addresses provided.";
pub static INFO_RESOLVE_ONE: &str = "Number of addresses resolved from hostname";
pub static INFO_RESOLVED: &str = "Number of new addresses resolved from hostnames";
pub static INFO_UNIQUE: &str = "Total unique addresses to monitor";
pub static INFO_ADJUST: &str = "Adjusting timeout to avoid excessive concurrent pings";

pub static HELP_TARGETS: &str = "Space separated list of IP addresses or ranges to monitor";
pub static HELP_EXCLUDE: &str = "Comma separated IP addresses (and/or ranges) to exclude";
pub static HELP_INTERVAL: &str = "Interval between pings to each target [0.01 - 10]";
pub static HELP_TIMEOUT: &str = "Timeout for each ping request [0.01 - 5]";
pub static HELP_SIZE: &str = "Size of ICMP payload (minus the 8-byte ICMP header) [32 - 32760]";
pub static HELP_RANDOMIZE: &str = "Randomize ICMP payload data [default: no]";
pub static HELP_HISTSIZE: &str = "Full history size (number of ping results to keep per target) [60 - 65536]";
pub static HELP_DETAILED: &str = "Detailed recent history size (for laggy/flappy detection etc) [10 - 1000]";
pub static HELP_PAUSED: &str = "Start with pinging paused for all targets [default: no]";
pub static HELP_REFRESH: &str = "TUI refresh interval in milliseconds [50 - 5000]";
pub static HELP_DNS_SERVERS: &str = "Comma separated list of DNS servers to use instead of system defaults";
pub static HELP_DNS_TIMEOUT: &str = "DNS resolution timeout in seconds [1 - 10] [default: 5]";
pub static HELP_STRETCH_FACTOR: &str = "Stretch factor for distance estimation (over 1.0 => compress distances)";
pub static HELP_PERF: &str = "Try to be more performant by reducing task spawn overhead";
pub static HELP_VERBOSE: &str = "Increase output verbosity";
pub static HELP_DEBUG: &str = "Print debug information where applicable";

pub static INFO_CAPS: &str = "This program requires CAP_NET_RAW to send ICMP packets.
Either run it with sudo, or grant the capability to the binary:
    sudo setcap cap_net_raw+ep";
pub static INFO_CAPS_V4: &str = "\nFor IPv4 only you can also allow group IDs system-wide (less secure):
    sudo sysctl -w net.ipv4.ping_group_range=\"<start> <end>\"\n";

// structs.rs
pub static INFO_PERF: &str = "performance mode {}";
pub static INFO_NEW: &str = "{} new target(s) added ({} -> {})";
pub static INFO_PING: &str = "pinging for '{}' {}";
pub static INFO_P_ALL: &str = "pausing all targets";
pub static INFO_R_ALL: &str = "resuming all targets";
pub static INFO_UPD: &str = "*** updating info for '{}' ***";
pub static INFO_HOPS: &str = "updated hops for '{}' in {}ms";
pub static INFO_PTR: &str = "resolved PTR for '{}' in {}ms";
pub static INFO_RESET: &str = "resetting statistics for '{}'";
pub static INFO_STOP: &str = "stopping '{}'";
pub static INFO_REMOVE: &str = "removing '{}'";
pub static INFO_UNR_REM: &str = "removed {} unreachable target(s) in {}ms ({} -> {})";

// App keybindings help text
pub static HELP_HDRS: [&str; 2] = ["Key(s)", "Action"];
pub static HELP_KEYS: [[&str; 2]; 17] = [
    ["q, CTRL-C",      "Quit the program"],
    ["h, F1",          "Show/hide this help screen"],
    ["Up, Down",       "Scroll target list up/down"],
    ["Left, Right",    "Scroll table columns left/right"],
    ["PageUp, PageDn", "Page target list up/down (with shift: 10 lines)"],
    ["Home, End",      "Jump to top/bottom of target list"],
    ["Backspace",      "Clear row and column selections"],
    ["<space>",        "Toggle pause/resume pinging for selected target"],
    ["Enter",          "Update the selected target's details in the info panel"],
    ["p",              "Pause pinging for all targets"],
    ["P",              "Resume pinging for all targets"],
    ["R",              "Reset ping statistics for selected target"],
    ["S",              "Stop pinging the selected target permanently"],
    ["Delete",         "Stop pinging the selected target and remove it"],
    ["CTRL+Delete",    "Stop and remove all unreachable targets"],
    ["F10",            "Toggle \"perf\" mode (reduce task spawn overhead)"],
    ["F12",            "Display application log message buffer in a popup"],
];
