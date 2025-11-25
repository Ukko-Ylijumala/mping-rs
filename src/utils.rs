// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use ncurses::*;
use signal_hook::{
    consts::signal::{SIGINT, SIGQUIT, SIGTERM},
    iterator::{Signals, SignalsInfo},
};
use std::{
    env,
    io::{
        Error,
        ErrorKind::{Other, PermissionDenied},
    },
    path::{MAIN_SEPARATOR, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

/// Set up handlers for various termination signals.
///
/// Currently we handle:
///   - [SIGINT] - `Ctrl-C`
///   - [SIGTERM] - `kill -15` from shell or systemd etc
///   - [SIGQUIT] - `Ctrl-\`. This normally creates a core dump, but here we just exit cleanly.
pub(crate) fn setup_signal_handler(quit: Arc<AtomicBool>) {
    // Signals to listen for
    let listen: [i32; 3] = [SIGINT, SIGTERM, SIGQUIT];
    let mut signals: SignalsInfo = Signals::new(&listen).expect("Error setting up signal handlers");

    // Spawn a dedicated thread that listens for signals.
    std::thread::spawn(move || {
        for sig in signals.forever() {
            match sig {
                SIGINT => eprintln!("Received SIGINT (Ctrl-C), shutting down..."),
                SIGTERM => eprintln!("Received SIGTERM (kill -15), shutting down..."),
                SIGQUIT => eprintln!("Received SIGQUIT (Ctrl-\\), shutting down..."),
                _ => {}
            }

            // Tell the rest of the program to exit.
            quit.store(true, Ordering::Relaxed);
        }
    });
}

/// Panic handler to restore the console to a sane state
pub(crate) fn panic_handler(info: &std::panic::PanicHookInfo) {
    curses_teardown(true);
    eprintln!("Application panic: {}", info);
}

/// Set up the ncurses environment
pub(crate) fn curses_setup(verbose: bool) {
    if verbose {
        eprintln!("Initializing ncurses UI...");
    }
    initscr();
    //start_color();
    //use_default_colors();
    noecho();
    curs_set(CURSOR_VISIBILITY::CURSOR_INVISIBLE);
    keypad(stdscr(), true);
    nodelay(stdscr(), true);
    clear();
}

/// Tear down the ncurses environment cleanly. Restores terminal to a sane state.
pub(crate) fn curses_teardown(verbose: bool) {
    curs_set(CURSOR_VISIBILITY::CURSOR_VISIBLE);
    echo();
    endwin();
    if verbose {
        eprintln!("nCurses UI was terminated.");
    }
}

/// Nicely handle permission errors when creating raw sockets.
pub(crate) fn nice_permission_error(err: &Error, ip_ver: &str) -> Box<dyn std::error::Error> {
    let msg: String = err.to_string().to_lowercase();

    if msg.contains("permission") || msg.contains("permitted") {
        let name: String = env::args()
            .next()
            .and_then(|p| p.split(MAIN_SEPARATOR).last().map(|s| s.to_string()))
            .unwrap_or_else(|| "mping".to_string());
        let bin_path: PathBuf = env::current_exe().unwrap_or_else(|_| PathBuf::from(&name));

        eprintln!("This program requires CAP_NET_RAW to send ICMP packets.");
        eprintln!("Either run \"{name}\" with sudo, or grant the capability to the binary:");
        eprintln!("    sudo setcap cap_net_raw+ep {}", bin_path.display());
        if ip_ver == "v4" {
            eprintln!();
            eprintln!("For IPv4 only you can also allow group IDs system-wide (less secure):");
            eprintln!("    sudo sysctl -w net.ipv4.ping_group_range=\"<start> <end>\"");
            eprintln!();
        }
        Box::new(Error::new(
            PermissionDenied,
            format!("Cannot create raw ICMP{ip_ver} sockets."),
        ))
    } else {
        // other error -> let it bubble up normally
        Box::new(Error::new(
            Other,
            format!("Failed to create ICMP{ip_ver} client: {err}"),
        ))
    }
}
