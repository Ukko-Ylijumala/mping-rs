// Copyright (c) 2025-2026 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

pub static APP_NAME: &str = "mping";
pub static APP_TITLE: &str = "mping - Multi-pinger";
pub static HEADERS: [&str; 10] = ["Address", "Sent", "Recv", "Loss", "Last", "Mean", "Min", "Max", "Stdev", "Status"];
pub static HDR_SEQ: &str = "Seq";

// missing value placeholder for textual output
pub static MISSING: &str = "-";

// glyphs
pub static CHECK_OK: &str = "✅";
pub static CHECK_NOK: &str = "❌";
pub static CHECK_EMPTY: &str = "⬜";
pub static EXCLAMATION: &str = "⚠️";
pub static QUESTION: &str = "❓";
pub static ARROW_UP: &str = "↑";
pub static ARROW_DOWN: &str = "↓";
pub static ARROW_RIGHT: &str = "→";
pub static ARROW_LEFT: &str = "←";
pub static TABULATOR: &str = "⇥ ⇤";

// generic
pub static TIMEOUT: &str = "timeout";
pub static UNAVAIL: &str = "unavailable";
pub static UNKNOWN: &str = "unknown";
pub static UNREACH: &str = "unreach";
pub static PAUSED: &str = "paused";
pub static RESUMED: &str = "resumed";
pub static STOPPED: &str = "stopped";
pub static ENABLED: &str = "enabled";
pub static DISABLED: &str = "disabled";

// main.rs
pub static ERR_V4_MISSING: &str = "IPv4 client missing";
pub static ERR_V6_MISSING: &str = "IPv6 client missing";
pub static ERR_KEV_JOIN: &str = "joining key event handler thread failed";
pub static APP_RUNNING: &str = " mping initialized. Press 'q' to quit, 'F1' for help.";
pub static INFO_QUITTING: &str = "main thread quitting. Waiting for tasks to terminate...";
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
pub static INFO_LOG: &str = " Log messages: {} ";
pub static INFO_HELP: &str = " Help ";
pub static INPUT_TITLE: &str = " Add target(s) ";
pub static INPUT_ADDRS: &str = " Addresses ";
pub static INPUT_EXCLS: &str = " Exclusions (optional) ";
pub static INPUT_PAUSED: &str = " Start paused";
pub static INPUT_SUBMIT: &str = " Submit ";
pub static INPUT_CANCEL: &str = " Cancel ";
pub static INPUT_WORKING: &str = "Resolving — please wait...";
pub static INPUT_HINT_CLOSE: &str =
    "[Esc to close, or type more addresses to add another batch. F12 for full log.]";
pub static INPUT_NOTHING: &str = "Nothing actionable in this submission.";
pub static SUMMARY_ADDED: &str = "added";
pub static SUMMARY_SKIPPED: &str = "skipped (already pinged)";
pub static SUMMARY_RESOLVED: &str = "resolved";
pub static SUMMARY_UNRESOLVED: &str = "unresolved";
pub static SUMMARY_EXCLUDED: &str = "excluded";
pub static SUMMARY_AND_MORE: &str = "…and {} more";

// tui.rs
pub static TUI_INIT: &str = "TerminalGuard: initializing terminal UI. Display refresh rate";
pub static TUI_TERMINAL: &str = "TerminalGuard: alternative screen entered. Initializing Ratatui.";
pub static TUI_TERMINATE: &str = "Terminal UI was terminated.";
pub static APP_PANIC: &str = "Application panic";

// keyboard.rs
pub static KEV_START: &str = "key event handler thread started";

// pingdata.rs
pub static ERR_PTR_EMPTY: &str = "PTR record empty";
pub static WARN_PTR_MANY: &str = "Multiple PTRs";
pub static ERR_PTR_FAILED: &str = "PTR lookup failed";
pub static ERR_NO_RESP: &str = "No Response";
pub static ERR_NORECORDS: &str = "No records";
pub static ERR_NOTENOUGH: &str = "Not enough records";
pub static ERR_NO_RTT: &str = "Could not find RTT";
pub static ERR_LARGE_WIN: &str = "Window size exceeds data length";
pub static INFO_LOCAL: &str = "local (a few km max)";
pub static INFO_NEARBY: &str = "same city (< 30 km)";
pub static INFO_INTERPLANETARY: &str = "outside of atmosphere";

// utils.rs
pub static ERR_SOCKETS: &str = "cannot create raw sockets for ICMPv";
pub static ERR_CLIENT: &str = "failed to create client for ICMPv";
pub static ERR_SIGNALS: &str = "setting up signal handlers failed";
pub static GOT_SIGINT: &str = "Received SIGINT (Ctrl-C), shutting down...";
pub static GOT_SIGTERM: &str = "Received SIGTERM (kill -15), shutting down...";
pub static GOT_SIGQUIT: &str = "Received SIGQUIT (Ctrl-\\), shutting down...";
pub static ERR_TIMEVAL: &str = "invalid time value";
pub static ERR_PARSE_IP: &str = "parsing failed";
pub static WARN_NO_MATCHES: &str = "exclusions did not match any addresses.";
pub static WARN_ALL_EXCLUDED: &str = "all target addresses were excluded.";
pub static INFO_EXPANDED: &str = "number of addresses expanded from";
pub static INFO_EXCLUDE: &str = "excluding addresses from target list";
pub static INFO_RESOLVE_ONE: &str = "number of addresses resolved from";
pub static ERR_RESOLVE: &str = "failed to resolve";
pub static PTR_IPV4: &str = ".in-addr.arpa";
pub static PTR_IPV6: &str = ".ip6.arpa";

// hopcount.rs
pub static BIND_SOCKET_IPV4: &str = "0.0.0.0:0";
pub static BIND_SOCKET_IPV6: &str = "[::]:0";
pub static ERR_PACKET: &str = "Failed to create echo packet";
pub static ERR_CKSUM: &str = "Failed to create IcmpPacket for checksumming";
pub static ERR_SOCK_RAW: &str = "Failed to create raw socket";
pub static ERR_SOCK_TIMEOUT: &str = "Failed to set socket timeout";
pub static ERR_SOCK_BIND: &str = "Bind to socket failed";
pub static ERR_SEND: &str = "Send failed";
pub static ERR_RECV: &str = "Receive failed";
pub static ERR_MALFORMED: &str = "Malformed ICMP packet";
pub static ERR_UNREACH: &str = "Destination Unreachable";
pub static ERR_ICMPTYPE: &str = "Wanted Echo Reply (0), got";
pub static ERR_HDR_IPV4: &str = "Truncated IPv4 header";
pub static ERR_HDR_IPV6: &str = "Truncated IPv6 header";
pub static INFO_SOCKET: &str = "Raw socket created for ICMPv";
pub static INFO_SEND: &str = "Sending ICMP Echo Request to";

// args.rs
pub static NUM: &str = "NUM";
pub static SECS: &str = "SECS";
pub static BYTES: &str = "BYTES";
pub static FLOAT: &str = "FLOAT";
pub static IP_LIST: &str = "IP1 [IP2...]";
pub static IP_LIST_COMMA: &str = "IP1[,IP2...]";
pub static WARN_NO_VALID_IPS: &str = "No valid IP addresses provided.";
pub static INFO_RESOLVED: &str = "number of new addresses resolved";
pub static INFO_UNIQUE: &str = "total unique addresses to monitor";
pub static INFO_ADJUST: &str = "adjusting timeout to avoid excessive concurrent pings";
pub static INFO_DNS: &str = "using system DNS configuration";
pub static INFO_DNS_TIMEO: &str = "setting custom DNS timeout";
pub static INFO_DNS_CUSTOM: &str = "using custom DNS server(s)";

// Clap cmdline args help texts
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

// capabilities error messages
pub static ERR_CAPS: &str = "This program requires raw sockets to send ICMP packets.";
pub static ERR_CAPS_LINUX: &str = "Either run it with sudo, or grant the CAP_NET_RAW capability to the binary:
    sudo setcap cap_net_raw+ep";
pub static ERR_CAPS_V4: &str = "\nFor IPv4 only you can also allow group IDs system-wide (less secure):
    sudo sysctl -w net.ipv4.ping_group_range=\"<start> <end>\"\n";
pub static ERR_CAPS_MACOS: &str = "Run it with sudo - raw ICMP sockets on macOS require root.";

// structs.rs
pub static INFO_STATE_INIT: &str = "application state initialized";
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
pub static EMPTY_RESP: &str = "empty response";
pub static ADD_TGT_DIALOG: &str = "Add targets and exclusions (space separated; Esc to cancel)";
pub static ADD_TGT_DIA_ADDRS: &str = "IP addrs/ranges/CIDRs:";
pub static ADD_TGT_DIA_EXCLS: &str = "Exclusions (optional):";
pub static ADD_TGT_DIA_PAUSE: &str = "Add as paused";

// App keybindings help text
pub static HELP_HDRS: [&str; 2] = ["Key(s)", "Action"];
pub static HELP_KEYS: [[&str; 2]; 24] = [
    ["Up, Down",       "Scroll target list up/down"],
    ["Left, Right",    "Scroll table columns left/right"],
    ["PageUp, PageDn", "Page target list up/down (with shift: 10 lines)"],
    ["",               "(if popup is visible, page its content up/down instead)"],
    ["Home, End",      "Jump to top/bottom of target list"],
    ["Backspace",      "Clear row and column selections"],
    ["",                ""],
    ["<space>",        "Toggle pause/resume pinging for selected target"],
    ["Enter",          "Update the selected target's details in the info panel"],
    ["R",              "Reset ping statistics for selected target"],
    ["p",              "Pause pinging for all targets"],
    ["P",              "Resume pinging for all targets"],
    ["a",              "Open the add target dialog"],
    ["",                ""],
    ["S",              "Stop pinging the selected target permanently"],
    ["Delete",         "Stop pinging the selected target and remove it"],
    ["CTRL+Delete",    "Stop and remove all unreachable targets"],
    ["",               ""],
    ["Esc",            "Close popups (help, messages) or input box"],
    ["q, CTRL-C",      "Quit the program"],
    ["",                ""],
    ["F1",             "Show/hide this help screen"],
    ["F10",            "Toggle \"perf\" mode (reduce task spawn overhead)"],
    ["F12",            "Display application log message buffer in a popup"],
];
