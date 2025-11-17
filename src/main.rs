// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

mod args;
mod structs;
use crate::{
    args::MpConfig,
    structs::{PingStatus, PingTarget, PingTargetInner},
};

use futures::future::join_all;
use ncurses::*;
use rand::random;
use std::{
    collections::VecDeque,
    net::IpAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use surge_ping::{Client, Config, ICMP, PingIdentifier, PingSequence, Pinger, SurgeError};
use tokio::{sync::Mutex, time::sleep};

const PING_DATA: &[u8] = &[0; 32];

/// Create PingTarget instances for each IP address.
fn make_targets(addrs: &Vec<IpAddr>, histsize: usize) -> Vec<Arc<PingTarget>> {
    addrs
        .into_iter()
        .map(|addr| {
            Arc::new(PingTarget {
                addr: *addr,
                data: Mutex::new(PingTargetInner {
                    sent: 0,
                    recv: 0,
                    rtts: VecDeque::with_capacity(histsize),
                    status: PingStatus::None,
                }),
            })
        })
        .collect()
}

/// Send a ping and update statistics.
async fn ping(pinger: Arc<Mutex<Pinger>>, tgt: Arc<PingTarget>, seq: u16, histsize: u32) {
    let mut pinger = pinger.lock().await;
    match pinger.ping(PingSequence(seq), &PING_DATA).await {
        Ok((_, dur)) => {
            let mut stats = tgt.data.lock().await;
            stats.recv += 1;
            stats.rtts.push_back(dur.as_micros() as u32);
            if stats.rtts.len() > histsize as usize {
                stats.rtts.pop_front();
            }
            stats.status = PingStatus::Ok;
        }
        Err(e) => {
            let mut stats = tgt.data.lock().await;
            stats.status = match e {
                SurgeError::Timeout { .. } => PingStatus::Timeout,
                _ => PingStatus::Error,
            };
        }
    };
}

/// Set up a ping loop for each target.
async fn ping_loop(
    tgt: Arc<PingTarget>,
    client: Arc<Client>,
    quit: Arc<AtomicBool>,
    conf: Arc<MpConfig>,
) {
    let id: PingIdentifier = PingIdentifier(random());
    let mut pinger: Pinger = client.pinger(tgt.addr, id).await;
    pinger.timeout(conf.timeout);
    // Wrap pinger in Arc<Mutex<>> for shared async access
    let pinger: Arc<Mutex<Pinger>> = Arc::new(Mutex::new(pinger));

    loop {
        if quit.load(Ordering::Relaxed) {
            break;
        }

        let seq: u16 = {
            let mut stats = tgt.data.lock().await;
            // update sent count here to make sure it's incremented before
            // sending so that the main sent count stays accurate even if
            // ping fails or we get out of order replies etc
            let sent: u64 = stats.sent;
            stats.sent += 1;
            // calculate the 16-bit sequence number from sent count,
            // since 2^16 is the max for ICMP sequence numbers
            (sent % 65536) as u16
        };
        tokio::spawn(ping(pinger.clone(), tgt.clone(), seq, conf.histsize));
        sleep(conf.interval).await;
    }
}

/// Set up the signal handler to catch Ctrl-C
fn setup_signal_handler(quit: Arc<AtomicBool>) {
    ctrlc::set_handler(move || {
        quit.store(true, Ordering::Relaxed);
        println!("\nInterrupted. Exiting...");
    })
    .expect("Error setting Ctrl-C handler");
}

/// Set up or tear down the ncurses environment
fn setup_curses(quit: bool) {
    match quit {
        true => {
            curs_set(CURSOR_VISIBILITY::CURSOR_VISIBLE);
            echo();
            endwin();
        }
        false => {
            initscr();
            noecho();
            curs_set(CURSOR_VISIBILITY::CURSOR_INVISIBLE);
            keypad(stdscr(), true);
            nodelay(stdscr(), true);
            clear();
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let conf: Arc<MpConfig> = MpConfig::parse().into();
    let targets: Vec<Arc<PingTarget>> = make_targets(&conf.addrs, conf.histsize as usize);
    let quit: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    // Pinger clients
    let client_v4: Arc<Client> = Client::new(&Config::default())?.into();
    let client_v6: Arc<Client> = Client::new(&Config::builder().kind(ICMP::V6).build())?.into();

    // Spawn ping tasks
    for tgt in &targets {
        let client = match tgt.addr {
            IpAddr::V4(_) => client_v4.clone(),
            IpAddr::V6(_) => client_v6.clone(),
        };
        tasks.push(tokio::spawn(ping_loop(
            tgt.clone(),
            client,
            quit.clone(),
            conf.clone(),
        )));
    }

    // Curses initialization
    setup_signal_handler(quit.clone());
    setup_curses(false);

    // Main display loop
    while !quit.load(Ordering::Relaxed) {
        mvprintw(
            0,
            0,
            "Address\t\tSent\tRecv\tLatest\tMean\tMin\tMax\tStatus",
        );

        for (row, tgt) in targets.iter().enumerate() {
            let stats = tgt.data.lock().await;

            let (latest, mean, min, max) = if stats.rtts.is_empty() {
                (
                    "-".to_string(),
                    "-".to_string(),
                    "-".to_string(),
                    "-".to_string(),
                )
            } else {
                let last: f64 = *stats.rtts.back().unwrap() as f64 / 1e3; // convert to ms
                let sum: f64 = stats.rtts.iter().map(|&x| (x as f64 / 1e3)).sum();
                let m: f64 = sum / stats.rtts.len() as f64;
                let min_v: f64 = *stats.rtts.iter().min().unwrap() as f64 / 1e3;
                let max_v: f64 = *stats.rtts.iter().max().unwrap() as f64 / 1e3;
                (
                    format!("{:.2}", last),
                    format!("{:.2}", m),
                    format!("{:.2}", min_v),
                    format!("{:.2}", max_v),
                )
            };

            mvprintw(
                (row + 1) as i32,
                0,
                &format!(
                    "{:<12}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    tgt.addr, stats.sent, stats.recv, latest, mean, min, max, stats.status
                ),
            );
        }

        refresh();
        sleep(Duration::from_millis(500)).await;
    }

    join_all(tasks).await;
    setup_curses(true);
    Ok(())
}
