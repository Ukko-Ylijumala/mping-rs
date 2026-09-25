# Origin AS lookup

mping shows the origin AS (autonomous system) of each target in the
selected-target info pane, next to the PTR / hop count / distance lines.
The data comes from the Team Cymru IP-to-ASN DNS service, so it costs two
plain DNS TXT queries and no extra dependencies or HTTP client.

## Why Cymru DNS

- `hickory_resolver` is already linked for target resolution; the same
  `TokioResolver` instance answers these queries, so `--dns-servers` and
  `--dns-timeout` apply unchanged.
- RDAP / WHOIS would need an HTTP+TLS stack or a port-43 client plus a
  referral chase (IANA → RIR), and return freeform text. The Cymru answer
  is one line with fixed fields.
- One TXT answer already covers most of the "who owns this / where is it
  registered" question: ASN, announced prefix, country code, RIR and
  allocation date. The country code is also a useful sanity check on the
  [distance estimate](distance-estimation.md) — a US-registered prefix
  that looks 200 km away is anycast.

## Query shape

`lookup_as` (`src/asinfo.rs`) issues:

1. **Origin query** — the address reversed the same way as for PTR
   (`utils::reversed_addr`, shared with `reverse_name`), under
   `origin.asn.cymru.com.` for IPv4 or `origin6.asn.cymru.com.` for IPv6
   (nibble-reversed, all 32 nibbles). The trailing dot marks the name as
   fully qualified so the resolver skips search-list expansion.

   ```text
   8.8.8.8.origin.asn.cymru.com.  TXT  "15169 | 8.8.8.0/24 | US | arin | 2023-12-28"
   ```

2. **AS name query** — one per ASN found, `AS<n>.asn.cymru.com.`:

   ```text
   AS15169.asn.cymru.com.  TXT  "15169 | US | arin | 2000-03-30 | GOOGLE - Google LLC, US"
   ```

Answer fields are `|`-separated and whitespace padded; `parse_origin` /
`parse_asname` split on `|` and trim. Two irregular shapes are handled:

- **Multi-origin prefix**: the first field carries several ASNs separated
  by spaces (`"15169 3356 | ..."`). All are kept, in order.
- **Several TXT records**: an address covered by more than one announced
  prefix yields one record per announcement. They are folded with
  `AsInfo::merge` — the union of ASNs, with the first record's prefix /
  country / RIR / date winning.

Unrouted space (RFC 1918, documentation ranges, unallocated blocks)
answers NXDOMAIN, which hickory surfaces as an error. `no_records_or_err`
maps `is_nx_domain() || is_no_records_found()` to the plain
`WARN_AS_NONE` text ("Not announced") rather than an error; anything else
(timeout, SERVFAIL) is shown as `E: <resolver error>`. An AS name lookup
failing is not fatal — the origin data is kept and the name left empty.

## Data model and display

`AsInfo` (`src/asinfo.rs`) holds `asns: Vec<u32>`, `prefix`, `country`,
`rir`, `allocated` and `names: Vec<String>` (parallel to `asns`). It is
carried as a new `QueryResponse::As(AsInfo)` variant (`structs.rs`) so it
sits in the same `RwLock<QueryResponse>` slot pattern as `hops`, `ptr` and
`rev_ptr` on `PingTarget` (`asinfo` field, `resolve_as` / `asinfo()`
accessors in `pingdata.rs`).

`Display` renders as:

```text
AS13335 CLOUDFLARENET, US [1.1.1.0/24 AU/apnic]
AS15169, AS3356 GOOGLE - Google LLC, US / LEVEL3, US [8.8.8.0/24 US/arin]
```

The AS name from Cymru already ends in the AS's own country; the bracketed
country is the *prefix* registration country, which can legitimately
differ (Cloudflare's 1.1.1.0/24 is APNIC/AU space).

## When it runs

Also per hop of a [traceroute](traceroute.md): every answering hop gets
the same two queries, displayed in the `AsInfo::short()` form.

### On Enter

Only on Enter (`Command::UpdateTgtInfo`), alongside the hop-count and PTR
tasks — see [keyboard-and-commands](keyboard-and-commands.md). The handler
in `AppState::update_target_info` (`structs.rs`) spawns `resolve_as` as a
third task via `AppState::spawn` (async DNS, no blocking I/O, so no
`spawn_blocking`). Like the PTR task it must go through the stored runtime
handle because the caller is the keyboard thread. Nothing is cached across
targets; a CIDR sweep pressing Enter on every host repeats the AS name
query, which is cheap and DNS-cached upstream anyway.

The info pane grew by one row for the `AS` line (`CON_NFO_T` in
`ui/tui.rs`, 10 → 11).

## Testing

`cargo test asinfo` covers the parsers, merge and display. A live test
against Cymru is `#[ignore]`d because it needs network:

```sh
cargo test asinfo::tests::live_lookup -- --ignored --nocapture
```

It checks 8.8.8.8 and 2001:4860:4860::8888 resolve to AS15169 and that
10.0.0.1 yields the "not announced" text.

## File map

- `src/asinfo.rs` — `AsInfo`, `lookup_as`, parsers, tests.
- `src/utils.rs` — `reversed_addr` (shared with `reverse_name`).
- `src/pingdata.rs` — `PingTarget::asinfo` field, `resolve_as`, `asinfo()`.
- `src/structs.rs` — `QueryResponse::As`, the third spawn in
  `update_target_info`.
- `src/strings.rs` — `CYMRU_*` zone suffixes, `WARN_AS_NONE`,
  `ERR_AS_PARSE`, `INFO_AS`, the extra `INFO_TARGET` line.
- `src/main.rs` — passes `t.asinfo()` into the info pane template.
