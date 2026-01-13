// Copyright (c) 2026 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use clap::{Parser, crate_authors, value_parser};
use mping::{
    imploder::{Cidr, collapse_cidrs, collapse_ranges},
    parse_float_into_duration, parse_ip_range,
};
use reqwest::blocking::Client;
use std::{
    collections::HashSet,
    fs::File,
    io::{BufRead, BufReader},
    time::{Duration, Instant},
};

const COMMENT_CHARS: [char; 2] = ['#', ';'];

/// Configuration struct for imploder.
#[derive(Parser, Debug)]
#[command(
    name = "imploder",
    version = "0.2.0",
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
        short = 'F',
        value_name = "PATH",
        help = "Path to a file containing IP addresses/ranges/CIDRs (one per line)"
    )]
    pub file: Option<Vec<String>>,

    #[arg(
        long,
        short = 'U',
        value_name = "URL",
        help = "HTTP URL of IP addresses/ranges/CIDRs (one per line)"
    )]
    pub url: Option<Vec<String>>,

    #[arg(
        long,
        value_name = "SECS",
        required = false,
        value_parser = parse_float_into_duration,
        default_value = "5",
        help = "HTTP requrest timeout in seconds"
    )]
    pub timeout: Duration,

    #[arg(
        short = '4',
        help = "Only output IPv4 CIDRs (ignore IPv6 entries)"
    )]
    pub v4: bool,

    #[arg(
        short = '6',
        conflicts_with_all = ["v4"],
        help = "Only output IPv6 CIDRs (ignore IPv4 entries)"
    )]
    pub v6: bool,

    #[arg(
        long,
        value_name = "N",
        default_value = "0",
        value_parser = value_parser!(u16).range(0..=65535),
        help = "Maximum gap between IPs to fuzzily merge nearby ranges (0 = exact)"
    )]
    pub merge_gap: u16,

    #[arg(long, help = "Output host addresses as /32 (IPv4) or /128 (IPv6) CIDRs too")]
    pub host_cidr: bool,

    #[arg(long, help = "Enable debug output")]
    pub debug: bool,
}

#[inline]
fn add_line(lines: &mut Vec<String>, line: String) {
    #[rustfmt::skip]
    let item: String = line.split(&COMMENT_CHARS[..]).next().unwrap_or("").trim().to_string();
    if !item.is_empty() {
        lines.push(item);
    }
}

/**
Read a text file, remove empty lines and comments (full lines and
inline comments), and return whatever is left. At present, handles
Unix-style ("#") and INI-style (";") comments.
*/
fn read_input_file(path: &str) -> Result<Vec<String>, String> {
    let file: File = File::open(path).map_err(|e| format!("Failed to open file '{path}': {e}"))?;
    let reader: BufReader<File> = BufReader::new(file);
    let mut lines: Vec<String> = Vec::new();

    for rl in reader.lines() {
        let line: String = rl.map_err(|e| format!("Failed to read line from '{path}': {e}"))?;
        add_line(&mut lines, line);
    }

    Ok(lines)
}

/// Retrieve a list of IPs/CIDRs from a URL and process it.
fn read_from_url(url: &str, timeout: Duration) -> Result<Vec<String>, String> {
    let client = Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {e}"))?;

    let resp = client
        .get(url)
        .send()
        .map_err(|e| format!("HTTP request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP request returned '{}'", resp.status()));
    }

    let content = resp
        .text()
        .map_err(|e| format!("Failed to read response body: {e}"))?;

    let mut lines: Vec<String> = Vec::new();
    for line in content.lines() {
        add_line(&mut lines, line.to_string());
    }

    Ok(lines)
}

fn main() -> Result<(), String> {
    let args: Args = Args::parse();
    let mut entries: Vec<Cidr> = Vec::new();
    let mut entries_read: HashSet<String> = HashSet::new();
    let mut num_read: usize = 0;
    let mut num_dupl: usize = 0;

    // Get entries from file if specified
    if let Some(paths) = &args.file {
        for path in paths {
            let timer: Instant = Instant::now();
            let lines: Vec<String> = read_input_file(path)?;
            let num: usize = lines.len();
            let before_len: usize = entries_read.len();
            entries_read.extend(lines);

            if args.debug {
                let t: Duration = Instant::now() - timer;
                num_read += num;
                num_dupl += num - entries_read.len().saturating_sub(before_len);
                eprintln!("Read {num} entries in {t:.2?} from file '{path}'");
            }
        }
    }

    // Get entries from URL if specified
    if let Some(urls) = &args.url {
        for url in urls {
            let timer: Instant = Instant::now();
            let lines: Vec<String> = read_from_url(url, args.timeout)?;
            let num: usize = lines.len();
            let before_len: usize = entries_read.len();
            entries_read.extend(lines);

            if args.debug {
                let t: Duration = Instant::now() - timer;
                num_read += num;
                num_dupl += num - entries_read.len().saturating_sub(before_len);
                eprintln!("Read {num} entries in {t:.2?} from URL '{url}'");
            }
        }
    }

    // Chain the iterators and process all entries
    entries.reserve(args.entries.len() + entries_read.len());
    let mut ranges = Vec::new();
    let mut timer: Instant = Instant::now();
    for entry in args.entries.iter().chain(entries_read.iter()) {
        if entry.contains("-") {
            let range = parse_ip_range(entry).map_err(|e| format!("{e}"))?;
            ranges.push(range);
        } else {
            entries.push(entry.parse::<Cidr>()?);
        }
    }

    if !ranges.is_empty() {
        // It would be simpler to call `collapse_ranges` repeatedly in the loop, but that would be
        // wasteful since we'll now only do one sort + merge pass. A small win is still a win.
        entries.extend(collapse_ranges(&ranges).map_err(|e| format!("{e}"))?);
    }

    // We could also filter the families during parsing, but this way looks
    // conceptually cleaner and the performance impact should be negligible.
    if args.v4 {
        entries.retain(|c| c.is_ipv4());
    } else if args.v6 {
        entries.retain(|c| c.is_ipv6());
    }

    if args.debug {
        let t: Duration = Instant::now() - timer;
        eprintln!("Entries preprocessed in {t:.2?} (total: {num_read}, duplicates: {num_dupl})");
    }

    timer = Instant::now();
    let result: Vec<Cidr> = collapse_cidrs(&entries, args.merge_gap as u128);

    if result.is_empty() {
        eprintln!("No CIDRs generated from the provided input.");
    } else {
        if args.debug {
            eprintln!(
                "Imploded {} entries down to {} CIDR(s) in {:.2?} ({} IPs total)",
                args.entries.len() + entries_read.len(),
                result.len(),
                Instant::now() - timer,
                result.iter().fold(0u128, |acc, c| acc.saturating_add(c.len()))
            );
        }
        for cidr in result {
            if !args.host_cidr && cidr.is_host() {
                println!("{}", cidr.addr);
            } else {
                println!("{cidr}");
            }
        }
    }
    Ok(())
}
