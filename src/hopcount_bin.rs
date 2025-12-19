// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use clap::{Parser, crate_authors, value_parser};
use mping::{determine_hops, parse_float_into_duration};
use std::{net::IpAddr, time::Duration};

/// Configuration struct for hopcount.
#[derive(Parser, Debug)]
#[command(
    name = "hopcount",
    version = "0.1.2",
    author = crate_authors!(),
    about = "Estimate hop counts using ICMP Echo Requests")]
struct Args {
    #[arg(
        required = true,
        value_name = "IPADDR",
        value_parser = value_parser!(IpAddr),
        help = "Target IP address to probe"
    )]
    pub target: IpAddr,

    #[arg(
        long,
        value_name = "SECS",
        required = false,
        value_parser = parse_float_into_duration,
        default_value = "1",
        help = "Timeout duration in seconds"
    )]
    pub timeout: Duration,

    #[arg(long, help = "Enable debug output")]
    pub debug: bool,

    #[arg(long, short = 'q', help = "Just print the hop count, nothing else")]
    pub quiet: bool,
}

fn main() {
    let args = Args::parse();

    match determine_hops(args.target, args.timeout, args.debug) {
        Ok((hops, ttl)) => {
            if args.quiet {
                println!("{hops}");
            } else {
                println!(
                    "Estimated hop count to {} (received TTL {ttl}): {hops}",
                    args.target
                );
            }
        }
        Err(e) => {
            if args.quiet {
                eprintln!("{e}");
            } else {
                eprintln!("Error determining hop count to {}: {e}", args.target);
                eprintln!(
                    "If permission is denied, try running with sudo or set the CAP_NET_RAW capability on the binary:"
                );
                eprintln!("    sudo setcap cap_net_raw+ep <path/to/hopcount>");
            }
            std::process::exit(1);
        }
    }
}
