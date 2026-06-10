# Rolling latency window

`LatencyWindow` (`src/latencywin.rs`) is the rolling-RTT data structure used
by every `PingTarget`. The whole point is that *all* the statistics the TUI
reads — count, sum, mean, min, max, population variance, population stdev —
are O(1) on `push`, so a renderer pulling stats from thousands of targets
doesn't degrade as window size grows.

## Storage

```text
LatencyWindow {
    cap:    usize          // window capacity, clamped to MIN_WINDOW_SIZE = 3
    buf:    Vec<u32>       // ring buffer of µs RTTs
    head:   usize          // next write position
    len:    usize          // current size, ≤ cap
    sum:    f64            // running Σ x
    sum_sq: f64            // running Σ x²
    variance: f64          // last computed population variance
    stdev:    f64          // last computed population stdev
    minq:   VecDeque<(u32, usize)>  // monotonic increasing  (value, sample idx)
    maxq:   VecDeque<(u32, usize)>  // monotonic decreasing  (value, sample idx)
    index:  usize          // monotonically-increasing global sample index
    min_ever: Option<u32>  // all-time minimum — survives window eviction
}
```

RTTs are stored as `u32` microseconds (the unit `update_stats` writes into
the window — `pingdata.rs:243`). Accumulators are `f64` so the variance
formula doesn't lose precision over the µs–ms range expected for network
latency. The capacity is clamped to a minimum of 3 because variance below
that is not meaningful.

## O(1) push

`push(val)` (`latencywin.rs:78`) does:

1. Bump the global `index` (with wraparound — `wrapping_add(1)`).
2. If growing, write to `buf[head]`, bump `head`, increment `len`, add
   `val` and `val²` to the running sums.
   If full, also subtract the evicted oldest value's contribution.
3. Recompute population variance with the **computational formula**:
   `var = (Σx² − (Σx)² / n) / n`. Negative values can appear due to
   floating-point cancellation when variance is near zero — clamped to 0
   (`latencywin.rs:114-117`).
4. Drop from the front of `minq`/`maxq` any entries whose original sample
   index has aged out of the window.
5. Drop from the back of `minq` any entries with values ≥ `val` (they can
   never be the min while `val` is alive), then push `(val, idx)`.
6. Mirror that for `maxq` (pop entries with values ≤ `val`).

The monotonic-deque trick is the standard sliding-window min/max
algorithm: front is always the current min (or max); back is always
prunable when a new value dominates. Amortized O(1) — each element is
pushed and popped from each deque at most once.

## Reads

- `len`, `is_empty`, `maxlen` — trivial.
- `mean()` — `Σx / n` from cached sums.
- `min()` / `max()` — front of `minq` / `maxq`. O(1). These are *windowed*:
  the all-time best sample expires after `cap` further pushes.
- `min_ever()` — the all-time minimum, updated on every `push` and only
  reset by `clear()`. This is what distance estimation uses
  (`est_distance_km`), because RTT noise is one-sided (queueing only adds
  latency), so the best sample ever seen is the tightest propagation-delay
  bound — letting it expire with the window would make the distance
  estimate drift upwards. The table's Min column stays windowed on purpose:
  it answers "how is the link lately", not "where is this".
- `mean_min_max()` — single-call helper used by `StatsSnapshot`.
- `variance()` / `stdev_pop()` — return the cached value.
- `stdev_n(window)` — a Bessel-corrected (sample) stdev over the last
  `window` samples. This *does* a backward scan, so it's O(window) — used
  only when the caller explicitly wants a smaller window than the full
  rolling history.
- `recent_samples(n)` — used for the RTT line graph; returns the last *n*
  values in chronological order. Walks the ring buffer.

## Numerical considerations

The computational variance formula `(Σx² − (Σx)²/n) / n` is fast and O(1),
but it can lose precision when the data is large relative to the variance
(catastrophic cancellation). For typical ping RTTs (microseconds to tens of
milliseconds with µs-scale jitter) this is fine — the doc comment on the
struct (`latencywin.rs:15-30`) calls this out. If anyone ever feeds it
nanosecond resolution or huge values, switch to Welford's algorithm
instead.

The guard in step 3 — clamping negative variance to 0 — is *only* there to
catch floating-point drift; if you see large negative values, something is
wrong.

## Clear and rebuild

`clear()` zeroes everything in place; the ring buffer is filled with `0u32`,
both deques are cleared, and `min_ever` resets to `None`. The `index` resets
to 0 — this is fine because all in-flight entries in the deques were just
dropped. `Command::ResetTgtStats` (`structs.rs:349`) is the only place that
calls it — which doubles as the user's "re-measure from here" gesture when
`min_ever` has gone stale (anycast POP change, laptop moved networks).

## Testing

`latencywin.rs:329-451` has a non-trivial test suite covering eviction,
ring-buffer wraparound, and the deque pruning corner cases. If you change
the data structure, run them — they're the only tests in the repo right
now and they exist because this code has subtle edge cases.

## File map

- `src/latencywin.rs` — everything.
- `src/pingdata.rs:243` — the single producer (`rtts.push(...)` inside
  `update_stats`).
- `src/pingdata.rs:518-528` — `get_recent_rtts` (consumer for the line
  graph).
