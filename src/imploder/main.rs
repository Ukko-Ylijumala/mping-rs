// Copyright (c) 2026 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use clap::{Parser, crate_authors};
use mping::{
    imploder::{Cidr, collapse_cidrs, collapse_ranges},
    parse_float_into_duration, parse_ip_range,
};
use reqwest::blocking::Client;
use std::{
    fs::File,
    io::{BufRead, BufReader},
    time::{Duration, Instant},
};

const COMMENT_CHARS: [char; 2] = ['#', ';'];

/// Configuration struct for imploder.
#[derive(Parser, Debug)]
#[command(
    name = "imploder",
    version = "0.1.5",
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
    pub file: Option<String>,

    #[arg(
        long,
        short = 'U',
        value_name = "URL",
        help = "HTTP URL of IP addresses/ranges/CIDRs (one per line)"
    )]
    pub url: Option<String>,

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
    let mut entries_f: Vec<String> = Vec::new();
    let mut entries_u: Vec<String> = Vec::new();
    let mut timer: Instant = Instant::now();

    // Get entries from file if specified
    if let Some(path) = &args.file {
        let mut lines = read_input_file(path)?;
        if args.debug {
            eprintln!("Read {} entries from file '{}'", lines.len(), path);
        }
        entries_f.append(&mut lines);

        if args.debug {
            eprintln!("File processed in {:.2?}", Instant::now() - timer);
        }
    }

    // Get entries from URL if specified
    if let Some(url) = &args.url {
        timer = Instant::now();
        let mut lines = read_from_url(url, args.timeout)?;
        if args.debug {
            eprintln!("Read {} entries from URL '{}'", lines.len(), url);
        }
        entries_u.append(&mut lines);

        if args.debug {
            eprintln!("URL processed in {:.2?}", Instant::now() - timer);
        }
    }

    // Chain the iterators and process all entries
    entries.reserve(args.entries.len() + entries_f.len() + entries_u.len());
    let mut ranges = Vec::new();
    timer = Instant::now();
    for entry in args
        .entries
        .iter()
        .chain(entries_f.iter())
        .chain(entries_u.iter())
    {
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

    if args.debug {
        eprintln!("Entries preprocessed in {:.2?}", Instant::now() - timer);
    }

    timer = Instant::now();
    let result: Vec<Cidr> = collapse_cidrs(&entries);

    if result.is_empty() {
        eprintln!("No CIDRs generated from the provided input.");
    } else {
        if args.debug {
            eprintln!(
                "Imploded {} entries down to {} CIDR(s) in {:.2?} ({} IPs total)",
                args.entries.len() + entries_f.len() + entries_u.len(),
                result.len(),
                Instant::now() - timer,
                result.iter().fold(0u128, |acc, c| acc.saturating_add(c.len()))
            );
        }
        for cidr in result {
            println!("{cidr}");
        }
    }
    Ok(())
}
