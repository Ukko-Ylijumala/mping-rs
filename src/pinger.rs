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
use std::{future::Future, net::IpAddr, sync::Arc, time::Duration};
use surge_ping::{Client, PingIdentifier, PingSequence, Pinger};
use tokio::time::{self, Interval, MissedTickBehavior};

const PAYLOAD_RND_BYTES: usize = 32;
const MAX_INFLIGHT: usize = 5; // perf mode: max pending pings per target

/**
Helper to mark a ping as sent and calculate the next sequence number.
Update sent timestamp as late as possible before sending so that the time
difference is minimized. There will still  be some delay due to task
scheduling etc, but this should be negligible compared to network latencies.

Returns the probe's [PacketRecord], stamped under the same lock that bumps
`sent`: a stats reset then orders cleanly before or after the probe, which
is what lets [PingTarget::update_stats] drop results from before a reset.
*/
#[inline]
fn mark_sent_and_next_seq(tgt: &PingTarget) -> PacketRecord {
    let mut stats = tgt.data.write();
    /*
    update sent count here to make sure it's incremented before
    sending so that the main sent count stays accurate even if
    ping fails or we get out of order replies etc
    */
    stats.sent += 1;

    // Sequence numbers come from a dedicated wrapping counter, NOT from `sent`:
    // send errors decrement `sent`, and reusing a seq that is still in flight
    // makes surge-ping reject the ping as an identical request.
    let seq: u16 = stats.next_seq;
    stats.next_seq = seq.wrapping_add(1);

    // store last sent seq and timestamp for master reference
    let rec: PacketRecord = PacketRecord::new(seq);
    stats.last_seq = seq;
    stats.last_sent = Some(rec.sent);
    rec
}

/// Helper to clone the payload slice into a new one, which we can randomize if
/// needed. Internally, [Arc::make_mut] will perform a clone-on-write if necessary.
#[inline]
fn build_payload(app: &AppState) -> Arc<[u8]> {
    match app.defaults.randomize {
        true => {
            let mut payload: Arc<[u8]> = app.payload.clone();
            /*
            Can't use a thread-local RNG here (for performance)
            because it's not Send'able across await points.
            However, we can spare CPU time by randomizing only
            the first 32 bytes of the payload, which should be plenty.
            And we already know the payload must be 32 bytes minimum.
            */
            fill(&mut Arc::make_mut(&mut payload)[..PAYLOAD_RND_BYTES]);
            payload // the make_mut copy itself - no second allocation
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

        let rec: PacketRecord = mark_sent_and_next_seq(&tgt);
        let res = pinger.ping(PingSequence(rec.seq), &pl).await;
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
    let rec: PacketRecord = mark_sent_and_next_seq(&tgt);

    app.spawn(async move {
        let res = pinger.ping(PingSequence(rec.seq), &pl).await;
        tgt.update_stats(res, rec).await;
    });
}

/**
Perf-mode bound on concurrently pending pings per target.

floor + 1, not ceil: at an integer ratio (the default 2s / 1s) the oldest
ping times out a hair *after* the tick that wants its slot, so ceil would
skip every (ratio + 1)th probe to an unresponsive target. Args caps the
timeout at 4 intervals, hence at most 5 in flight.
*/
fn max_inflight(interval: Duration, timeout: Duration) -> usize {
    let interval: f64 = interval.as_secs_f64().max(1e-6); // 1 us min to avoid div by zero
    ((timeout.as_secs_f64() / interval).floor() as usize + 1).clamp(1, MAX_INFLIGHT)
}

/* -------------------------------------------------------------------------- */

/**
Set up a ping loop for each target.

The loop sleeps on an [Interval] running at the ping interval itself, so
pings go out on schedule (no quantization to a coarser polling tick), and
every other wakeup is event-driven: shutdown, stop, resume from pause and
perf-mode completions. [MissedTickBehavior::Skip] drops ticks missed during
a stall instead of bursting catch-up pings.
*/
pub(crate) async fn ping_loop(tgt: Arc<PingTarget>, app: Arc<AppState>) {
    let client = match tgt.addr {
        IpAddr::V4(_) => app.c_v4.as_ref().expect(ERR_V4_MISSING),
        IpAddr::V6(_) => app.c_v6.as_ref().expect(ERR_V6_MISSING),
    };
    let id: PingIdentifier = PingIdentifier(random());
    let mut ticker: Interval = time::interval(app.defaults.interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // These variables are used only if perf mode is enabled
    let max_inflight: usize = max_inflight(app.defaults.interval, app.defaults.timeout);
    let mut inflight = FuturesUnordered::new();

    loop {
        // While paused the ticker isn't polled at all; the resume wakeup re-arms it.
        let paused: bool = tgt.is_paused();
        tokio::select! {
            biased;
            _ = app.shutdown.cancelled() => break,
            _ = tgt.stopped() => break,

            Some(_) = inflight.next(), if !inflight.is_empty() => { /* stats updated inside future */ }

            // ping immediately on resume, then at normal pace from there
            _ = tgt.resumed(), if paused => ticker.reset_immediately(),

            _ = ticker.tick(), if !paused => {
                if tgt.is_paused() {
                    continue; // paused since the check above
                }
                if app.perf() {
                    if inflight.len() >= max_inflight {
                        continue; // skip this probe, try again next tick
                    }
                    inflight.push(build_ping_future(tgt.clone(), client.clone(), app.clone(), id));
                } else {
                    ping_task(tgt.clone(), client, &app, id).await;
                }
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

Handles of loops that have already finished (removed / stopped targets) are
pruned first: a [tokio::task::JoinHandle] keeps its task's allocation alive,
so repeated add/remove cycles would otherwise grow memory without bound.
*/
pub(crate) fn spawn_ping_loops(app: &Arc<AppState>, new_targets: &[Arc<PingTarget>]) {
    if new_targets.is_empty() {
        return;
    }
    let mut tasks = app.tasks.write();
    tasks.retain(|h| !h.is_finished());
    for tgt in new_targets {
        tasks.push(app.spawn(ping_loop(tgt.clone(), app.clone())));
    }
}

/**
Outcome of a single "add target(s)" submission: the raw [CollectedTargets] from
the parse+resolve pipeline plus the actual handles to newly-added targets and
the count of duplicates that were skipped.

Used by the add-target dialog to populate its post-submit feedback area.
*/
#[derive(Default)]
pub(crate) struct AddOutcome {
    pub collected: CollectedTargets,
    pub added: Vec<Arc<PingTarget>>,
    pub skipped: usize,
}

/**
Runtime entry point used by the "add target" dialog: parse and resolve the
user-supplied `targets`/`exclude` strings, fold any new DNS resolutions into
[AppState::resolved], then build and spawn [PingTarget]s using the current
[crate::structs::TargetDefaults].

Returns an [AddOutcome] so the caller can report counts and surface any
unresolved strings back into the dialog.
*/
pub(crate) async fn collect_and_spawn(
    app: &Arc<AppState>,
    targets: &[String],
    exclude: Option<&[String]>,
    paused: bool,
) -> AddOutcome {
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

    let (added, skipped) = app.add_targets(new_targets);
    spawn_ping_loops(app, &added);

    AddOutcome { collected, added, skipped }
}

/* -------------------------------------------------------------------------- */

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_inflight_bounds() {
        let ms = Duration::from_millis;
        // integer ratios need one slot more than the ratio (see fn docs)
        assert_eq!(max_inflight(ms(1000), ms(2000)), 3);
        assert_eq!(max_inflight(ms(500), ms(2000)), 5);
        assert_eq!(max_inflight(ms(1000), ms(1000)), 2);
        // fractional ratios: floor + 1 == ceil
        assert_eq!(max_inflight(ms(1000), ms(2500)), 3);
        assert_eq!(max_inflight(ms(1000), ms(500)), 1);
        // clamped
        assert_eq!(max_inflight(ms(10), ms(5000)), MAX_INFLIGHT);
        assert_eq!(max_inflight(Duration::ZERO, ms(10)), MAX_INFLIGHT);
    }
}
