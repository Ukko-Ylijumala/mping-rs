// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use clap::{
    ArgMatches, CommandFactory, Parser, crate_authors, crate_description, crate_name,
    crate_version, value_parser,
};
use std::{fmt::Debug, net::IpAddr, time::Duration};

/// Parse an IP address from a string
fn parse_ip(arg: &str) -> Result<IpAddr, String> {
    if let Ok(ip) = arg.parse::<IpAddr>() {
        Ok(ip)
    } else {
        Err(format!("Invalid IP address: {}", arg))
    }
}

/// Parse a floating point number into a Duration.
fn parse_float_into_duration(arg: &str) -> Result<Duration, String> {
    match arg.parse::<f64>() {
        Ok(secs) if secs > 0.0 => {
            let millis = (secs * 1000.0).round() as u64;
            Ok(Duration::from_millis(millis))
        }
        _ => Err(format!("Invalid time value: {}", arg)),
    }
}

/// Configuration struct for the program.
#[derive(Parser, Default, Debug, Clone)]
#[command(name = crate_name!(), version = crate_version!(), author = crate_authors!(), about = crate_description!())]
pub(crate) struct MpConfig {
    #[arg(
        required = true,
        value_name = "IP1 [IP2...]",
        value_parser = parse_ip,
        help = "Space separated list of IP addresses to monitor",
    )]
    pub addrs: Vec<IpAddr>,

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
        short = 'H',
        value_name = "NUM",
        required = false,
        value_parser = value_parser!(u32).range(60..65536),
        default_value = "3600",
        help = "History size (number of pings results to keep) [default: 3600]"
    )]
    pub histsize: u32,

    #[arg(long, short = 'v', help = "Increase output verbosity")]
    pub verbose: bool,

    #[arg(long, help = "Print debug information where applicable")]
    pub debug: bool,
}

impl MpConfig {
    /// Parses command line arguments and returns a [MpConfig] struct.
    pub fn parse() -> MpConfig {
        let args: ArgMatches = Self::command().get_matches();

        let addrs: Vec<IpAddr> = args
            .get_many::<IpAddr>("addrs")
            .unwrap()
            .map(|&ip| ip)
            .collect();
        if addrs.is_empty() {
            eprintln!("No valid IP addresses provided.");
            std::process::exit(1);
        }

        // clamp interval between 500ms and 10s...
        let interval: Duration = match *args.get_one::<Duration>("interval").unwrap() {
            d if d < Duration::from_millis(500) => Duration::from_millis(500),
            d if d > Duration::from_secs(10) => Duration::from_secs(10),
            d => d,
        };
        // ... and timeout between 100ms and 5s
        let timeout: Duration = match *args.get_one::<Duration>("timeout").unwrap() {
            d if d < Duration::from_millis(100) => Duration::from_millis(100),
            d if d > Duration::from_secs(5) => Duration::from_secs(5),
            d => d,
        };

        Self {
            addrs,
            interval,
            timeout,
            histsize: *args.get_one::<u32>("histsize").unwrap(),
            verbose: args.get_flag("verbose"),
            debug: args.get_flag("debug"),
        }
    }
}
