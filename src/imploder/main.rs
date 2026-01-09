// Copyright (c) 2026 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use clap::{Parser, crate_authors, value_parser};
use mping::{
    imploder::{Cidr, collapse_cidrs},
    parse_float_into_duration,
};
use std::time::{Duration, Instant};

/// Configuration struct for imploder.
#[derive(Parser, Debug)]
#[command(
    name = "imploder",
    version = "0.1.1",
    author = crate_authors!(),
    about = "Implode (collapse) IP addresses/CIDRs into minimal CIDR representation")]
struct Args {
    #[arg(
        value_name = "IP_or_CIDR",
        value_parser = value_parser!(Cidr),
        help = "IP address(es) and/or CIDRs to collapse"
    )]
    pub entries: Vec<Cidr>,

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

fn main() {
    let args = Args::parse();
    let start = Instant::now();
    let result = collapse_cidrs(&args.entries);
    let duration = Instant::now() - start;

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
}
