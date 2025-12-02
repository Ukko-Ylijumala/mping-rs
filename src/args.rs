// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{ip_addresses::parse_ip_or_range, utils::parse_float_into_duration};
use clap::{Parser, crate_authors, crate_description, crate_name, crate_version, value_parser};
use std::{collections::HashSet, fmt::Debug, net::IpAddr, process, time::Duration};

/// Configuration struct for the program.
#[derive(Parser, Default, Debug, Clone)]
#[command(name = crate_name!(), version = crate_version!(), author = crate_authors!(), about = crate_description!())]
pub(crate) struct MpConfig {
    #[arg(
        required = true,
        value_name = "IP1 [IP2...]",
        help = "Space separated list of IP addresses or ranges to monitor"
    )]
    pub targets: Vec<String>,

    #[arg(
        long,
        value_name = "IP1[,IP2...]",
        value_delimiter = ',',
        require_equals = true,
        help = "Comma-separated IP addresses (and/or ranges) to exclude"
    )]
    pub exclude: Vec<String>,

    #[arg(
        long,
        short = 'I',
        value_name = "SECS",
        required = false,
        value_parser = parse_float_into_duration,
        default_value = "1",
        help = "Interval between pings to each target [0.01-10]"
    )]
    pub interval: Duration,

    #[arg(
        long,
        short = 'T',
        value_name = "SECS",
        required = false,
        value_parser = parse_float_into_duration,
        default_value = "2",
        help = "Timeout for each ping request [0.01-5]"
    )]
    pub timeout: Duration,

    #[arg(
        long,
        short = 's',
        value_name = "BYTES",
        required = false,
        value_parser = value_parser!(u16).range(32..32760),
        default_value = "32",
        help = "Size of ICMP payload (minus the 8-byte ICMP header) [32-32760]"
    )]
    pub size: u16,

    #[arg(long, short = 'R', help = "Randomize ICMP payload data [default: no]")]
    pub randomize: bool,

    #[arg(
        long,
        short = 'H',
        value_name = "NUM",
        required = false,
        value_parser = value_parser!(u32).range(60..65536),
        default_value = "3600",
        help = "Full history size (number of ping results to keep per target) [60-65536]"
    )]
    pub histsize: u32,

    #[arg(
        long,
        value_name = "NUM",
        required = false,
        value_parser = value_parser!(u16).range(10..1000),
        default_value = "100",
        help = "Detailed recent history size (for laggy/flappy detection etc) [10-1000]"
    )]
    pub detailed: u16,

    #[arg(
        long,
        value_name = "ms",
        required = false,
        value_parser = value_parser!(u64).range(100..5000),
        default_value = "250",
        help = "TUI refresh interval in milliseconds [100-5000]"
    )]
    pub refresh: u64,

    #[arg(long, short = 'v', help = "Increase output verbosity")]
    pub verbose: bool,

    #[arg(long, help = "Print debug information where applicable")]
    pub debug: bool,

    #[arg(skip)]
    pub addrs: Vec<IpAddr>,

    #[arg(skip)]
    pub ver: String,
}

impl MpConfig {
    /// Parses command line arguments and returns a [MpConfig] struct.
    pub fn parse() -> MpConfig {
        let mut config: MpConfig = <MpConfig as Parser>::parse();
        config.ver = crate_version!().to_string();

        // Parse all targets and expand them into individual IPs
        let mut all_addrs: Vec<IpAddr> = Vec::new();
        for target in &config.targets {
            match parse_ip_or_range(target) {
                Ok(mut ips) => {
                    if config.verbose {
                        if ips.len() > 1 {
                            eprintln!("Expanded '{target}' to {} addresses", ips.len());
                        }
                    }
                    all_addrs.append(&mut ips);
                }
                Err(e) => {
                    eprintln!("Error parsing target '{target}': {e}");
                    process::exit(1);
                }
            }
        }

        // Remove duplicates while preserving order
        let mut seen: HashSet<IpAddr> = HashSet::new();
        all_addrs.retain(|ip: &IpAddr| seen.insert(*ip));

        // Parse exclusions and expand them into individual IPs
        let mut exclusions: HashSet<IpAddr> = HashSet::new();
        for exc in &config.exclude {
            match parse_ip_or_range(exc) {
                Ok(mut ips) => {
                    if config.verbose {
                        if ips.len() > 1 {
                            eprintln!("Expanded '{exc}' to {} addresses (exclusion)", ips.len());
                        }
                    }
                    exclusions.extend(ips.drain(..));
                }
                Err(e) => {
                    eprintln!("Error parsing exclusion '{exc}': {e}");
                    process::exit(1);
                }
            }
        }

        // Apply exclusions if needed
        if !exclusions.is_empty() {
            // let's see if we actually exclude anything
            let remainder: HashSet<IpAddr> = &seen - &exclusions;
            if remainder == seen {
                eprintln!("WARN: exclusions did not match any target addresses.");
            } else if remainder.is_empty() {
                eprintln!("All target addresses were excluded.");
                process::exit(1);
            } else {
                if config.verbose {
                    eprintln!(
                        "Excluding {} addresses from target list",
                        (seen.len() - remainder.len())
                    );
                }
                all_addrs.retain(|ip: &IpAddr| !exclusions.contains(ip));
            }
        }

        config.addrs = all_addrs;
        if config.addrs.is_empty() {
            eprintln!("No valid IP addresses provided.");
            process::exit(1);
        } else if config.verbose {
            eprintln!("Total unique addresses to monitor: {}", config.addrs.len());
        }

        // clamp interval between 10ms and 10s...
        config.interval = match config.interval {
            d if d < Duration::from_millis(10) => Duration::from_millis(10),
            d if d > Duration::from_secs(10) => Duration::from_secs(10),
            d => d,
        };
        // ... and timeout between 10ms and 5s
        config.timeout = match config.timeout {
            d if d < Duration::from_millis(10) => Duration::from_millis(10),
            d if d > Duration::from_secs(5) => Duration::from_secs(5),
            d => d,
        };

        // If necessary, tweak the timeout so that we can't have an excessive number of
        // pending pings (tasks) to the same target. This is a simple heuristic to avoid
        // overwhelming the application with too many concurrent pings if the user has
        // set an unreasonably high timeout combined with a very low interval.
        let limit: Duration = config.interval * 4; // max. 4 pending pings per target
        if config.timeout > limit {
            if config.verbose {
                eprintln!(
                    "Adjusting timeout ({:.2}s -> {:.2}s) to avoid excessive concurrent pings (interval: {:.2}s)",
                    config.timeout.as_secs_f64(),
                    limit.as_secs_f64(),
                    config.interval.as_secs_f64(),
                );
            }
            config.timeout = limit;
        }

        config
    }
}
