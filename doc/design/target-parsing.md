# Target parsing & DNS resolution

Users supply targets as space-separated tokens that can be:

- A single IPv4 / IPv6 address: `8.8.8.8`, `::1`
- A CIDR range: `10.0.0.0/28`
- An inclusive range: `172.16.1.1-10`
- A DNS name: `dns.google`

`parse_ip_or_range` (from the `miniutils` dependency) handles the first
three; anything that fails to parse is then tried as a DNS name through
`hickory-resolver`. The same pipeline is used both at startup and at
runtime from the add-target dialog.

## Two-phase pipeline

```text
collect_targets(targets, exclude?, resolver, logger)
  ├ parse_ip_addresses(...)         // utils.rs:117
  │     ├ expand IP / CIDR / range  → all_addrs (deduped, ordered)
  │     ├ apply exclusions          → drops parsed addrs in `excluded`
  │     └ collect failures          → `failed` Set<String>
  └ resolve_names(failed, resolver) // utils.rs:226
        ├ lookup_ip(name)           → (name, Vec<IpAddr>) on success
        └ otherwise insert into     → `unresolved`
returns CollectedTargets {
    addrs, seen, excluded, resolved, unresolved
}
```

Defined in `src/utils.rs:294-318`. Resolved IPs are appended to the
`addrs` Vec only if they weren't already covered by parsed entries
(`utils.rs:309-315`).

`addrs` is a `Vec<IpAddr>` in first-seen order (so the TUI table preserves
the order the user listed things); `seen` is the matching `HashSet` so
callers don't have to re-derive it.

## Exclusions apply to parsed IPs only

The `--exclude` list is intersected with the parsed-IP set. DNS-resolved
results are **not** filtered through exclusions — this matches the
behaviour from before runtime add-target existed, so the rules are
predictable. If you exclude `10.0.0.0/24` and then add `myhost.internal`
which resolves to `10.0.0.5`, you get `10.0.0.5`. If you don't want
that, exclude `myhost.internal` by name won't do anything either —
exclusions are an IP-only filter.

Exclusions are honored even when they would remove *everything* (you get
a warning and an empty target list, not your full input back), and after
they're applied `seen` is updated so it always mirrors `addrs` membership
exactly — the `debug_assertions` check in `MpConfig::parse` relies on
that invariant.

`resolve_names` resolves the leftover non-IP tokens concurrently (up to
`MAX_CONCURRENT_DNS` in-flight lookups via `buffer_unordered`), so
startup latency doesn't scale linearly with the number of hostnames. The
input is a `HashSet`, so the arbitrary completion order changes nothing.

## Initial startup vs. runtime add

`MpConfig::parse()` (`src/args.rs:182-330`) calls `collect_targets` once
during CLI parsing. The runtime add-target dialog calls
`pinger::collect_and_spawn` (`src/pinger.rs:195-225`), which wraps the
same pipeline, folds new name↔IP mappings into the shared `Resolved` map
on `AppState`, builds fresh `PingTarget`s with current `TargetDefaults`,
adds them via `AppState::add_targets`, and spawns ping loops for the
survivors.

`AppState::add_targets` (`structs.rs:236-272`) deduplicates against the
existing target list under the `targets` write lock and stamps the
hostname (from `Resolved`) on each new `PingTarget` before pushing.

## The `Resolved` map

`Resolved` (`structs.rs:452-517`) is a bidirectional map: name → set of
IPs, IP → name. It's used for two things:

- Display: when a target's hostname is set, the table shows
  `1.2.3.4 (dns.google)` instead of just the IP.
- Reverse-lookup hint: when a user adds `dns.google` and it resolves to
  several IPs, each gets the same `dns.google` name attached. If the
  same IP shows up later from a different name, the IP→name mapping is
  overwritten (last-add wins — there's a comment about this at
  `structs.rs:449-451`).

It lives inside `RwLock<Resolved>` on `AppState` because runtime
additions need write access while the rest of the program reads it. To
avoid a lock-order bug, `collect_and_spawn` finishes its write to
`AppState::resolved` *before* calling `add_targets` (which then reads
the map under its own lock — see comment at `pinger.rs:203-213`).

## DNS resolver configuration

The resolver is built in `MpConfig::parse()` (`args.rs:207-257`) and
shared via `Arc<TokioResolver>` on `AppState.resolver`. Configuration
combines `--dns-servers` and `--dns-timeout` with `hickory-resolver`'s
`read_system_conf()`. The combinations are:

| `--dns-servers` | `--dns-timeout` | Resolver |
|---|---|---|
| not set | default (5s) | system config + system options |
| not set | custom | system config + custom timeout |
| set | default | custom servers + default options |
| set | custom | custom servers + custom timeout |

The DNS timeout is clamped to `[1s, 10s]` (`args.rs:201-205`). The
resolver is **also** used at runtime for PTR lookups when the user
hits Enter on a target — see
[per-target-data](per-target-data.md).

## Interval/timeout clamping

While we're in `MpConfig::parse`, two other clamps happen
(`args.rs:299-327`):

- `interval` clamped to `[10ms, 10s]`
- `timeout` clamped to `[10ms, 5s]`, and additionally to `interval * 4`

The `timeout ≤ 4×interval` rule limits how many pending pings can stack
up per target. Without it, a user could set `--interval 0.01 --timeout 5`
and accumulate 500 inflight pings per target. With it, even adversarial
combinations cap at 4 — which is also the upper bound used by
`max_inflight` in perf mode (`pinger.rs:124`).

## File map

- `src/utils.rs:117-192` — `parse_ip_addresses`.
- `src/utils.rs:226-255` — `resolve_names`.
- `src/utils.rs:266-318` — `CollectedTargets` and `collect_targets`.
- `src/utils.rs:196-212` — `reverse_name` (PTR-style name from `IpAddr`).
- `src/args.rs:182-330` — `MpConfig::parse` including resolver build,
  the initial pipeline run, and the interval/timeout clamps.
- `src/structs.rs:444-517` — `Resolved` map.
- `src/pinger.rs:195-225` — runtime entry point used by the add-target
  dialog.
