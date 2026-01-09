// Copyright (c) 2026 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use clap::{Parser, crate_authors};
use mping::{
    imploder::{Cidr, collapse_cidrs, collapse_ips},
    parse_float_into_duration, parse_ip_range,
};
use std::time::{Duration, Instant};

/// Configuration struct for imploder.
#[derive(Parser, Debug)]
#[command(
    name = "imploder",
    version = "0.1.2",
    author = crate_authors!(),
    about = "Implode (collapse) IP addresses/ranges/CIDRs into minimal CIDR representations")]
struct Args {
    #[arg(
        value_name = "IP_or_CIDR",
        help = "IP addresses/ranges/CIDRs to collapse"
    )]
    pub entries: Vec<String>,

    #[arg(
        long,
        value_name = "SECS",
        required = false,
        value_parser = parse_float_into_duration,
        default_value = "5",
        help = "HTTP requrest timeout in seconds"
    )]
    pub timeout: Duration,

    #[arg(long, help = "Enable debug output")]
    pub debug: bool,
}

fn main() -> Result<(), String> {
    let args: Args = Args::parse();
    let mut entries: Vec<Cidr> = Vec::new();

    for entry in &args.entries {
        if entry.contains("-") {
            let ips =
                parse_ip_range(entry, true).map_err(|e| format!("Invalid range '{entry}': {e}"))?;
            entries.extend(collapse_ips(&ips));
        } else {
            entries.push(entry.parse::<Cidr>()?);
        }
    }

    let start: Instant = Instant::now();
    let result: Vec<Cidr> = collapse_cidrs(&entries);
    let duration: Duration = Instant::now() - start;

    if result.is_empty() {
        eprintln!("No CIDRs generated from the provided input.");
    } else {
        if args.debug {
            eprintln!(
                "Imploded {} entries down to {} CIDR(s) in {duration:.2?} ({} IPs total)",
                args.entries.len(),
                result.len(),
                result.iter().fold(0u128, |acc, c| acc + c.len())
            );
        }
        for cidr in result {
            println!("{cidr}");
        }
    }
    Ok(())
}
