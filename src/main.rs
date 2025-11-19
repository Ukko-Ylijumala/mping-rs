// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

#![allow(dead_code)]

mod args;
mod ip_addresses;
mod structs;
mod tabulator;
mod utils;
use crate::{
    args::MpConfig,
    structs::{PingStatus, PingTarget, PingTargetInner, StatsWindow},
    tabulator::simple_tabulate,
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

/// Create PingTarget instances for each IP address.
fn make_targets(addrs: &[IpAddr], histsize: usize) -> Vec<Arc<PingTarget>> {
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

/// Send a ping and update statistics (deprecated, for reference).
async fn ping(
    cl: Arc<Client>,
    tgt: Arc<PingTarget>,
    to: Duration,
    id: PingIdentifier,
    seq: u16,
    pl: Arc<[u8]>,
) {
    let mut pinger: Pinger = cl.pinger(tgt.addr, id).await;
    pinger.timeout(to);
    let res = pinger.ping(PingSequence(seq), &pl).await;
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
    payload: Arc<[u8]>,
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
        let tgt_clone: Arc<PingTarget> = tgt.clone();
        let pl: Arc<[u8]> = payload.clone();
        tokio::spawn(async move {
            let res = pinger.ping(PingSequence(seq), &pl).await;
            update_ping_stats(&tgt_clone, res).await;
        });
    }
}

/// Format statistics data for display.
async fn format_stats(tgt: &Arc<PingTarget>) -> (u64, u64, String, String, String, String, String) {
    let (sent, recv, status_s, rtts) = {
        // Only hold the lock inside this block to try to minimize contention.
        let stats = tgt.data.lock().await;
        let sent: u64 = stats.sent;
        let recv: u64 = stats.recv;
        // status formatting is cheap relative to float formatting
        let status_s: String = format!("{}", stats.status);
        let rtt_snap = if stats.rtts.is_empty() {
            None
        } else {
            // pull raw numeric RTTs out while holding the lock
            let (m, mi, ma) = stats.rtts.mean_min_max_ms().unwrap();
            Some((stats.rtts.latest_ms().unwrap(), m, mi, ma))
        };
        (sent, recv, status_s, rtt_snap)
    };

    // Do all the (expensive) string formatting after releasing the lock.
    let (latest_s, mean_s, min_s, max_s) = if let Some((last, m, mi, ma)) = rtts {
        (
            format!("{:.2}", last),
            format!("{:.2}", m),
            format!("{:.2}", mi),
            format!("{:.2}", ma),
        )
    } else {
        (
            "-".to_string(),
            "-".to_string(),
            "-".to_string(),
            "-".to_string(),
        )
    };

    (sent, recv, latest_s, mean_s, min_s, max_s, status_s)
}

/// Gather current data from all targets.
async fn gather_target_data(targets: &[Arc<PingTarget>]) -> Vec<[String; 9]> {
    let mut data: Vec<[String; 9]> = Vec::new();

    for tgt in targets {
        let (sent, recv, last, mean, min, max, stat) = format_stats(tgt).await;

        let loss: String = if sent == 0 {
            "-".to_string()
        } else if (sent - recv) == 1 {
            // catch the common case of one receive missing (probably in transit)
            "0.0%".to_string()
        } else {
            format!("{:.1}%", 100.0 * (sent as f64 - recv as f64) / sent as f64)
        };

        data.push([
            tgt.addr.to_string(),
            sent.to_string(),
            recv.to_string(),
            loss,
            last,
            mean,
            min,
            max,
            stat,
        ]);
    }

    data
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let conf: Arc<MpConfig> = MpConfig::parse().into();

    // Pinger clients
    // sharing a client across multiple targets is safe and allows socket reuse
    let client_v4: Option<Arc<Client>> = if conf.addrs.iter().any(|a: &IpAddr| a.is_ipv4()) {
        match Client::new(&Config::default()) {
            Ok(c) => Some(Arc::new(c)),
            Err(e) => nice_permission_error(&e, "v4"),
        }
    } else {
        None
    };
    let client_v6: Option<Arc<Client>> = if conf.addrs.iter().any(|a: &IpAddr| a.is_ipv6()) {
        let cfg: Config = Config::builder().kind(ICMP::V6).build();
        match Client::new(&cfg) {
            Ok(c) => Some(Arc::new(c)),
            Err(e) => nice_permission_error(&e, "v6"),
        }
    } else {
        None
    };

    // Spawn ping tasks
    let payload: Arc<[u8]> = vec![0u8; conf.size as usize].into();
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
            payload.clone(),
        )));
    }

    // Curses initialization
    setup_signal_handler(quit.clone());
    panic::set_hook(Box::new(panic_handler));
    setup_curses(false);

    // Main display loop
    let mut ui_tick: Interval = time::interval(Duration::from_millis(250));
    let headers: Vec<&str> = vec![
        "Address", "Sent", "Recv", "Loss", "Latest", "Mean", "Min", "Max", "Status",
    ];
    while !quit.load(Ordering::Relaxed) {
        ui_tick.tick().await;

        // Render the table with dynamic tabulation, correct column widths etc
        let data: Vec<[String; 9]> = gather_target_data(&targets).await;
        for (i, line) in simple_tabulate(data, Some(&headers)).iter().enumerate() {
            // mvprintw could segfault if the string contains "%" characters, use mvaddstr instead
            mvaddstr(i as i32, 0, line);
        }

        refresh();
        if getch() == 'q' as i32 {
            quit.store(true, Ordering::Relaxed);
        }
    }

    // Cleanup
    setup_curses(true);
    eprintln!("Interrupted. Exiting...");
    join_all(tasks).await;

    // Print final stats
    let data: Vec<[String; 9]> = gather_target_data(&targets).await;
    for line in simple_tabulate(data, Some(&headers)) {
        println!("{line}");
    }
    Ok(())
}
