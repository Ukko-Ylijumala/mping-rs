// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

#![allow(dead_code)]

mod args;
mod structs;
mod utils;
use crate::{
    args::MpConfig,
    structs::{PingStatus, PingTarget, PingTargetInner, StatsWindow},
    utils::{nice_permission_error, panic_handler, setup_curses, setup_signal_handler},
};

use futures::future::join_all;
use ncurses::*;
use rand::random;
use std::{
    net::IpAddr,
    panic,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use surge_ping::{
    Client, Config, ICMP, IcmpPacket, PingIdentifier, PingSequence, Pinger, SurgeError,
};
use tokio::{sync::Mutex, time, time::Interval};

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
                    rtts: StatsWindow::new(histsize),
                    status: PingStatus::None,
                }),
            })
        })
        .collect()
}

/// Send a ping and update statistics.
async fn ping(cl: Arc<Client>, tgt: Arc<PingTarget>, to: Duration, id: PingIdentifier, seq: u16) {
    let mut pinger: Pinger = cl.pinger(tgt.addr, id).await;
    pinger.timeout(to);
    let res = pinger.ping(PingSequence(seq), &PING_DATA).await;
    update_ping_stats(&tgt, res).await;
}

/// Update ping statistics based on the result. Separated into fn for target lock granularity.
async fn update_ping_stats(tgt: &Arc<PingTarget>, res: Result<(IcmpPacket, Duration), SurgeError>) {
    let mut stats = tgt.data.lock().await;
    match res {
        Ok((_, dur)) => {
            stats.recv += 1;
            stats.rtts.push(dur.as_micros() as u32);
            stats.status = PingStatus::Ok;
        }
        Err(e) => {
            stats.status = match e {
                SurgeError::Timeout { .. } => PingStatus::Timeout,
                _ => PingStatus::Error(e),
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
    let mut ticker: Interval = time::interval(conf.interval);

    while !quit.load(Ordering::Relaxed) {
        ticker.tick().await;

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

        // The async ping task can be spawned either using a closure, or an
        // async fn block. Both should be functionally equivalent.
        // In either case the pinger is created anew for each async context.
        //
        // Function style (saved for reference):
        // tokio::spawn(ping(client.clone(), tgt.clone(), conf.timeout, id, seq));
        //
        let mut pinger: Pinger = client.pinger(tgt.addr, id).await;
        pinger.timeout(conf.timeout);
        let tgt_clone = tgt.clone();
        tokio::spawn(async move {
            let res = pinger.ping(PingSequence(seq), &PING_DATA).await;
            update_ping_stats(&tgt_clone, res).await;
        });
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let conf: Arc<MpConfig> = MpConfig::parse().into();

    // Pinger clients
    // sharing a client across multiple targets is safe and allows socket reuse
    let client_v4: Option<Arc<Client>> = if conf.addrs.iter().any(|a| a.is_ipv4()) {
        match Client::new(&Config::default()) {
            Ok(c) => Some(Arc::new(c)),
            Err(e) => nice_permission_error(&e, "v4"),
        }
    } else {
        None
    };
    let client_v6: Option<Arc<Client>> = if conf.addrs.iter().any(|a| a.is_ipv6()) {
        let cfg: Config = Config::builder().kind(ICMP::V6).build();
        match Client::new(&cfg) {
            Ok(c) => Some(Arc::new(c)),
            Err(e) => nice_permission_error(&e, "v6"),
        }
    } else {
        None
    };

    // Spawn ping tasks
    let targets: Vec<Arc<PingTarget>> = make_targets(&conf.addrs, conf.histsize as usize);
    let quit: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    for tgt in &targets {
        let client = match tgt.addr {
            IpAddr::V4(_) => client_v4.as_ref().expect("IPv4 client missing"),
            IpAddr::V6(_) => client_v6.as_ref().expect("IPv6 client missing"),
        };
        tasks.push(tokio::spawn(ping_loop(
            tgt.clone(),
            client.clone(),
            quit.clone(),
            conf.clone(),
        )));
    }

    // Curses initialization
    setup_signal_handler(quit.clone());
    panic::set_hook(Box::new(panic_handler));
    setup_curses(false);

    // Main display loop
    let mut ui_tick: Interval = time::interval(Duration::from_millis(250));
    let header: &str = "Address\t\tSent\tRecv\tLoss\tLatest\tMean\tMin\tMax\tStatus";
    while !quit.load(Ordering::Relaxed) {
        ui_tick.tick().await;
        mvprintw(0, 0, header);

        for (row, tgt) in targets.iter().enumerate() {
            // Snapshot data under lock; compute outside.
            let (addr, sent, recv, latest, mean, min, max, status) = {
                let stats = tgt.data.lock().await;

                let (latest_s, mean_s, min_s, max_s) = if stats.rtts.is_empty() {
                    (
                        "-".to_string(),
                        "-".to_string(),
                        "-".to_string(),
                        "-".to_string(),
                    )
                } else {
                    let last: f64 = stats.rtts.latest_ms().unwrap();
                    let (m, mi, ma) = stats.rtts.mean_min_max_ms().unwrap();
                    (
                        format!("{:.2}", last),
                        format!("{:.2}", m),
                        format!("{:.2}", mi),
                        format!("{:.2}", ma),
                    )
                };

                (
                    format!("{}", tgt.addr),
                    stats.sent,
                    stats.recv,
                    latest_s,
                    mean_s,
                    min_s,
                    max_s,
                    format!("{}", stats.status),
                )
            };

            let loss_pct: String = if sent == 0 {
                "-".to_string()
            } else if sent - recv == 1 {
                // catch the common case of one receive missing (probably in transit)
                "0.0%".to_string()
            } else {
                format!("{:.1}%", 100.0 * (sent as f64 - recv as f64) / sent as f64)
            };

            let line: String = format!(
                "{:<12}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                addr, sent, recv, loss_pct, latest, mean, min, max, status
            );
            mvprintw((row + 1) as i32, 0, &line);
        }

        refresh();
        if getch() == 'q' as i32 {
            quit.store(true, Ordering::Relaxed);
        }
    }

    setup_curses(true);
    eprintln!("Interrupted. Exiting...");
    join_all(tasks).await;
    Ok(())
}
