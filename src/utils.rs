// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{ip_addresses::parse_ip_or_range, logging::Logger, strings::*};
use hickory_resolver::TokioResolver;
use itertools::Itertools;
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
    vec,
};

static PERMS_STR: [&str; 2] = ["permission", "permitted"];

/**
Set up handlers for various termination signals.

Currently we handle:
  - [SIGINT] - `Ctrl-C`
  - [SIGTERM] - `kill -15` from shell or systemd etc
  - [SIGQUIT] - `Ctrl-\`. This normally creates a core dump, but here we just exit cleanly.

NOTE: some (many? most?) console emulators do not process SIGINT when in raw mode,
hence Ctrl-C might need to be handled manually in a key event loop instead.
*/
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
pub(crate) fn nice_permission_error(err: &Error, ip_ver: usize) -> Box<dyn std::error::Error> {
    let msg: String = err.to_string().to_lowercase();

    if msg.contains(PERMS_STR[0]) || msg.contains(PERMS_STR[1]) {
        let name: String = env::args()
            .next()
            .and_then(|p| p.split(MAIN_SEPARATOR).last().map(|s| s.to_string()))
            .unwrap_or_else(|| APP_NAME.to_string());
        let bin_path: PathBuf = env::current_exe().unwrap_or_else(|_| PathBuf::from(&name));

        eprintln!("{ERR_CAPS}");
        #[cfg(target_os = "linux")]
        {
            eprintln!("{ERR_CAPS_LINUX} {}", bin_path.display());
            if ip_ver == 4 {
                eprintln!("{ERR_CAPS_V4}");
            }
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
pub fn parse_float_into_duration(arg: &str) -> Result<Duration, String> {
    match arg.parse::<f64>() {
        Ok(secs) if secs > 0.0 => {
            let millis = (secs * 1e3).round() as u64;
            Ok(Duration::from_millis(millis))
        }
        _ => Err(format!("{ERR_TIMEVAL}: {arg}")),
    }
}

/* -------------------------------------------------------------------------- */

/**
Parse and expand a list of space separated IPv4/v6 addresses
(single, range, CIDR), taking exclusions into account if applicable.

Removes duplicates and preserves order of first occurrence.

### Returns a tuple of:
- Vec of parsed IP addresses
- Set of seen IP addresses (so that the caller doesn't need to re-derive it)
- Set of excluded IP addresses (parsed)
- Set of strings that failed to parse
*/
pub fn parse_ip_addresses<T>(
    targets: &[String],
    exclude: Option<&[String]>,
    logger: &T,
) -> (
    Vec<IpAddr>,
    HashSet<IpAddr>,
    HashSet<IpAddr>,
    HashSet<String>,
)
where
    T: Logger,
{
    let mut all_addrs: Vec<IpAddr> = Vec::new();
    let mut seen: HashSet<IpAddr> = HashSet::new();
    let mut failed: HashSet<String> = HashSet::new();

    // Parse all targets and expand them into individual IPs
    for target in targets {
        match parse_ip_or_range(target) {
            Ok(mut ips) => {
                if ips.len() > 1 {
                    logger.log(format!("{INFO_EXPANDED} '{target}': {}", ips.len()));
                }
                all_addrs.append(&mut ips);
            }
            Err(e) => {
                logger.warn(format!("{ERR_PARSE_IP} '{target}': {e}"));
                failed.insert(target.clone());
            }
        }
    }

    // Remove duplicates while preserving order
    all_addrs.retain(|ip: &IpAddr| seen.insert(*ip));

    // Parse exclusions and expand them into individual IPs
    let mut exclusions: HashSet<IpAddr> = HashSet::new();
    if let Some(exclude) = exclude {
        for exc in exclude {
            match parse_ip_or_range(exc) {
                Ok(mut ips) => {
                    if ips.len() > 1 {
                        logger.log(format!(
                            "{INFO_EXPANDED} '{exc}': {} (exclusion)",
                            ips.len()
                        ));
                    }
                    exclusions.extend(ips.drain(..));
                }
                Err(e) => {
                    logger.error(format!("{ERR_PARSE_IP} '{exc}' (exclusion): {e}"));
                }
            }
        }

        // Apply exclusions if needed
        if !exclusions.is_empty() {
            // let's see if we actually exclude anything
            let remainder: HashSet<IpAddr> = &seen - &exclusions;
            if remainder == seen {
                logger.notice(WARN_NO_MATCHES);
            } else if remainder.is_empty() {
                logger.warn(WARN_ALL_EXCLUDED);
            } else {
                logger.log(format!(
                    "{INFO_EXCLUDE}: {}",
                    (seen.len() - remainder.len())
                ));
                all_addrs.retain(|ip: &IpAddr| !exclusions.contains(ip));
            }
        };
    }

    (all_addrs, seen, exclusions, failed)
}

/* -------------------------------------------------------------------------- */

/// Return the reverse DNS name of an address (`<..>.in-addr.arpa` or `<..>.ip6.arpa`).
pub fn reverse_name(addr: &IpAddr) -> String {
    match addr {
        IpAddr::V4(v4) => v4.octets().iter().rev().join(".") + PTR_IPV4,
        IpAddr::V6(v6) => {
            itertools::Itertools::intersperse(
                v6.octets()
                    .iter()
                    .rev()
                    .flat_map(|b| format!("{b:02x}").chars().rev().collect::<Vec<char>>()),
                '.',
            )
            .collect::<String>()
                + PTR_IPV6
        }
    }
}

/* -------------------------------------------------------------------------- */

/**
Asynchronously resolve a set of DNS names into IP addresses.

### Returns:
- a Vec of tuples ([String], [Vec] of [IpAddr]) for successfully resolved names
*/
pub(crate) async fn resolve_names<T>(
    failed: HashSet<String>,
    res: &TokioResolver,
    logger: &T,
) -> Vec<(String, Vec<IpAddr>)>
where
    T: Logger,
{
    let mut resolved: Vec<(String, Vec<IpAddr>)> = Vec::with_capacity(failed.len());
    for name in &failed {
        match res.lookup_ip(name).await {
            Ok(lookup) => {
                let ips: Vec<IpAddr> = lookup.iter().collect();
                if !ips.is_empty() {
                    logger.info(format!("{INFO_RESOLVE_ONE} '{name}': {}", ips.len()));
                }
                // if this was a fully qualified domain name, strip the trailing dot
                resolved.push((name.trim_end_matches(".").to_string(), ips));
            }
            Err(e) => {
                logger.error(format!("{ERR_RESOLVE} '{name}': {e}"));
            }
        }
    }
    resolved
}

/* -------------------------------------------------------------------------- */

/// A single histogram bucket for data distribution.
#[derive(Debug, Clone)]
pub struct HistogramBucket {
    pub low: f64,
    pub high: f64,
    pub count: u64,
}

/// Create histogram buckets from a set of floating point data.
pub fn make_histogram_buckets(data: Vec<f64>, bins: usize) -> Vec<HistogramBucket> {
    if data.is_empty() {
        return vec![];
    }

    let min: f64 = data.iter().fold(f64::INFINITY, |a: f64, &b| a.min(b));
    let max: f64 = data.iter().fold(0.0, |a: f64, &b| a.max(b));
    let bin_width: f64 = (max - min) / bins as f64;
    let mut counts: Vec<u64> = vec![0u64; bins];

    for &entry in &data {
        let bin_idx: usize = ((entry - min) / bin_width).floor() as usize;
        let bin_idx: usize = bin_idx.min(bins - 1); // clamp to last bin
        counts[bin_idx] += 1;
    }

    counts
        .iter()
        .enumerate()
        .map(|(i, &count)| {
            let low = min + (i as f64 * bin_width);
            let high = low + bin_width;
            HistogramBucket { low, high, count }
        })
        .collect()
}
