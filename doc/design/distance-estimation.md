# Distance estimation from RTT

mping shows an approximate physical distance to each target in the
selected-target info pane, derived from the minimum observed RTT. This
is a heuristic — many things affect RTT that have nothing to do with
geography (queueing, peering, routing inefficiency, encapsulation) — so
the value is quantized into bands rather than presented as a precise
figure.

## Formula

In `PingTarget::est_distance_km` (`src/pingdata.rs:572-580`):

```text
                  (RTT_min − t0) · v
L_geodesic  =  ────────────────────────
                   2 · s · factor
```

- `L_geodesic` — estimated one-way distance, in kilometres
- `RTT_min`   — minimum observed RTT over the rolling window, in seconds
- `t0`        — `LATENCY_FLOOR = 200 µs` baseline for non-propagation delays
- `v`         — `SPEED_KM_S = 204 000 km/s`, signal speed in single-mode fiber
                (≈ 2/3 of `c` in vacuum)
- `s`         — `STRETCH_FACTOR = 1.3`, baked-in inflation for typical
                routing inefficiency
- `factor`    — `--stretch-factor` from the CLI (default `1.0`), clamped
                to `>= 0.1` to avoid division by zero

The division by 2 converts round-trip time to one-way. The result is
floored at 1 metre.

## Stretch factor

There are *two* stretch factors stacked:

- The constant `STRETCH_FACTOR = 1.3` accounts for the average case
  where the network path isn't a straight line.
- The user's `--stretch-factor` further compresses (`> 1.0`) or
  inflates (`< 1.0`) the result.

A user who finds the default consistently overestimates distance for
their network can tune `--stretch-factor` upwards to compress the
estimates. The CLI value is stored as `app.distance_stretch_factor`
(`structs.rs:103`) and passed in on each render.

## Quantization bands

`est_distance_str` (`pingdata.rs:596-624`) wraps the kilometre value
into one of these display bands:

| Range (km) | Display |
|---|---|
| < 2 | `local` |
| 2 – 30 | `nearby` |
| 30 – 100 | `< 100 km` |
| 100 – 200 | `< 200 km` |
| 200 – 500 | `< 500 km` |
| 500 – 1 000 | `< 1000 km` |
| 1 000 – `SPEED_KM_S / 5` | `≈ <banded>+ km` (rounded down to nearest 100 km) |
| > `SPEED_KM_S / 5` (~40 800 km) | `interplanetary` |

The intermediate bands use `BAND_SIZE_KM = 100` for the quantization.
The "interplanetary" cap exists because at one-way >40 000 km a
terrestrial signal couldn't have got there in the observed RTT —
either the clocks are off, the network is doing something unusual
(satellite hop), or it's a measurement artefact.

If `RTT_min` is missing (no replies received yet) the displayed value
is the standard "missing" placeholder.

## Caveats

The doc comments at `pingdata.rs:588-591` and `pingdata.rs:566-567`
state this plainly: the estimate is rough. RTT is shaped by:

- Queueing inside switches/routers
- Peering choices (paths optimised for cost, not distance)
- Encapsulation, VPNs, tunnels
- Hop-by-hop processing time
- Cross-traffic in the local network

Long-running minimum RTT smooths most of the noise, but not all of it.
Don't use this for anything that needs to be even approximately right —
it's a "huh, that's interesting" feature, not a measurement.

## File map

- `src/pingdata.rs:34-37` — the constants (`SPEED_KM_S`,
  `STRETCH_FACTOR`, `LATENCY_FLOOR`, `BAND_SIZE_KM`).
- `src/pingdata.rs:548-580` — `est_distance_km`.
- `src/pingdata.rs:582-624` — `est_distance_str` and the bands.
- `src/structs.rs:57, 103` — `distance_stretch_factor` on `AppState`.
- `src/main.rs:217-230` — where the formatted distance string is used
  in the right-hand info pane.
