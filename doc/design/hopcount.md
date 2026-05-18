# Hop count estimation

mping estimates the number of hops to a target by sending one ICMP Echo
Request and inferring the original TTL from the TTL value in the reply.
The same code is reachable two ways:

1. As a library function used by the TUI to fill in the "hops" column.
2. As a standalone `hopcount` CLI binary (`./target/release/hopcount <ip>`).

Both live under `src/hopcount/` — `mod.rs` is the library, `main.rs` is the
binary. The binary is declared as a second `[[bin]]` in `Cargo.toml`.

## The algorithm

`determine_hops(target, timeout, debug) -> Result<(hops, received_ttl)>`
(`src/hopcount/mod.rs:23-143`):

1. Build an ICMP Echo Request packet (id `0xb00b`, seq `1`, 48-byte
   default payload) with a correct ICMP checksum.
2. Open a raw ICMP socket via `socket2` (`Type::RAW`, `Protocol::ICMPV4`
   or `ICMPV6`). Requires `CAP_NET_RAW` or root.
3. Bind to the unspecified address (`0.0.0.0` / `::`) so the OS picks
   the source IP. Set a receive timeout.
4. Send the request.
5. Read the reply into a 1500-byte buffer; convert from
   `[MaybeUninit<u8>]` via a single `transmute` against the
   `bytes_read` slice (safe because the kernel has just initialized that
   range).
6. Skip the 20-byte IPv4 header (or 40-byte IPv6 header) and verify the
   ICMP type is `EchoReply`. `DestinationUnreachable` is treated as an
   error; other types are surfaced verbatim.
7. Read the TTL byte from the IP header (offset 8 for IPv4, offset 7
   for IPv6) — `recv_buffer[8]` / `recv_buffer[7]`.
8. Estimate the original TTL by bucketing the received value:
   - `> 128` → original 255 (e.g. some Solaris-flavoured stacks)
   - `> 64`  → original 128 (Windows)
   - else    → original 64  (Linux / macOS / *BSD)
9. Return `original_TTL - received_TTL` as the hop count, plus the raw
   received TTL so the caller can sanity-check.

## "AI slop" comment block

The bottom of `mod.rs` (lines 145-196) is a commented-out IPv4-header
construction block left in place as a cautionary tale. An earlier
AI-suggested implementation wrapped the ICMP packet in a manually-built
IPv4 header, which produced a malformed packet on the wire (`tcpdump`
showed `ICMP type-#69, length 76` — the 0x45 being the first IP header
byte misinterpreted as an ICMP type). The kernel was wrapping that in
another IP header. The author left the comment for posterity; leave it
unless you have a strong reason to remove it.

## How the TUI invokes it

When the user hits Enter on a target, the keyboard branch dispatches
`Command::UpdateTgtInfo(idx)`. The handler (`structs.rs:303-346`) does
two things in parallel:

```text
self.spawn_blocking(move || tgt.determine_hops(UPDATE_TASK_TIMEOUT));
self.spawn(async move { tgt.resolve_ptr(&resolver).await; });
```

`determine_hops` must use `spawn_blocking` because (a) it does blocking
socket I/O, and (b) `tgt.determine_hops` (`pingdata.rs:275-283`)
acquires a write lock on the target's `hops` field. Scheduling that on
a runtime worker that's also holding a read lock on the same target
risks deadlock — see the load-bearing comment at
`structs.rs:309-316`.

The 3-second timeout (`UPDATE_TASK_TIMEOUT` in `structs.rs:33`) is the
deadline the helper passes through to `set_read_timeout`. The user can
still cancel a stuck task by quitting; the spawned blocking thread
will exit on its own when the syscall returns or times out.

## Cross-platform TTL constant

`SYSTEM_TTL` (`structs.rs:34-39`) is platform-conditional: 64 on Linux
and macOS, 128 on Windows. It's currently only used inside the
commented-out IPv4-wrap block. The hop-count *receiver* logic (step 8
above) doesn't reference `SYSTEM_TTL` — it infers the originator's
default by bucketing, not by knowing the local default. That's correct
because we're trying to estimate the remote sender's original TTL, not
our own.

## The standalone binary

`src/hopcount/main.rs` is a thin clap wrapper. Usage:

```sh
./target/release/hopcount 8.8.8.8                   # default 1s timeout
./target/release/hopcount --timeout 2 dns.google    # no, an IP — clap value_parser is IpAddr
./target/release/hopcount -q 1.1.1.1                # just the number
./target/release/hopcount --debug 1.1.1.1           # verbose internals
```

Note that the binary takes `IpAddr` (not a hostname) — it doesn't link
the resolver. If you need DNS, resolve outside.

It exits with code 1 and prints a `setcap` hint when raw sockets are
denied.

## File map

- `src/hopcount/mod.rs` — `determine_hops` and the AI-slop comment.
- `src/hopcount/main.rs` — the standalone binary.
- `src/pingdata.rs:275-288` — `PingTarget::determine_hops` /
  `PingTarget::hops`.
- `src/structs.rs:303-346` — `UpdateTgtInfo` handler that spawns it.
- `src/lib.rs:19` — `pub use hopcount::determine_hops` so the binary
  can import it.
