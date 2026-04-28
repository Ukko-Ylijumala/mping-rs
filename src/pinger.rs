// Copyright (c) 2025-2026 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Per-target async ping loop and the runtime "add new targets" entry point.

use crate::{
    pingdata::{PacketRecord, PingTarget},
    strings::*,
    structs::AppState,
    utils::{CollectedTargets, collect_targets},
};
use futures::stream::{FuturesUnordered, StreamExt};
use rand::{fill, random};
use std::{future::Future, net::IpAddr, sync::Arc};
use surge_ping::{Client, PingIdentifier, PingSequence, Pinger};
use tokio::time::{self, Instant, Interval};

const PAYLOAD_RND_BYTES: usize = 32;

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

/* -------------------------------------------------------------------------- */

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

    let pl: Arc<[u8]> = build_payload(app);
    let seq: u16 = mark_sent_and_next_seq(&tgt);

    app.spawn(async move {
        let rec: PacketRecord = PacketRecord::new(seq);
        let res = pinger.ping(PingSequence(seq), &pl).await;
        tgt.update_stats(res, rec).await;
    });
}

/* -------------------------------------------------------------------------- */

/// Set up a ping loop for each target.
pub(crate) async fn ping_loop(tgt: Arc<PingTarget>, app: Arc<AppState>) {
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
                    and subsequent pings will resume at normal pace.
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
                    ping_task(tgt.clone(), client, &app, id).await;
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

/* -------------------------------------------------------------------------- */

/**
Spawn a [ping_loop] task for each newly-added target and stash the [tokio::task::JoinHandle]
in `app.tasks` so shutdown can join them.
*/
pub(crate) fn spawn_ping_loops(app: &Arc<AppState>, new_targets: &[Arc<PingTarget>]) {
    if new_targets.is_empty() {
        return;
    }
    let mut tasks = app.tasks.write();
    for tgt in new_targets {
        tasks.push(app.spawn(ping_loop(tgt.clone(), app.clone())));
    }
}

/**
Runtime entry point used by the "add target" dialog: parse and resolve the
user-supplied `targets`/`exclude` strings, fold any new DNS resolutions into
[AppState::resolved], then build and spawn [PingTarget]s using the current
[crate::structs::TargetDefaults].

Returns the raw [CollectedTargets] so the caller can report counts and surface
any unresolved strings back into the dialog.
*/
pub(crate) async fn collect_and_spawn(
    app: &Arc<AppState>,
    targets: &[String],
    exclude: Option<&[String]>,
    paused: bool,
) -> CollectedTargets {
    let collected = collect_targets(targets, exclude, &app.resolver, app.logger.as_ref()).await;

    /*
    Fold the new name->IP mappings into the shared map *before* add_targets,
    since add_targets reads from it to set hostnames on freshly-built targets.
    Scoped block so the write lock is released before the read inside add_targets.
    */
    if !collected.resolved.is_empty() {
        let mut resolved = app.resolved.write();
        for (name, ips) in &collected.resolved {
            resolved.add(name, ips);
        }
    }

    let new_targets: Vec<PingTarget> = collected
        .addrs
        .iter()
        .map(|addr| PingTarget::new(*addr, app.defaults.histsize, app.defaults.detailed, paused))
        .collect();

    let added = app.add_targets(new_targets);
    spawn_ping_loops(app, &added);

    collected
}
