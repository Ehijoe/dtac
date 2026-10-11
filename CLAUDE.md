# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

DTAC (Dynamic Traffic-Aware Cache) is a Rust library + standalone proxy binary that caches HTTP
responses while weighting eviction/promotion decisions by an estimated bot probability ("AI-Score")
for each request, so that human traffic is favored over scraper/bot traffic for cache residency.

## Plans

Plan-mode plans are stored in `plans/` (set via `plansDirectory` in `.claude/settings.json`). Plan
files must be numbered so the order to apply them is clear:

- Name each plan `NNN-short-descriptive-slug.md`, where `NNN` is a zero-padded, 3-digit number one
  greater than the highest existing number in `plans/` (start at `001`).
- Plan mode assigns a random filename while drafting. Once a plan is approved, rename it
  (`mv plans/<random-name>.md plans/NNN-<slug>.md`) before starting implementation.
- Apply plans in ascending numeric order. Never renumber an existing plan; if a plan is superseded,
  note that at the top of the old file instead of deleting it.

## Commands

Build/test commands assume a working Rust toolchain (`rustup default stable` if `cargo` reports no
default toolchain — this was observed to be unconfigured in at least one sandboxed environment).

```bash
cargo build                 # build the library + binaries
cargo build --release       # release build (used for benchmarking)
cargo test                  # run all tests (unit tests in src/, integration tests in tests/)
cargo test <test_name>      # run a single test by name (substring match)
cargo clippy                # lint — this repo enables pedantic+nursery+cargo clippy lint groups
```

Integration tests live in `tests/cache_engine_tests.rs` and `tests/heuristic_router_tests.rs`; unit
tests live inline in `#[cfg(test)]` modules within the source files they test (e.g.
`src/heuristic_router/score.rs`, `src/heuristic_router/space_saver.rs`).

### Lints

`Cargo.toml` turns on `clippy::pedantic`, `clippy::nursery`, and `clippy::cargo` as warnings, plus
`unsafe_code = "forbid"` and `missing_docs = "warn"`. New public items need doc comments;
`unwrap()`/`expect()` are warned on (`clippy::unwrap_used`, `clippy::expect_used`) — existing code
has some pre-existing violations (e.g. mutex locks, `tokio::net::TcpListener::bind(...).await.unwrap()`
in the standalone binary) that are not yet cleaned up.

### Running the standalone proxy

```bash
cargo run --bin dtac_standalone -- --capacity 1024 --target http://127.0.0.1:8081 --port 8080
```

Starts an Axum HTTP server that proxies all requests to `--target`, caching responses through
`DtacCache`, and exposes Prometheus metrics at `/metrics`.

### Benchmarking pipeline

Benchmarking is two separate steps: generate synthetic traffic logs with Python, then replay them
through the cache engine in-process with Rust (no real network calls).

```bash
# 1. Generate NDJSON traffic logs into benchmark_logs/ (requires numpy; see scripts/requirements.txt)
python scripts/generate_logs.py

# 2. Replay a log directly against the cache engine and print a benchmark report
cargo run --release --bin replay_bench -- --file benchmark_logs/traffic_mixed_30pct_bot.ndjson \
    --capacity 10000 --concurrency 128
```

`generate_logs.py` produces a Zipfian-distributed "catalog" of items requested by synthetic human
traffic (bursty, realistic headers, occasional secondary asset fetch) interleaved with bot traffic
(mechanical timing, stripped headers) at configurable bot ratios (10/30/50/75%) into
`benchmark_logs/*.ndjson`. `replay_bench` streams those records concurrently through a real
`DtacCache` instance and reports overall/human/bot hit rates and throughput — this is the main tool
for evaluating whether router+cache tuning actually improves bot resistance without hurting human
hit rate.

## Architecture

The system has two independently-testable halves that `DtacCache` (`src/dtac_cache.rs`) composes:

1. **`heuristic_router`** — scores *requests* (not resources) with a bot probability, used as an
   input signal to the cache engine. It does not know about cache state.
2. **`cache_engine`** — a sharded, randomized-eviction cache keyed by *resource*, which uses the
   router's score to bias which resources get to stay cached.

These are deliberately decoupled: the router answers "how bot-like is this request?" and the engine
answers "given that signal, should this resource be promoted/evicted?". `DtacCache::access` is the
only place that wires them together per-request, and it's also where hit/miss/duration telemetry is
recorded.

### Heuristic router (`src/heuristic_router/`)

`HeuristicRouter::evaluate_request` implements a **log-odds (logistic regression-style) scoring
pipeline**: start from a baseline prior `l_0`, add weighted contributions from independent heuristics,
then squash through a sigmoid to get a `[0,1]` bot probability (`AiScore`, in `score.rs`):

- **Velocity heuristic**: per-IP request rate tracked as an EWMA using *bitwise right-shift* for
  exponential decay (not floating-point multiply) — `velocity >> delta_t`, fixed-point scaled by
  1024. A graded penalty (`ALPHA * ln(velocity/BURST_THRESHOLD)`) applies once velocity exceeds
  `BURST_THRESHOLD`.
- **ASN heuristic** (`asn_db.rs`): flags datacenter/hosting-provider IPs. Currently a stub
  (`AsnDatabase::is_datacenter` always returns `false`) — real ASN lookup is not implemented.
- **"Shy guys" heuristic**: penalizes IPs that never fetch auxiliary assets (favicon, etc.) the way
  real browsers do — tracked via `IpState.missing_auxiliary_assets`, but nothing in this router
  currently *sets* that flag to `true`; it's plumbed through from `SpaceSaver` state for a caller to
  populate.
- **Header heuristic**: penalizes requests missing an `Accept-Language` header.

Per-IP state (`IpState`: last-seen time, EWMA velocity, asset-miss flag, request count) is tracked in
**`SpaceSaver`** (`space_saver.rs`), a bounded-memory structure implementing the SpaceSaver/Stream-Summary
algorithm: it holds at most `capacity` IPs, indexed by both a count-ordered `BTreeSet` (to find the
minimum for eviction) and a time-ordered `BTreeSet` (to find stale entries to decay/prune). When a new
IP arrives and memory is full, it evicts the lowest-count IP and the new IP *inherits* that evicted
count + 1 — this is the standard SpaceSaver technique for bounding the undercount error on frequency
estimates. Counts decay via a half-life exponential model (`2^(-elapsed/half_life)`), evaluated lazily
on read rather than on a timer.

`evaluate_request` is a two-phase read/mutate/write against `SpaceSaver`
(`get_or_add_and_evict` then `update`) — not a single atomic operation. Callers modifying this logic
should preserve that order.

### Cache engine (`src/cache_engine/mod.rs`)

`DtacCacheEngine<K, P>` shards state across `NUM_SHARDS` (256) independent `Mutex<CacheShard<K, P>>`
instances, keyed by hashing the resource key, to reduce lock contention under concurrent access. Each
shard independently runs the same algorithm:

- **Ghost cache**: metadata-only (no payload) tracking for resources not currently in the main cache.
  Every miss first lands here rather than immediately entering the main cache — this prevents one-off/
  scan traffic from evicting genuinely popular resources ("cold-start mitigation").
- **Promotion**: a ghost-cache entry is promoted into the main cache (fetching the real payload) once
  its score crosses `PROMOTION_THRESHOLD`.
- **Scoring**: each hit increases a resource's EWMA score by `(1.0 - request_bot_prob)` — i.e. human
  requests (`bot_prob` near 0) contribute close to a full point, bot-like requests (`bot_prob` near 1)
  contribute close to nothing. This is the core mechanism connecting router output to cache behavior.
  Scores decay the same bitwise-shift-style way as router velocity, keyed off wall-clock `Instant`.
- **Eviction**: `execute_eviction_tournament` samples `N_SAMPLES` (5) random entries from the shard's
  dense `entries` vec and evicts whichever has the lowest *currently decayed* score (not the lowest
  stored score) — a randomized approximation of LFU/LRU that avoids maintaining a full priority queue.
  Removal uses `Vec::swap_remove` for O(1) removal, which requires fixing up the moved entry's index in
  `main_cache_map` afterward.

`DtacCacheEngine::access` takes a `fetch_from_origin` closure and only calls it on an actual miss or
promotion — hits and ghost-hits return without touching the closure. The lock is held only for the
cache-state transition decision; the async `fetch_from_origin` call happens *outside* the lock
(`Action` enum marks what to do after releasing it), so origin fetches never block other shard access.

### Binaries (`src/bin/`)

- **`dtac_standalone.rs`**: Axum-based reverse proxy. Wraps a single shared `DtacCache<String, Bytes>`
  in `Arc`, forwards every path to `--target` via `reqwest` on miss, and exports OpenTelemetry metrics
  through a Prometheus registry at `/metrics`. Note the client IP is currently hardcoded to
  `127.0.0.1` rather than extracted from the connection/`X-Forwarded-For` — this means the ASN and
  per-IP velocity heuristics are not meaningfully exercised when actually running this binary.
- **`replay_bench.rs`**: in-process benchmark harness (see Benchmarking pipeline above). Deserializes
  NDJSON into memory up front, then drives concurrent `DtacCache::access` calls via
  `futures::stream::iter(...).for_each_concurrent`, tracking separate hit/miss counters for
  `human`/`bot`-labeled records so cache effectiveness can be compared across bot-traffic ratios.

### Observability

Both the router and cache engine take OpenTelemetry `Counter`/`Histogram` instruments from the global
meter (`opentelemetry::global::meter("dtac")`) at construction time, and emit `tracing` spans/events
(`#[instrument]`, `info!`/`debug!`) per request. The library itself (`tracing`, `opentelemetry`) has no
opinion on where telemetry goes; only the binaries wire up exporters (`tracing-subscriber`,
`opentelemetry-prometheus`, `opentelemetry_sdk`) — keep that split when adding new instrumentation
inside `src/` modules used by the library.
