// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

#![allow(dead_code)]

mod args;
mod hopcount;
mod latencywin;
mod logging;
mod macros;
mod pingdata;
mod strings;
mod structs;
mod ui;
mod utils;

use crate::{
    args::MpConfig,
    pingdata::{PacketRecord, PingStatus, PingTarget, StatsSnapshot},
    strings::*,
    structs::{AppState, TargetDefaults},
    ui::{PopupContents, TableRow, TerminalGuard, keyboard::key_event_handler},
    utils::{make_histogram_buckets, setup_signal_handler},
};

use futures::{
    future::join_all,
    stream::{FuturesUnordered, StreamExt},
};
use miniutils::{ToDisplay, inject, simple_tabulate, templater};
use rand::{fill, random};
use ratatui::{prelude::*, widgets::*};
use std::{
    fmt::Display,
    future::Future,
    net::IpAddr,
    sync::{Arc, LazyLock},
    thread,
    time::Duration,
};
use surge_ping::{Client, PingIdentifier, PingSequence, Pinger};
use tokio::time::{self, Instant, Interval};

const PAYLOAD_RND_BYTES: usize = 32;
const GRAPH_SAMPLES: usize = 180; // 3 minutes @ default interval

type WritableLayout<'a> =
    parking_lot::lock_api::RwLockWriteGuard<'a, parking_lot::RawRwLock, ui::AppLayout>;

/* -------------------------------------------------------------------------- */

/// Create [PingTarget] instances for each IP address.
fn make_targets(addrs: &[IpAddr], defaults: &TargetDefaults) -> Vec<PingTarget> {
    addrs
        .iter()
        .map(|addr| PingTarget::new(*addr, defaults.histsize, defaults.detailed, defaults.paused))
        .collect()
}

/**
Helper to mark a ping as sent and calculate the next sequence number.
Update sent timestamp as late as possible before sending so that the time
difference is minimized. There will still  be some delay due to task
scheduling etc, but this should be negligible compared to network latencies.
*/
#[inline]
fn mark_sent_and_next_seq(tgt: &PingTarget) -> u16 {
    let mut stats = tgt.data.write();
    /*
    update sent count here to make sure it's incremented before
    sending so that the main sent count stays accurate even if
    ping fails or we get out of order replies etc
    */
    let sent: u64 = stats.sent;
    stats.sent += 1;

    // calculate the 16-bit sequence number from sent count,
    // since 2^16 is the max for ICMP sequence numbers
    let seq: u16 = (sent % 65_536) as u16;

    // store last sent seq and timestamp for master reference
    stats.last_seq = seq;
    stats.last_sent = Some(std::time::Instant::now());
    seq
}

/// Helper to clone the payload slice into a new one, which we can randomize if
/// needed. Internally, [Arc::make_mut] will perform a clone-on-write if necessary.
#[inline]
fn build_payload(app: &AppState) -> Arc<[u8]> {
    match app.defaults.randomize {
        true => {
            let mut payload: Arc<[u8]> = app.payload.clone();
            let payload: &mut [u8] = Arc::make_mut(&mut payload);
            /*
            Can't use a thread-local RNG here (for performance)
            because it's not Send'able across await points.
            However, we can spare CPU time by randomizing only
            the first 32 bytes of the payload, which should be plenty.
            And we already know the payload must be 32 bytes minimum.
            */
            fill(&mut payload[..PAYLOAD_RND_BYTES]);
            payload.into()
        }
        false => app.payload.clone(),
    }
}

/// Build a future that performs a single ping for the given target.
fn build_ping_future(
    tgt: Arc<PingTarget>,
    c: Arc<Client>,
    app: Arc<AppState>,
    id: PingIdentifier,
) -> impl Future<Output = ()> + Send + 'static {
    let pl: Arc<[u8]> = build_payload(&app);

    async move {
        let mut pinger: Pinger = c.pinger(tgt.addr, id).await;
        pinger.timeout(app.defaults.timeout);

        let seq: u16 = mark_sent_and_next_seq(&tgt);
        let rec: PacketRecord = PacketRecord::new(seq);
        let res = pinger.ping(PingSequence(seq), &pl).await;
        tgt.update_stats(res, rec).await;
    }
}

/// Prepare and spawn a single ping task for the given target.
async fn ping_task(tgt: Arc<PingTarget>, c: &Arc<Client>, app: &Arc<AppState>, id: PingIdentifier) {
    // We must create a new Pinger for each async context, since otherwise we'll have
    // to wait for the previous ping to complete before sending the next one.
    let mut pinger: Pinger = c.pinger(tgt.addr, id).await;
    pinger.timeout(app.defaults.timeout);

    let pl: Arc<[u8]> = build_payload(&app);
    let seq: u16 = mark_sent_and_next_seq(&tgt);

    app.inc_spawned_tasks();
    tokio::spawn(async move {
        let rec: PacketRecord = PacketRecord::new(seq);
        let res = pinger.ping(PingSequence(seq), &pl).await;
        tgt.update_stats(res, rec).await;
    });
}

/// Set up a ping loop for each target.
async fn ping_loop(tgt: Arc<PingTarget>, app: Arc<AppState>) {
    let client = match tgt.addr {
        IpAddr::V4(_) => app.c_v4.as_ref().expect(ERR_V4_MISSING),
        IpAddr::V6(_) => app.c_v6.as_ref().expect(ERR_V6_MISSING),
    };
    let id: PingIdentifier = PingIdentifier(random());
    let mut ticker: Interval = time::interval(app.internal_tick);
    let mut next_ping: Instant = tokio::time::Instant::now();

    // These variables are used only if perf mode is enabled
    let interval: f64 = app.defaults.interval.as_secs_f64().max(1e-6); // 1 us min to avoid div by zero
    let timeout: f64 = app.defaults.timeout.as_secs_f64();
    let max_inflight: usize = ((timeout / interval).ceil() as usize).clamp(1, 4);
    let mut inflight = FuturesUnordered::new();

    loop {
        tokio::select! {
            biased;
            true = app.is_quitting_async() => break,
            true = tgt.is_stopped_async() => break,

            Some(_) = inflight.next(), if !inflight.is_empty() => { /* stats updated inside future */ }

            _ = ticker.tick() => {
                let now = tokio::time::Instant::now();
                if tgt.is_paused() {
                    /*
                    Adjust next ping time to not build a backlog while paused.
                    When unpaused, the next ping should be pretty much immediate
                    and subsequent pings will resume at the normal interval.
                    */
                    next_ping = now;
                    continue;
                } else if now < next_ping {
                    continue;
                }

                if app.perf() {
                    if inflight.len() >= max_inflight {
                        next_ping = now + app.defaults.interval;
                        continue;
                    }
                    inflight.push(build_ping_future(tgt.clone(), client.clone(), app.clone(), id));
                } else {
                    ping_task(tgt.clone(), &client, &app, id).await;
                }
                next_ping += app.defaults.interval;
            }
        }
    }

    // Drain outstanding pings (bounded by timeout, same practical behavior as with spawned tasks).
    // However, if the app is quitting, just abandon the tasks or we will incur delays.
    if !app.is_quitting() {
        while inflight.next().await.is_some() {}
    }
}

/// Format a single target's data into a [TableRow]. Separate fn for ease of parallelization.
async fn format_row(tgt: &Arc<PingTarget>, debug: bool, timeout: Duration) -> TableRow {
    let snap: StatsSnapshot = StatsSnapshot::new_from(&tgt, timeout);
    let status: String = match &snap.status {
        PingStatus::Error(e) if debug => e.to_string(),
        _ => snap.status.to_display(),
    };

    // Do all the (expensive) string formatting after releasing the lock.
    let mut row: TableRow = TableRow::from_iter([
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
    if debug {
        row.add_item(snap.latest_seq.to_string());
    }

    // Add full-row styling based on statuses
    if tgt.is_stopped() {
        row.set_style_all(Style::new().dim().italic().crossed_out());
    } else if tgt.is_paused() {
        row.set_style_all(Style::new().dim().italic());
    } else {
        match snap.status {
            PingStatus::Error(_) => {
                row.set_style_all(Style::new().on_red());
            }
            PingStatus::NotReachable => {
                row.set_style_all(Style::new().light_red());
            }
            PingStatus::Timeout => {
                row.set_style_all(Style::new().light_magenta());
            }
            PingStatus::Lossy => {
                row.set_style_all(Style::new().light_yellow());
            }
            PingStatus::Laggy | PingStatus::Flappy => {
                row.set_style_all(Style::new().yellow());
            }
            _ => {}
        }
    }
    row
}

/**
Gather current stringified data from some or all targets.

For large target lists, gathering data for all can be slow, hence
it makes sense to only gather data for the currently visible targets
in the TUI table. This function supports both modes via the `all` param.
*/
async fn gather_target_data(state: &AppState, all: bool) -> Vec<TableRow> {
    let vp = state.viewport();
    let tgts = state.targets.read();

    // Full list requested, or we have fewer targets than rows
    if all || !vp.needs_paging() {
        return join_all(
            tgts.iter()
                .map(|t| format_row(t, state.debug, state.defaults.timeout)),
        )
        .await;
    }

    /*
    Since the target list is longer than the table height, we have to fake
    empty rows for Ratatui to render the full table. We create those to fill
    the space. Depending on the offset, we need to do this at the start, end
    or both. Chaining the Vec iterators makes this fairly straightforward.
    */
    let pre: Vec<TableRow> = vec![TableRow::new(); vp.offset];
    let post: Vec<TableRow> = vec![TableRow::new(); vp.targets - vp.end_pos];
    let visible: &[Arc<PingTarget>] = &tgts[vp.offset..vp.end_pos];

    // Combine the Vec iterators
    pre.into_iter()
        .chain(
            join_all(
                visible
                    .iter()
                    .map(|t| format_row(t, state.debug, state.defaults.timeout)),
            )
            .await
            .into_iter(),
        )
        .chain(post.into_iter())
        .collect()
}

/* -------- Static Ratatui constructs for render_frame() -------- */
static ST_REV: Style = Style::new().reversed();
static ST_COL: Style = Style::new().bg(Color::Indexed(240));
static ST_CYAN: Style = Style::new().fg(Color::Cyan);
static ST_GREEN: Style = Style::new().fg(Color::Green);
static BORDERS: Block = Block::bordered();
static BLK_PAD_L1: Block = Block::new().padding(Padding::left(1));
static BLK_NFO_INNER: Block = Block::new()
    .borders(Borders::TOP)
    .border_type(BorderType::QuadrantOutside)
    .title_alignment(Alignment::Center);
static SCROLLBAR: Scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight);
static BLK_NFO: LazyLock<Block> = LazyLock::new(|| BORDERS.clone().title_top(INFO_INFO));
static BLK_POPUP: LazyLock<Block> = LazyLock::new(|| {
    BORDERS
        .clone()
        .padding(Padding::horizontal(1))
        .border_type(BorderType::Double)
        .border_style(Color::Indexed(190))
});
static BLK_HELP: LazyLock<Block> = LazyLock::new(|| {
    BORDERS
        .clone()
        .title(INFO_HELP)
        .padding(Padding::horizontal(1))
        .border_type(BorderType::QuadrantOutside)
        .border_style(Color::Indexed(216))
});
static PARA_NFO_SEL: LazyLock<Paragraph> = LazyLock::new(|| Paragraph::new(INFO_SELECT));
static PARA_NO_TGTS: LazyLock<Paragraph> = LazyLock::new(|| Paragraph::new(INFO_NO_TGTS));
static PARA_NO_DATA: LazyLock<Paragraph> = LazyLock::new(|| Paragraph::new(INFO_NO_RTT));
static DATASET_BASE: LazyLock<Dataset> = LazyLock::new(|| {
    Dataset::default()
        .name(INFO_RTT)
        .marker(symbols::Marker::Braille)
        .style(ST_CYAN)
        .graph_type(GraphType::Line)
});
static HISTOGRAM_BASE: LazyLock<BarChart> = LazyLock::new(|| {
    BarChart::default()
        .bar_width(1)
        .bar_gap(0)
        .direction(Direction::Horizontal)
});

/**
Render the current frame. Display will be updated as soon as this function completes.

NOTE: the order of statements inside the function body is arranged such that
the time of holding the targets lock is minimized to reduce contention.
*/
fn render_frame(frame: &mut Frame, state: &AppState, data: &[TableRow]) {
    let layout = &mut state.layout.write();
    layout.maybe_update(frame.area(), &data);
    let tgts = state.targets.read();
    let num_tgts: usize = tgts.len();
    let selected = layout
        .tablestate
        .selected()
        // TableState::selected can return an out-of-bounds index, so clamp it here.
        .and_then(|i: usize| tgts.get(i.min(num_tgts.saturating_sub(1))));

    /* -------- Info areas -------- */
    let w_info_upper = if let Some(t) = selected {
        Paragraph::new(templater!(
            INFO_TARGET,
            t.to_string(),
            &t.rev,
            t.ptr().to_string(),
            t.rev_ptr().to_string(),
            t.est_distance_str(state.distance_stretch_factor),
            t.hops().to_string(),
        ))
    } else {
        PARA_NFO_SEL.clone()
    }
    .block(BLK_NFO.clone());

    /* -------- Recent RTT graph and histogram for selected target. -------- */
    if let Some(target) = selected {
        let rtt_data: Vec<(f64, f64)> = target.get_recent_rtts(GRAPH_SAMPLES);
        // release targets read lock here, not needed anymore
        drop(tgts);

        // Carve out areas for graph and histogram and define the inner Block.
        // We need to do this since the outer enclosing Block includes borders.
        let a_graph = BLK_NFO.inner(layout.i_upper_graph);
        let a_histo = BLK_NFO.inner(layout.i_upper_histo);
        let b_graph = BLK_NFO_INNER.clone().title(INFO_RTT_G);
        let b_histo = BLK_NFO_INNER.clone().title(INFO_RTT_H);

        if !rtt_data.is_empty() {
            let samples: usize = rtt_data.len();
            let sample_t: f64 = samples as f64 * state.defaults.interval.as_secs_f64();
            let mut values: Vec<f64> = Vec::with_capacity(samples);

            // get values and min/max RTT (for graph scaling) in one pass
            let (mut min_rtt, mut max_rtt) = rtt_data
                .iter()
                .map(|&(_, y)| {
                    values.push(y);
                    y
                })
                .fold((f64::INFINITY, 0.0), |(min, max), y| {
                    (f64::min(min, y), f64::max(max, y))
                });

            // round max_rtt up and min_rtt down for cleaner graph axis labels
            max_rtt = (max_rtt * 10.0).ceil() / 10.0;
            if min_rtt < 0.5 {
                min_rtt = 0.0;
            } else {
                min_rtt = (min_rtt * 10.0).floor() / 10.0;
            };

            // RTT line graph widget
            let w_rtt_chart = Chart::new(vec![DATASET_BASE.clone().data(&rtt_data)])
                .block(b_graph)
                .x_axis(
                    Axis::default()
                        .bounds([0.0, samples as f64 - 1.0])
                        .labels([format!("-{sample_t:.0}s").bold(), INFO_NOW.bold()]),
                )
                .y_axis(Axis::default().bounds([min_rtt, max_rtt]).labels([
                    format!("{min_rtt:.1}").bold().cyan(),
                    format!("{max_rtt:.1}").bold().cyan(),
                ]));

            // RTT histogram widget
            let bars = make_histogram_buckets(values, 8)
                .iter()
                .map(|b| {
                    Bar::default()
                        .value(b.count)
                        .label(format!("{:.2} - {:.2}", b.low, b.high))
                        .style(ST_GREEN)
                })
                .collect::<Vec<_>>();
            let w_histogram = HISTOGRAM_BASE
                .clone()
                .block(b_histo)
                .data(BarGroup::new(bars));

            frame.render_widget(w_rtt_chart, a_graph);
            frame.render_widget(w_histogram, a_histo);
        } else {
            frame.render_widget(PARA_NO_DATA.clone().block(b_graph), a_graph);
            frame.render_widget(PARA_NO_DATA.clone().block(b_histo), a_histo);
        }
    } else {
        drop(tgts);
    }

    /* -------- Lower info area - bottom right corner -------- */
    #[rustfmt::skip]
    let w_info_lower = Paragraph::new(templater!(
        INFO_STATE,
        state.defaults.interval.as_millis(),
        state.defaults.timeout.as_millis(),
        state.payload.len(),
        if state.defaults.randomize { INFO_RAND } else { "" },
        state.spawned_tasks()
    ))
    .block(BLK_PAD_L1.clone());

    /* -------- CPU & process info - bottom line, right side -------- */
    let w_procinfo = Line::from(templater!(
        INFO_CPU,
        format!("{:>7}", state.pi.cpu_str()),
        state.pi.mem_str(),
        state.pi.pid,
    ))
    .alignment(Alignment::Right);

    /* -------- Status line - bottom line, left side -------- */
    state.status_line.replace(match state.debug {
        true => templater!(
            INFO_DEBUG,
            data.len(),
            &layout.tablestate.offset(),
            &layout
                .tablestate
                .selected()
                .map_or("none".into(), |i| i.to_string())
        ),
        false => APP_RUNNING.into(),
    });

    /* -------- Data table -------- */
    if num_tgts == 0 {
        frame.render_widget(PARA_NO_TGTS.clone().block(BORDERS.clone()), layout.table);
    } else {
        let b_tbl = BORDERS
            .clone()
            .title_bottom(Line::from(templater!(INFO_TGTS, num_tgts)));
        let w_table = Table::new(
            data.iter().map(|r| <&TableRow as Into<Row>>::into(r)),
            &layout.tbl_constraints,
        )
        .header((&state.headers).into())
        .column_spacing(layout.tbl_colspacing)
        .block(b_tbl)
        .row_highlight_style(ST_REV)
        .column_highlight_style(ST_COL);
        frame.render_stateful_widget(w_table, layout.table, &mut layout.tablestate);

        /*
        Render scrollbar if we have enough targets. Drawn over the right-side table border
        since we use `layout.table` and not `b_tbl.inner()` as the area (saves 1 column! :D).
        The scrollbar state is throwaway since there's no benefit to storing it in the layout:
        - the setters consume `self`, so we can't reuse the same instance anyway
        - doesn't support scrolling by N rows, which we need
        - would introduce more complexity in the keyboard handler
        - we already have the relevant info in the table state
        */
        if num_tgts > layout.tbl_usable_rows() {
            let bar_pos = layout
                .tablestate
                .selected()
                .unwrap_or_else(|| layout.tablestate.offset());
            frame.render_stateful_widget(
                SCROLLBAR.clone().style(ST_CYAN),
                layout.table,
                &mut ScrollbarState::new(num_tgts).position(bar_pos),
            );
        }
    }

    // Render all components. Order matters for layering; later ones overwrite earlier ones,
    // faking z-index behavior even though we're not working with "real" windows.
    frame.render_widget(&state.title, layout.title);
    frame.render_widget(w_procinfo, layout.status_r);
    // NOTE: we're writing to `info_upper` instead of `i_upper_text` to get the border around it all.
    frame.render_widget(w_info_upper, layout.info_upper);
    frame.render_widget(w_info_lower, layout.info_lower);
    frame.render_widget(state.status_line.clone().bold().as_line(), layout.status_l);

    // Finally render the popus if any are visible (on top of everything else)
    render_popups(frame, state, layout);
}

/// Render the popups: text box, help.
fn render_popups(frame: &mut Frame, state: &AppState, layout: &mut WritableLayout) {
    /* -------- Text popup -------- */
    if layout.popup_visible {
        let contents = &*state.popup_contents.read();
        if !contents.is_empty() {
            frame.render_widget(Clear, layout.popup);

            match contents {
                PopupContents::Buffer(_) => {
                    let num = state.logger.len();
                    let b_popup = BLK_POPUP.clone().title(templater!(INFO_LOG, num));

                    if !(num > layout.popup_usable_rows()) {
                        // no statefulness needed here
                        frame.render_widget(contents.to_list().block(b_popup), layout.popup)
                    } else {
                        frame.render_stateful_widget(
                            contents.to_list().block(b_popup),
                            layout.popup,
                            &mut layout.liststate,
                        );

                        // Render scrollbar for log popup
                        let bar_pos = layout
                            .liststate
                            .selected()
                            .unwrap_or_else(|| layout.liststate.offset());
                        frame.render_stateful_widget(
                            SCROLLBAR.clone(),
                            layout.popup,
                            &mut ScrollbarState::new(num).position(bar_pos),
                        );
                    }
                }

                PopupContents::Multiline(_) => {
                    frame.render_widget(contents.to_list().block(BLK_POPUP.clone()), layout.popup)
                }

                _ => frame.render_widget(contents.to_para().block(BLK_POPUP.clone()), layout.popup),
            }
        }
    }

    /* -------- Help popup -------- */
    if layout.help_visible {
        frame.render_widget(Clear, layout.help);
        frame.render_widget(
            state.help_contents.to_para().block(BLK_HELP.clone()),
            layout.help,
        );
    }
}

/* -------------------------------------------------------------------------- */

#[tokio::main(worker_threads = 8)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let conf: MpConfig = MpConfig::parse().await?;
    let app: AppState = AppState::from_conf(&conf);
    let tgts: Vec<PingTarget> = make_targets(&conf.addrs, &app.defaults);
    let app: Arc<AppState> = app.build(tgts)?;

    // Spawn ping tasks
    {
        let mut tasks = app.tasks.write();
        for tgt in app.targets.read().iter() {
            tasks.push(tokio::spawn(ping_loop(tgt.clone(), app.clone())));
            app.inc_spawned_tasks();
        }
    }

    // Full-console TUI initialization - the RAII guard will clean up on drop
    setup_signal_handler(app.quit.clone());
    let mut guard: TerminalGuard = TerminalGuard::new(app.ui_interval, app.logger.clone())?;
    let mut tick: Interval = time::interval(app.internal_tick);

    // Start the key event handling thread
    let app_clone: Arc<AppState> = app.clone();
    let kev_handle = thread::spawn(move || key_event_handler(app_clone));

    // Main display loop
    loop {
        tokio::select! {
            biased; // preferentially handle quit condition first, then rest in order
            true = app.is_quitting_async() => break,
            true = app.ui_refresh_elapsed_async() => {
                // Gather data for display and render the frame
                let data = gather_target_data(&app, false).await;
                guard.term.draw(|frame: &mut Frame| render_frame(frame, &app, &data))?;
                app.ui_schedule_next_refresh();
            },
            _ = app.key_event.notified() => {
                // Immediate refresh on key event. NOTE: don't reschedule next refresh!
                let data = gather_target_data(&app, false).await;
                guard.term.draw(|frame: &mut Frame| render_frame(frame, &app, &data))?;
                // sleep a little to avoid busy looping during key event bursts
                tokio::time::sleep(Duration::from_millis(5)).await;
            },
            _ = tick.tick() => { /* no-op, just to keep the select! happy */ }
        }
    }

    // Cleanup
    drop(guard); // explicitly drop TUI guard to restore terminal so we can print
    if app.debug {
        eprintln!("{INFO_QUITTING}");
    }
    kev_handle.join().expect(ERR_KEV_JOIN);
    let mut tasks = app.tasks.write();
    join_all(tasks.iter_mut()).await;

    // Print final stats
    let data: Vec<TableRow> = gather_target_data(&app, true).await;
    // Display the same rows as were visible in the TUI
    let vp = app.viewport();
    simple_tabulate(&data[vp.offset..vp.end_pos], Some(&app.headers.strings()))
        .iter()
        .for_each(|line| println!("{line}"));
    Ok(())
}
