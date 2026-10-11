# Plan 001 — Real ASN datacenter lookup + "Shy Guys" heuristic

**Status:** not started

## Context

Two of the four bot-detection heuristics in `HeuristicRouter::evaluate_request`
(`src/heuristic_router/mod.rs`) do nothing:

- **ASN (Heuristic B)**: `AsnDatabase::is_datacenter` (`src/heuristic_router/asn_db.rs`) always returns `false`.
- **Shy Guys (Heuristic C)**: `IpState.missing_auxiliary_assets` (`space_saver.rs`) is read but never set.

The README lists both as known limitations, thesis-gap rows #5 and #6, and open checklist items. They are
also why research objective 1 is only "partially achieved". This plan makes both heuristics work. It keeps
the existing log-odds weights (`w_asn = 1.5`, `w_fav = 1.2`) and the two-phase
`get_or_add_and_evict` → `update` order that CLAUDE.md asks us to keep.

Decision (from user): ASN data comes from the public-domain **iptoasn.com `ip2asn-v4/v6.tsv`** files. There
are no new crate dependencies, and a small fixture file is committed so tests and benchmarks work offline.

---

## Part A — ASN datacenter lookup

### `src/heuristic_router/asn_db.rs` (rewrite)

- `AsnDatabase` becomes a real data structure instead of a unit struct:
  - `v4: Vec<(u32, u32, u32)>`: (range_start, range_end, asn), sorted by start.
  - `v6: Vec<(u128, u128, u32)>`: the same for IPv6.
  - `datacenter_asns: HashSet<u32>`.
  - Derive `Debug, Clone, Default`. Default is empty, which keeps today's "always false" behaviour.
- Constructors:
  - `AsnDatabase::empty()`.
  - `AsnDatabase::from_ip2asn_reader(impl BufRead) -> Result<Self>`, plus `from_ip2asn_file(&Path)`. These parse
    the tab-separated `range_start  range_end  AS_number  country_code  AS_description` format.
    - Skip blank or `#` lines, and skip `AS_number == 0` ("Not routed").
    - Skip malformed lines with a `tracing::warn!` instead of failing.
    - Accept v4 and v6 rows in one stream, sorted by start after loading.
  - `with_datacenter_asns(impl IntoIterator<Item = u32>)` overrides the default list.
- `DEFAULT_DATACENTER_ASNS: &[u32]` is a curated list of hosting/cloud ASNs with comments: AWS 16509/14618,
  Google Cloud 396982, Microsoft 8075, DigitalOcean 14061, Linode/Akamai 63949, Hetzner 24940, OVH 16276,
  Vultr 20473, Oracle 31898, Alibaba 45102, Scaleway 12876, Contabo 51167, Leaseweb 60781.
- Lookup:
  - `pub fn lookup_asn(&self, ip: &IpAddr) -> Option<u32>` uses `partition_point` binary search: last range
    with start ≤ ip, then check ip ≤ end. Cost is O(log n).
  - `pub fn is_datacenter(&self, ip: &IpAddr) -> bool` changes from an associated fn to `&self`. Its only
    caller is `mod.rs`.
- Add a new `DTACError::AsnDbError(String)` variant in `src/error.rs` for I/O errors.

### `src/heuristic_router/mod.rs`

- Add an `asn_db: AsnDatabase` field to `HeuristicRouter`.
- `new()` keeps its current behaviour by using `AsnDatabase::empty()`.
- Add `pub fn with_asn_db(asn_db: AsnDatabase) -> Self`. Factor the shared setup into a private
  `fn build(asn_db)` so the telemetry setup isn't duplicated.
- Heuristic B becomes `self.asn_db.is_datacenter(&client_ip)`.

### Wiring

- `src/dtac_cache.rs`: add `DtacCache::with_router(capacity, router: HeuristicRouter)`. `new()` delegates to
  it with `HeuristicRouter::new()`.
- `src/bin/replay_bench.rs` and `src/bin/dtac_standalone.rs`:
  - Add an optional `--asn-db <PATH>` (ip2asn TSV) flag.
  - Add an optional `--datacenter-asns <PATH>` flag (one ASN per line, overrides the default list).
  - When a path is given, load it at startup and build the cache with `DtacCache::with_router`. Log the
    number of ranges loaded.
  - Without the flags, behaviour is unchanged.

### Data files

- Add `data/asn/ip2asn-fixture.tsv`, a small committed fixture in real ip2asn format:
  - Datacenter rows covering the generator's existing `BOT_IP_POOL` datacenter IPs:
    - `45.33.0.0–45.33.127.255` (AS63949, Linode)
    - `104.248.0.0–104.248.255.255` and `167.172.0.0–167.172.255.255` (AS14061, DigitalOcean)
  - A couple of non-datacenter ISP rows and an IPv6 row for negative and v6 tests.
  - The TEST-NET bot IPs (`198.51.100.x`, `203.0.113.x`) are intentionally left unmatched. This gives the
    benchmark a mix of ASN-detectable and undetectable bots, so no generator change is needed.
- `.gitignore`: add `data/asn/*` with `!data/asn/ip2asn-fixture.tsv`, so a full iptoasn download isn't
  committed by accident.
- Add a README note on downloading the real files:
  `curl -O https://iptoasn.com/data/ip2asn-v4.tsv.gz && gunzip` into `data/asn/`.

---

## Part B — "Shy Guys" missing-auxiliary-asset heuristic

Real browsers fetch the favicon and static assets **once** and then cache them. So the right signal is
"this IP has made N page requests and has *never* fetched an auxiliary asset". "No asset since the last
page" would be wrong, because it flags humans who browse several pages with a cached favicon.

### `src/heuristic_router/space_saver.rs` — `IpState`

- Add `pub page_requests: u16` (saturating) and `pub has_fetched_asset: bool`.
- Keep `missing_auxiliary_assets: bool` as the derived flag the router reads.
- All three default to zero/false, so the `IpState { .., ..Default::default() }` call sites and the SpaceSaver
  eviction/inheritance paths need no changes. An evicted or pruned IP starts fresh.

### `src/heuristic_router/mod.rs`

- Add `const SHY_PAGE_THRESHOLD: u16 = 5;` with a doc comment explaining the grace period. A one-page human
  visit that never pulls the favicon must not be penalized. In the generator, 60% of human sessions have no
  favicon and use one IP per session.
- Add `fn is_auxiliary_asset(path: &str) -> bool`:
  - Strip `?query`/`#fragment` and lowercase.
  - Match `/favicon.ico`, `/apple-touch-icon*`, or an extension in
    `ico, css, js, mjs, png, jpg, jpeg, gif, svg, webp, avif, woff, woff2, ttf`.
- In Phase 2, inside the existing read → mutate → `update` block (before `space_saver.update`):
  - If the path is an asset, set `has_fetched_asset = true`. Otherwise do `page_requests = page_requests.saturating_add(1)`.
  - Set `missing_auxiliary_assets = !has_fetched_asset && page_requests >= SHY_PAGE_THRESHOLD`.
- Heuristic C stays as is (`if ip_state.missing_auxiliary_assets { score += self.w_fav }`). It now reads the
  freshly updated state. Add `page_requests` to the existing `info!` event.

Expected benchmark effect: the 14 bot IPs never fetch `/favicon.ico`, so each is flagged from its 5th page
onward. Human IPs make one page request each and are never flagged.

---

## Tests

- **Unit tests in `asn_db.rs`**:
  - Parse fixture-style text from `&[u8]`.
  - Lookups at range start, end and end+1.
  - Gap between ranges returns `None`.
  - IPv6 hit.
  - ASN-0 and malformed lines are skipped.
  - Unsorted input is still found.
  - Empty DB always returns false.
  - A custom datacenter list overrides the default.
- **Unit tests in `mod.rs`**: `is_auxiliary_asset` cases (query strings, case, `/item/42` = false, `.css` = true).
- **`tests/heuristic_router_tests.rs`** (integration):
  - *ASN*: a router built with `with_asn_db(fixture)` gives a higher score to `104.248.33.7` than to an ISP
    IP for an otherwise identical request. `HeuristicRouter::new()` gives both the same score.
  - *Shy guys*:
    - IP A sends `/favicon.ico` then 5 pages.
    - IP B sends 6 pages.
    - Both have the same request count, so velocity is equal. B's last score must be greater than A's.
    - An IP with only 4 pages must not be flagged (equal to A).
  - Load `data/asn/ip2asn-fixture.tsv` via `env!("CARGO_MANIFEST_DIR")` to check the committed fixture parses.

## Docs

- **README**:
  - Update the Architecture bullets for ASN and Shy Guys, and the Project layout (add `data/asn/`).
  - Document the `--asn-db` / `--datacenter-asns` flags in both binaries' sections and the CLI table.
  - Remove the two Known-limitations bullets. Replace them with honest caveats: an IP-level heuristic is
    evaded by fetching one favicon; the ASN list is curated, not complete; NAT/shared IPs.
  - Thesis table rows #5 and #6 become "Done". Tick the checklist items. Objective 1 becomes "Achieved".
  - Re-run the benchmark and refresh the results tables, with and without `--asn-db`, at all 4 ratios.
- **CLAUDE.md**: update the ASN and "Shy guys" paragraphs in the Heuristic router section, which currently
  call them a stub and unset.

## Verification

1. `cargo build && cargo test`: all existing and new tests pass.
2. `cargo clippy --all-targets`: no new warnings in touched code (`missing_docs` on the new public items,
   no new `unwrap` outside tests).
3. `.venv/bin/python scripts/generate_logs.py` (if logs are missing).
4. For each ratio, run:
   `cargo run --release --bin replay_bench -- --file benchmark_logs/traffic_mixed_75pct_bot.ndjson --capacity 10000 --algorithm dtac [--asn-db data/asn/ip2asn-fixture.tsv]`.
   Confirm the human hit rate is not worse than the current README numbers and the bot hit rate does not
   increase.
5. Spot-check with `RUST_LOG=dtac=info`: `missing_assets=true` appears for bot IPs after their 5th request,
   and `is_datacenter=true` appears only for `45.33.*`, `104.248.*` and `167.172.*`.
6. Smoke-test `dtac_standalone --asn-db data/asn/ip2asn-fixture.tsv`: it starts and logs the number of
   ranges loaded.
