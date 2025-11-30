// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

#![allow(dead_code)]

mod args;
mod ip_addresses;
mod latencywin;
mod structs;
mod tabulator;
mod tui;
mod utils;

use crate::{
    args::MpConfig,
    structs::{AppState, PacketRecord, PingStatus, PingTarget, StatsSnapshot},
    tabulator::simple_tabulate,
    tui::{TerminalGuard, determine_widths, key_event_poll},
    utils::{nice_permission_error, setup_signal_handler},
};

use futures::future::join_all;
use miniutils::ProcessInfo;
use rand::{fill, random};
use ratatui::{prelude::*, widgets::*};
use std::{
    net::IpAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use surge_ping::{
    Client, Config, ICMP, IcmpPacket, PingIdentifier, PingSequence, Pinger, SurgeError,
};
use tokio::time::{self, Instant, Interval};

const DEFAULT_TICK: Duration = Duration::from_millis(200); // 5 Hz

////////////////////////////////////////////////////////////////////////////////

/// Create [PingTarget] instances for each IP address.
fn make_targets(addrs: &[IpAddr], histsize: usize, detailed: usize) -> Vec<Arc<PingTarget>> {
    addrs
        .into_iter()
        .map(|addr| Arc::new(PingTarget::new(*addr, histsize, detailed)))
        .collect()
}

/// Update ping statistics based on the result. Separated into fn for target lock granularity.
async fn update_ping_stats(
    tgt: &Arc<PingTarget>,
    res: Result<(IcmpPacket, Duration), SurgeError>,
    mut rec: PacketRecord,
) {
    let mut stats = tgt.data.write();
    match res {
        Ok((_, dur)) => {
            stats.recv += 1;
            stats.rtts.push(dur.as_micros() as u32);
            stats.status = PingStatus::Ok;
            rec.set_rtt(dur);
        }
        Err(e) => {
            stats.status = match e {
                SurgeError::Timeout { .. } => {
                    if stats.sent > 10 && stats.recv == 0 {
                        PingStatus::NotReachable
                    } else {
                        PingStatus::Timeout
                    }
                }
                _ => PingStatus::Error(e),
            };
        }
    };
    stats.recent.push(rec);
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
    let mut ticker: Interval = time::interval(conf.interval.min(DEFAULT_TICK));
    let mut next_ping: Instant = tokio::time::Instant::now();
    let mut payload: Arc<[u8]> = match conf.randomize {
        // create a new payload for the ping loop which we can randomize
        true => payload.as_ref().to_vec().into(),
        false => payload.clone(),
    };

    while !quit.load(Ordering::Relaxed) {
        ticker.tick().await;
        if tokio::time::Instant::now() < next_ping {
            continue;
        }

        let seq: u16 = {
            let mut stats = tgt.data.write();
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
        let pl: Arc<[u8]> = match conf.randomize {
            true => {
                let payload: &mut [u8] = Arc::make_mut(&mut payload);
                // Can't use a thread-local RNG here (for performance)
                // because it's not Send'able across await points.
                // However, we can spare CPU time by randomizing only
                // the first 32 bytes of the payload, which should be plenty.
                // And we already know the payload must be 32 bytes minimum.
                fill(&mut payload[..32]);
                payload.into()
            }
            false => payload.clone(),
        };

        tokio::spawn(async move {
            let rec: PacketRecord = PacketRecord::new(seq);
            let res = pinger.ping(PingSequence(seq), &pl).await;
            update_ping_stats(&tgt_clone, res, rec).await;
        });

        next_ping += conf.interval;
    }
}

/// Extract statistics data from a target's inner data.
async fn extract_stats(tgt: &Arc<PingTarget>) -> (StatsSnapshot, String) {
    // Holding the lock inside this function only should minimize contention.
    // Do all the expensive string formatting in the caller.
    let stats = tgt.data.read();
    let snap: StatsSnapshot = StatsSnapshot::new_from(&stats);
    // status formatting is cheap relative to float formatting
    (snap, format!("{}", stats.status))
}

/// Gather current data from all targets.
async fn gather_target_data(targets: &[Arc<PingTarget>], debug: bool) -> Vec<Vec<String>> {
    let mut data: Vec<Vec<String>> = Vec::new();

    // Collect all extract_stats futures and run them concurrently, then process results
    let res = join_all(targets.iter().map(|t| extract_stats(t))).await;

    for (tgt, (snap, stat)) in targets.iter().zip(res.into_iter()) {
        let status: String = if debug {
            match &snap.error {
                Some(e) => e.to_string(),
                None => stat,
            }
        } else {
            stat
        };

        // Do all the (expensive) string formatting after releasing the lock.
        data.push(vec![
            tgt.addr.to_string(),
            snap.sent.to_string(),
            snap.recv.to_string(),
            snap.loss_str(),
            snap.last_str(),
            snap.mean_str(),
            snap.min_str(),
            snap.max_str(),
            snap.stdev_str(),
            status,
        ]);
    }

    data
}

/// Render the current frame. Display will be updated as soon as this function completes.
fn render_frame<'a>(frame: &mut Frame, state: &'a AppState<'a>, data: &'a [Vec<String>]) {
    let area: Rect = frame.area();
    let (widths, sum) = determine_widths(&data, Some(&state.header_widths));
    let tbl_req_w: u16 = ((state.colspacing as usize) * (state.headers.len() - 1) + sum + 2) as u16;
    let tbl_req_h: u16 = (data.len() + 3) as u16; // title + header
    let title_area = Rect {
        x: 0,
        y: 0,
        width: area.width,
        height: 1,
    };
    let table_area = Rect {
        x: 0,
        y: 1,
        width: tbl_req_w,
        height: tbl_req_h,
    };
    let proc_area = Rect {
        x: 0,
        y: area.height - 1,
        width: area.width,
        height: 1,
    };

    let block = Block::bordered().title(format!(" Ping targets: {} ", state.targets.len()));
    let hdr_row = state
        .headers
        .iter()
        .map(|h| {
            Cell::from(*h).style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
        })
        .collect::<Vec<Cell>>();

    let data_rows = data
        .iter()
        .map(|r| Row::new(r.iter().map(|c| Cell::from(c.as_str()))))
        .collect::<Vec<Row>>();

    let table = Table::new(data_rows, &widths)
        .header(Row::new(hdr_row))
        .column_spacing(state.colspacing)
        .block(block);

    let procinfo = Paragraph::new(format!(
        "CPU: {:>7} | mem: {} | pid: {}",
        state.pi.cpu_str(),
        state.pi.mem_str(),
        state.pi.pid,
    ))
    .alignment(Alignment::Right);

    frame.render_widget(&state.title, title_area);
    frame.render_widget(table, table_area);
    frame.render_widget(procinfo, proc_area);
    *state.next_refresh.write() += state.interval;
}

////////////////////////////////////////////////////////////////////////////////

#[tokio::main(worker_threads = 8)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let conf: Arc<MpConfig> = MpConfig::parse().into();

    // Pinger clients
    // sharing a client across multiple targets is safe and allows socket reuse
    let client_v4: Option<Arc<Client>> = if conf.addrs.iter().any(|a: &IpAddr| a.is_ipv4()) {
        match Client::new(&Config::default()) {
            Ok(c) => Some(Arc::new(c)),
            Err(e) => return Err(nice_permission_error(&e, "v4")),
        }
    } else {
        None
    };
    let client_v6: Option<Arc<Client>> = if conf.addrs.iter().any(|a: &IpAddr| a.is_ipv6()) {
        let cfg: Config = Config::builder().kind(ICMP::V6).build();
        match Client::new(&cfg) {
            Ok(c) => Some(Arc::new(c)),
            Err(e) => return Err(nice_permission_error(&e, "v6")),
        }
    } else {
        None
    };

    let mut app: AppState<'static> = AppState {
        pi: ProcessInfo::new(),
        targets: make_targets(&conf.addrs, conf.histsize as usize, conf.detailed as usize),
        title: Paragraph::new(format!("*** Multi-pinger v{} ***", conf.ver))
            .alignment(Alignment::Center)
            .style(Style::new().bold().green()),
        headers: vec![
            "Address", "Sent", "Recv", "Loss", "Last", "Mean", "Min", "Max", "Stdev", "Status",
        ],
        colspacing: 2,
        tasks: vec![],
        header_widths: vec![],
        interval: Duration::from_millis(250), // main display refresh interval
        next_refresh: tokio::time::Instant::now().into(),
        verbose: conf.verbose,
        debug: conf.debug,
    }
    .build();

    // Spawn ping tasks
    let payload: Arc<[u8]> = vec![0u8; conf.size as usize].into();
    let quit: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    for tgt in &app.targets {
        let client = match tgt.addr {
            IpAddr::V4(_) => client_v4.as_ref().expect("IPv4 client missing"),
            IpAddr::V6(_) => client_v6.as_ref().expect("IPv6 client missing"),
        };
        app.tasks.push(tokio::spawn(ping_loop(
            tgt.clone(),
            client.clone(),
            quit.clone(),
            conf.clone(),
            payload.clone(),
        )));
    }

    // Full-console TUI initialization - the RAII guard will clean up on drop
    setup_signal_handler(quit.clone());
    let mut guard: TerminalGuard = TerminalGuard::new(app.interval.as_millis(), app.verbose)?;
    let mut tick: Interval = time::interval(DEFAULT_TICK);
    let mut data: Vec<Vec<String>> =
        vec![vec!["".to_string(); app.headers.len()]; app.targets.len()];

    // Main display loop
    while !quit.load(Ordering::Relaxed) {
        tick.tick().await;
        key_event_poll(0, &quit)?;
        if tokio::time::Instant::now() < *app.next_refresh.read() {
            continue;
        }

        // Gather data for display and render the frame
        data = gather_target_data(&app.targets, app.debug).await;
        guard
            .term
            .draw(|frame: &mut Frame| render_frame(frame, &mut app, &data))?;
    }

    // Cleanup
    drop(guard); // explicitly drop TUI guard to restore terminal so we can print
    if app.debug {
        eprintln!("Main thread quitting. Waiting for tasks to terminate...");
    }
    join_all(app.tasks).await;

    // Print final stats
    for line in simple_tabulate(&data, Some(&app.headers)) {
        println!("{line}");
    }
    Ok(())
}
