// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{ip_addresses::parse_ip_or_range, strings::*};
use signal_hook::{
    consts::signal::{SIGINT, SIGQUIT, SIGTERM},
    iterator::Signals,
};
use std::{
    collections::HashSet,
    env,
    io::{
        Error,
        ErrorKind::{Other, PermissionDenied},
    },
    net::IpAddr,
    path::{MAIN_SEPARATOR, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// Set up handlers for various termination signals.
///
/// Currently we handle:
///   - [SIGINT] - `Ctrl-C`
///   - [SIGTERM] - `kill -15` from shell or systemd etc
///   - [SIGQUIT] - `Ctrl-\`. This normally creates a core dump, but here we just exit cleanly.
///
/// NOTE: some (many? most?) console emulators do not process SIGINT when in raw mode,
/// hence Ctrl-C might need to be handled manually in a key event loop instead.
pub(crate) fn setup_signal_handler(quit: Arc<AtomicBool>) {
    // Signals to listen for
    let mut signals = Signals::new(&[SIGINT, SIGTERM, SIGQUIT]).expect(ERR_SIGNALS);

    // Spawn a dedicated thread that listens for signals.
    std::thread::spawn(move || {
        for sig in signals.forever() {
            match sig {
                SIGINT => eprintln!("{GOT_SIGINT}"),
                SIGTERM => eprintln!("{GOT_SIGTERM}"),
                SIGQUIT => eprintln!("{GOT_SIGQUIT}"),
                _ => {}
            }

            // Tell the rest of the program to exit.
            quit.store(true, Ordering::Relaxed);
        }
    });
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

        eprintln!("{INFO_CAPS} {}", bin_path.display());
        if ip_ver == "v4" {
            eprintln!("{INFO_CAPS_V4}");
        }
        Box::new(Error::new(
            PermissionDenied,
            format!("{ERR_SOCKETS}{ip_ver}"),
        ))
    } else {
        // other error -> let it bubble up normally
        Box::new(Error::new(Other, format!("{ERR_CLIENT}{ip_ver}: {err}")))
    }
}

/// Parse a floating point number into a Duration.
pub(crate) fn parse_float_into_duration(arg: &str) -> Result<Duration, String> {
    match arg.parse::<f64>() {
        Ok(secs) if secs > 0.0 => {
            let millis = (secs * 1e3).round() as u64;
            Ok(Duration::from_millis(millis))
        }
        _ => Err(format!("{ERR_TIMEVAL}: {arg}")),
    }
}

/// Parse and expand a list of space separated IPv4/v6 addresses
/// (single, range, CIDR), taking exclusions into account if applicable.
///
/// Removes duplicates and preserves order of first occurrence.
pub(crate) fn parse_ip_addresses(
    targets: &[String],
    exclude: Option<&[String]>,
    verbose: bool,
) -> Vec<IpAddr> {
    let mut all_addrs: Vec<IpAddr> = Vec::new();
    let mut seen: HashSet<IpAddr> = HashSet::new();

    // Parse all targets and expand them into individual IPs
    for target in targets {
        match parse_ip_or_range(target) {
            Ok(mut ips) => {
                if verbose && ips.len() > 1 {
                    eprintln!("Expanded '{target}' to {} addresses", ips.len());
                }
                all_addrs.append(&mut ips);
            }
            Err(e) => {
                eprintln!("{ERR_PARSE_IP} '{target}': {e}");
            }
        }
    }

    // Remove duplicates while preserving order
    all_addrs.retain(|ip: &IpAddr| seen.insert(*ip));

    // Parse exclusions and expand them into individual IPs
    if let Some(exclude) = exclude {
        let mut exclusions: HashSet<IpAddr> = HashSet::new();

        for exc in exclude {
            match parse_ip_or_range(exc) {
                Ok(mut ips) => {
                    if verbose && ips.len() > 1 {
                        eprintln!("Expanded '{exc}' to {} addresses (exclusion)", ips.len());
                    }
                    exclusions.extend(ips.drain(..));
                }
                Err(e) => {
                    eprintln!("{ERR_PARSE_IP} '{exc}' (exclusion): {e}");
                }
            }
        }

        // Apply exclusions if needed
        if !exclusions.is_empty() {
            // let's see if we actually exclude anything
            let remainder: HashSet<IpAddr> = &seen - &exclusions;
            if remainder == seen {
                eprintln!("{WARN_NO_MATCHES}");
            } else if remainder.is_empty() {
                eprintln!("{ERR_ALL_EXCLUDED}");
            } else {
                if verbose {
                    eprintln!("{INFO_EXCLUDE}: {}", (seen.len() - remainder.len()));
                }
                all_addrs.retain(|ip: &IpAddr| !exclusions.contains(ip));
            }
        };
    }

    all_addrs
}
