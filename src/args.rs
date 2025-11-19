// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::ip_addresses::parse_ip_or_range;
use clap::{Parser, crate_authors, crate_description, crate_name, crate_version, value_parser};
use std::{collections::HashSet, fmt::Debug, net::IpAddr, process, time::Duration};

/// Parse a floating point number into a Duration.
fn parse_float_into_duration(arg: &str) -> Result<Duration, String> {
    match arg.parse::<f64>() {
        Ok(secs) if secs > 0.0 => {
            let millis = (secs * 1000.0).round() as u64;
            Ok(Duration::from_millis(millis))
        }
        _ => Err(format!("Invalid time value: {arg}")),
    }
}

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
        short = 'I',
        value_name = "SECS",
        required = false,
        value_parser = parse_float_into_duration,
        default_value = "1",
        help = "Interval between pings to each target"
    )]
    pub interval: Duration,

    #[arg(
        long,
        short = 'T',
        value_name = "SECS",
        required = false,
        value_parser = parse_float_into_duration,
        default_value = "2",
        help = "Timeout for each ping request"
    )]
    pub timeout: Duration,

    #[arg(
        long,
        short = 's',
        value_name = "NUM",
        required = false,
        value_parser = value_parser!(u16).range(32..32768),
        default_value = "32",
        help = "Size of ICMP payload in bytes"
    )]
    pub size: u16,

    #[arg(
        long,
        short = 'H',
        value_name = "NUM",
        required = false,
        value_parser = value_parser!(u32).range(60..65536),
        default_value = "3600",
        help = "History size (number of pings results to keep)"
    )]
    pub histsize: u32,

    #[arg(long, short = 'v', help = "Increase output verbosity")]
    pub verbose: bool,

    #[arg(long, help = "Print debug information where applicable")]
    pub debug: bool,

    #[arg(skip)]
    pub addrs: Vec<IpAddr>,
}

impl MpConfig {
    /// Parses command line arguments and returns a [MpConfig] struct.
    pub fn parse() -> MpConfig {
        let mut config: MpConfig = <MpConfig as Parser>::parse();

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
        config.addrs = all_addrs;

        if config.addrs.is_empty() {
            eprintln!("No valid IP addresses provided.");
            process::exit(1);
        }
        if config.verbose {
            eprintln!("Total unique addresses to monitor: {}", config.addrs.len());
        }


        // clamp interval between 500ms and 10s...
        config.interval = match config.interval {
            d if d < Duration::from_millis(500) => Duration::from_millis(500),
            d if d > Duration::from_secs(10) => Duration::from_secs(10),
            d => d,
        };
        // ... and timeout between 100ms and 5s
        config.timeout = match config.timeout {
            d if d < Duration::from_millis(100) => Duration::from_millis(100),
            d if d > Duration::from_secs(5) => Duration::from_secs(5),
            d => d,
        };

        config
    }
}
