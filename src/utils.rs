// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{logging::Logger, strings::*};
use futures::stream::{self, StreamExt};
use hickory_resolver::TokioResolver;
use itertools::Itertools;
use miniutils::iptools::parse_ip_or_range;
use signal_hook::{
    consts::signal::{SIGINT, SIGQUIT, SIGTERM},
    iterator::Signals,
};
use std::{
    collections::HashSet,
    io::{
        Error,
        ErrorKind::{Other, PermissionDenied},
    },
    net::IpAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
    vec,
};
#[cfg(target_os = "linux")]
use std::{
    env,
    path::{MAIN_SEPARATOR, PathBuf},
};

static PERMS_STR: [&str; 2] = ["permission", "permitted"];
const MAX_CONCURRENT_DNS: usize = 16; // concurrent in-flight DNS lookups

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
        eprintln!("{ERR_CAPS}");
        #[cfg(target_os = "linux")]
        {
            let name: String = env::args()
                .next()
                .and_then(|p| p.split(MAIN_SEPARATOR).last().map(|s| s.to_string()))
                .unwrap_or_else(|| APP_NAME.to_string());
            let bin_path: PathBuf = env::current_exe().unwrap_or_else(|_| PathBuf::from(&name));
            eprintln!("{ERR_CAPS_LINUX} {}", bin_path.display());
            if ip_ver == 4 {
                eprintln!("{ERR_CAPS_V4}");
            }
        }
        #[cfg(target_os = "macos")]
        eprintln!("{ERR_CAPS_MACOS}");
        Box::new(Error::new(
            PermissionDenied,
            format!("{ERR_SOCKETS}{ip_ver}"),
        ))
    } else {
        // other error -> let it bubble up normally
        Box::new(Error::new(Other, format!("{ERR_CLIENT}{ip_ver}: {err}")))
    }
}

/// Parse a floating point number into a Duration. Result is at least 1 ms —
/// a sub-millisecond input must not round down to [Duration::ZERO], which
/// breaks socket read timeouts and tokio interval tickers.
pub fn parse_float_into_duration(arg: &str) -> Result<Duration, String> {
    match arg.parse::<f64>() {
        Ok(secs) if secs > 0.0 => {
            let millis = ((secs * 1e3).round() as u64).max(1);
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
                logger.warn(format!("{ERR_PARSE_IP}: {e}"));
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
                    logger.error(format!("{ERR_PARSE_IP} (exclusion): {e}"));
                }
            }
        }

        // Apply exclusions if needed
        if !exclusions.is_empty() {
            // let's see if we actually exclude anything
            let remainder: HashSet<IpAddr> = &seen - &exclusions;
            if remainder == seen {
                logger.notice(WARN_NO_MATCHES);
            } else {
                if remainder.is_empty() {
                    logger.warn(WARN_ALL_EXCLUDED);
                } else {
                    logger.log(format!(
                        "{INFO_EXCLUDE}: {}",
                        (seen.len() - remainder.len())
                    ));
                }
                // Exclusions are honored even when they remove everything,
                // and `seen` must keep mirroring `all_addrs` membership.
                all_addrs.retain(|ip: &IpAddr| !exclusions.contains(ip));
                seen = remainder;
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

### Returns a tuple of:
- Vec of ([String], [Vec] of [IpAddr]) pairs for successfully resolved names
  (FQDN trailing dot stripped). Names that resolved to zero addresses are not
  included here — they go into the unresolved set instead.
- Set of input names that either errored out or resolved to no addresses
  (with their original spelling, including any trailing dot).
*/
pub(crate) async fn resolve_names<T>(
    failed: HashSet<String>,
    res: &TokioResolver,
    logger: &T,
) -> (Vec<(String, Vec<IpAddr>)>, HashSet<String>)
where
    T: Logger,
{
    /*
    Resolve concurrently — sequential lookups would make startup latency
    scale linearly with the number of names. The input is a set, so the
    (arbitrary) completion order of buffer_unordered changes nothing.
    */
    let lookups: Vec<(String, Result<Vec<IpAddr>, _>)> = stream::iter(failed)
        .map(|name| async move {
            let result = res.lookup_ip(&name).await;
            (name, result.map(|lookup| lookup.iter().collect::<Vec<IpAddr>>()))
        })
        .buffer_unordered(MAX_CONCURRENT_DNS)
        .collect()
        .await;

    let mut resolved: Vec<(String, Vec<IpAddr>)> = Vec::with_capacity(lookups.len());
    let mut unresolved: HashSet<String> = HashSet::new();
    for (name, result) in lookups {
        match result {
            Ok(ips) if !ips.is_empty() => {
                logger.info(format!("{INFO_RESOLVE_ONE} '{name}': {}", ips.len()));
                // if this was a fully qualified domain name, strip the trailing dot
                resolved.push((name.trim_end_matches('.').to_string(), ips));
            }
            Ok(_) => {
                unresolved.insert(name);
            }
            Err(e) => {
                logger.error(format!("{ERR_RESOLVE} '{name}': {e}"));
                unresolved.insert(name);
            }
        }
    }
    (resolved, unresolved)
}

/* -------------------------------------------------------------------------- */

/**
Result of parsing+resolving a batch of target strings via [collect_targets].

`addrs` and `seen` describe the same set; the [Vec] preserves first-seen order
(useful for the TUI table), the [HashSet] is there so callers don't have to
re-derive it.
*/
#[derive(Debug, Default)]
pub(crate) struct CollectedTargets {
    /// All unique target IPs (parsed + resolved-from-DNS), in first-seen order.
    /// Exclusions have already been applied to the parsed-IP portion.
    pub addrs: Vec<IpAddr>,
    /// Set form of `addrs`; mirrors its membership.
    pub seen: HashSet<IpAddr>,
    /// IPs that the exclusion list filtered out (parsed-IP portion only — DNS
    /// resolutions are not subjected to exclusions, matching prior behavior).
    pub excluded: HashSet<IpAddr>,
    /**
    Successfully resolved DNS names paired with their IPs (trailing dot
    stripped). Caller folds these into a [crate::structs::Resolved] map if
    it wants name display.
    */
    pub resolved: Vec<(String, Vec<IpAddr>)>,
    /// Strings that were neither valid IP/range/CIDR nor a resolvable DNS name.
    pub unresolved: HashSet<String>,
}

/**
Parse a batch of target strings, then try to resolve any leftovers as DNS names.

Combines [parse_ip_addresses] and [resolve_names] into the single pipeline used
by both initial CLI parsing ([crate::args::MpConfig::parse]) and the runtime
"add target" dialog. Resolved IPs are deduplicated against the parsed-IP set,
so the final `addrs` Vec contains each address at most once.
*/
#[rustfmt::skip]
pub(crate) async fn collect_targets<T>(
    targets: &[String],
    exclude: Option<&[String]>,
    resolver: &TokioResolver,
    logger: &T,
) -> CollectedTargets
where
    T: Logger,
{
    let (mut addrs, mut seen, excluded, failed) = parse_ip_addresses(targets, exclude, logger);
    let (resolved, unresolved) = resolve_names(failed, resolver, logger).await;

    // Append resolved IPs, skipping any already covered by parsed entries
    // (and skipping duplicates between resolved names too).
    for (_, ips) in &resolved {
        for ip in ips {
            if seen.insert(*ip) {
                addrs.push(*ip);
            }
        }
    }

    CollectedTargets { addrs, seen, excluded, resolved, unresolved }
}

/* -------------------------------------------------------------------------- */

/// Format a byte rate as a human-readable string (B/s, kB/s or MB/s).
/// Uses decimal (SI) units, as is the convention for network data rates.
pub fn human_rate(bytes_per_sec: f64) -> String {
    if bytes_per_sec >= 1e6 {
        format!("{:.2} MB/s", bytes_per_sec / 1e6)
    } else if bytes_per_sec >= 1e3 {
        format!("{:.1} kB/s", bytes_per_sec / 1e3)
    } else {
        format!("{bytes_per_sec:.0} B/s")
    }
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
    let max: f64 = data.iter().fold(f64::NEG_INFINITY, |a: f64, &b| a.max(b));
    let bin_width: f64 = (max - min) / bins as f64;

    // All values identical (or a single sample): zero bin width would make the
    // bucket math NaN. Collapse to a single bucket instead.
    if bin_width <= 0.0 {
        return vec![HistogramBucket {
            low: min,
            high: max,
            count: data.len() as u64,
        }];
    }

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
