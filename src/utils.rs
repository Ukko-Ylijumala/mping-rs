// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use ncurses::*;
use std::{
    env,
    io::Error,
    path::{MAIN_SEPARATOR, PathBuf},
    process,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

/// Set up the signal handler to catch Ctrl-C
pub(crate) fn setup_signal_handler(quit: Arc<AtomicBool>) {
    ctrlc::set_handler(move || {
        quit.store(true, Ordering::Relaxed);
    })
    .expect("Error setting Ctrl-C handler");
}

/// Panic handler to restore the console to a sane state
pub(crate) fn panic_handler(info: &std::panic::PanicHookInfo) {
    setup_curses(true);
    eprintln!("Application panic: {}", info);
}

/// Set up or tear down the ncurses environment
pub(crate) fn setup_curses(quit: bool) {
    match quit {
        true => {
            curs_set(CURSOR_VISIBILITY::CURSOR_VISIBLE);
            echo();
            endwin();
        }
        false => {
            eprintln!("Initializing ncurses UI...");
            initscr();
            noecho();
            curs_set(CURSOR_VISIBILITY::CURSOR_INVISIBLE);
            keypad(stdscr(), true);
            nodelay(stdscr(), true);
            clear();
        }
    }
}

/// Nicely handle permission errors when creating raw sockets.
pub(crate) fn nice_permission_error(err: &Error, ip_ver: &str) -> ! {
    let msg: String = err.to_string().to_lowercase();

    if msg.contains("permission") || msg.contains("permitted") {
        let name: String = env::args()
            .next()
            .and_then(|p| p.split(MAIN_SEPARATOR).last().map(|s| s.to_string()))
            .unwrap_or_else(|| "mping".to_string());
        let bin_path: PathBuf = env::current_exe().unwrap_or_else(|_| PathBuf::from(&name));

        eprintln!("Error: Cannot create raw ICMP{ip_ver} sockets — insufficient privileges",);
        eprintln!();
        eprintln!("This program requires CAP_NET_RAW to send ICMP packets.");
        eprintln!("Either run \"{name}\" with sudo, or grant the capability to the binary:");
        eprintln!("    sudo setcap cap_net_raw+ep {}", bin_path.display());
        if ip_ver == "v4" {
            eprintln!();
            eprintln!("For IPv4 only you can also allow group IDs system-wide (less secure):");
            eprintln!("    sudo sysctl -w net.ipv4.ping_group_range=\"<start> <end>\"");
        }
        process::exit(1);
    } else {
        // other error -> let it bubble up normally
        eprintln!("Failed to create ICMP{ip_ver} client: {err}");
        process::exit(1);
    }
}
