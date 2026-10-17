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

## Commits

Keep commits small and focused so a reviewer can dig into one part of a change on its own:

- Split work into one commit per logical part (e.g. per plan section: a data structure, a
  heuristic, a config surface, binary wiring, docs) instead of one large commit.
- Each commit should build and pass `cargo test` on its own.
- Write a subject line that says what the commit does, plus a short body explaining why when it
  isn't obvious.

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

Integration tests live in `tests/cache_engine_tests.rs`, `tests/heuristic_router_tests.rs`,
`tests/dtac_cache_tests.rs` and `tests/networked_tests.rs` (spawns `origin_stub` and
`dtac_standalone` on ephemeral ports); unit
tests live inline in `#[cfg(test)]` modules within the source files they test (e.g.
`src/heuristic_router/score.rs`, `src/heuristic_router/space_saver.rs`).

### Lints

`Cargo.toml` turns on `clippy::pedantic`, `clippy::nursery`, and `clippy::cargo` as warnings, plus
`unsafe_code = "forbid"` and `missing_docs = "warn"`. New public items need doc comments;
`unwrap()`/`expect()` are warned on (`clippy::unwrap_used`, `clippy::expect_used`) outside tests
(`clippy.toml` allows them in test code). The tree is warning-free and CI runs clippy with
`-D warnings`, so any new warning fails the build: lock mutexes with `crate::sync::lock`, propagate
errors with `?` in the binaries, and put a justifying comment on any `#[allow(...)]`.

### Running the standalone proxy

```bash
cargo run --bin dtac_standalone -- --capacity 1024 --target http://127.0.0.1:8081 --port 8080
```

Starts an Axum HTTP server that proxies all requests to `--target`, caching responses through
`DtacCache`, and exposes Prometheus metrics at `/metrics`. Responses carry `X-Cache: HIT|MISS`.
`--trusted-proxy <CIDR>` (repeatable) is the only way `X-Forwarded-For` is believed.
`--clock replay-header` and `--debug-headers` exist only for the networked benchmark.

### Benchmarking pipeline

Benchmarking is two separate steps: generate synthetic traffic logs with Python, then replay them
through the cache engine in-process with Rust (no real network calls). `scripts/run_experiments.py`
drives both over the whole experiment grid (see below).

```bash
# 1. Generate NDJSON traffic logs into benchmark_logs/ (requires numpy; see scripts/requirements.txt)
python scripts/generate_logs.py --seed 1 --workload both     # --legacy: the pre-plan-003 model
(cd scripts && python -m unittest test_generate_logs)        # generator tests (~20 s)

# 2. Replay a log directly against the cache engine and print a benchmark report
cargo run --release --bin replay_bench -- --file benchmark_logs/traffic_mixed_30pct_bot.ndjson \
    --capacity 10000 --concurrency 128 --asn-db data/asn/ip2asn-fixture.tsv --json-out /tmp/r.json
```

`generate_logs.py` (argparse; `--seed`, `--out-dir`, `--ratios`, `--workload {naive,stealthy,both}`,
session/bot/size parameters) builds one shared human stream per seed: Zipf-catalog multi-page
sessions, one IP each, exponential think time, a favicon for new visitors only
(`--returning-frac`), optional NAT. It reuses that stream in every ratio file and both workloads.
For each ratio it adds `B = H·r/(1−r)` bots spread evenly over the measurement window, so the bot
share is stationary:
- naive bots (`traffic_mixed_*`): a pool that grows with the ratio, each IP sending at
  `--bot-ip-rate`, IPs in the fixture's datacenter ranges or TEST-NET;
- stealthy bots (`traffic_stealthy_*`): browser headers, rotating residential IPs, one favicon per
  IP.

Every record has a deterministic per-item `size_bytes`; humans carry `visitor`, bots `bot_kind`.
Each `.ndjson` gets a `.meta.json` sidecar (seed, args, git commit). `--legacy` reproduces the old
generator byte for byte (its code is kept verbatim in the same file); don't change it.

`replay_bench` streams records concurrently through a real `DtacCache` (or `--algorithm lru`)
and reports:
- human/overall/bot hit rate and Byte Hit Rate;
- `T_proxy`/`T_cache`/total latency percentiles (hdrhistogram);
- memory (peak RSS, RSS delta, structure sizes, logical resident bytes);
- eviction-by-admitter counts and the Cache Pollution Index;
- the router as a classifier (ROC-AUC, precision/recall/FPR, new vs. returning visitor FPR).

`--json-out` writes all of it plus the run config (including the resolved shard count) and the
log's `.meta.json`. `--quiet` hides the progress bar and the Prometheus dump. `--algorithm` is one
of `dtac`, `dtac-no-score` (the ablation: DTAC's engine with a constant `sigmoid(l_0)` score),
`lru`, `sieve` or `s3fifo`; every engine uses the same `--shards` (default `auto`) and
capacity-per-shard split. `--target-url` replays over HTTP against a running `dtac_standalone`
instead (with `origin_stub` as its origin).

```bash
# 3. The experiment grid: calibration on seed 0 first, then seeds 1-5 (resumable)
python scripts/run_experiments.py tune        # writes results/tuning.md + tuning.json
python scripts/run_experiments.py --quick     # 20-run smoke test
python scripts/run_experiments.py             # 350 runs (thesis profile); --profile full: 800
(cd scripts && python -m unittest test_run_experiments)
```

`run_experiments.py` writes one `replay_bench --json-out` per cell to `results/raw/` (gitignored)
and aggregates them into `results/summary.csv` and `results/summary.md`: mean ± 95% t-interval
over seeds, the paired per-seed differences `dtac − X`, router quality and capacity sweeps.
Evaluation seeds are refused until `results/tuning.md` exists, seed 0 is never aggregated, and
results from different commits or configs are refused unless `--allow-mixed`. A run records the
commit with `-dirty` if tracked files changed, so **don't edit tracked files while a sweep runs**.
`tune` keeps the hand-written text around its generated block in `tuning.md`. The library
defaults are its calibrated values, so changing a `RouterConfig`/`EngineConfig` default
invalidates `results/` until the sweep is rerun.

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
then squash through a sigmoid to get a `[0,1]` bot probability (`AiScore`, in `score.rs`). All
weights/thresholds live in `RouterConfig` (`config.rs`, serde-deserializable, validated by
`HeuristicRouter::with_config`; defaults calibrated in `results/tuning.md`); `l_0` is derived from
`prior_bot_rate` (default 0.02, so `l_0 = ln(0.02/0.98) ≈ −3.89`, and a clean human request scores
≈ 0.02, `RouterConfig::clean_request_bot_prob`). `evaluate_request_detailed` also returns which
heuristics fired (`Evidence`), which `AccessStats` carries and `replay_bench` turns into per-heuristic
firing rates and empirical LLRs:

- **Velocity heuristic**: per-IP request rate tracked as an EWMA using *bitwise right-shift* for
  exponential decay (not floating-point multiply) — `velocity >> delta_t`, fixed-point scaled by
  1024. `delta_t` counts `velocity_tick_ms` tick boundaries crossed since `IpState.last_seen_ms`
  (epoch-aligned, so sub-tick request streams still decay); beyond `velocity_reset_ticks` it resets.
  A graded penalty (`alpha * ln(velocity/burst_threshold)`) applies once velocity exceeds
  `burst_threshold`.
- **ASN heuristic** (`asn_db.rs`): `AsnDatabase` holds sorted v4 (`u32`) and v6 (`u128`) ranges
  parsed from iptoasn.com `ip2asn` TSV files and finds an IP's ASN via `partition_point` binary
  search, then checks it against a datacenter-ASN set (`DEFAULT_DATACENTER_ASNS`, overridable with
  `with_datacenter_asns`). `HeuristicRouter::new()` uses an empty DB (heuristic never fires);
  `with_asn_db`/`with_config` take a loaded one. Both binaries accept repeatable `--asn-db` and
  `--datacenter-asns` via the shared `src/bin/common/mod.rs`. `data/asn/ip2asn-fixture.tsv` is a
  committed fixture matching the generator's datacenter bot IPs (by construction — not evidence
  of real-world accuracy); full downloads in `data/asn/` are gitignored.
- **"Shy guys" heuristic**: penalizes IPs that have made `shy_page_threshold` (default 12) page
  requests and *never* fetched an auxiliary asset (`is_auxiliary_asset` in `mod.rs`: favicon,
  apple-touch-icon, or a static-asset extension). `IpState::record_request` updates
  `page_requests`/`has_fetched_asset` and derives `missing_auxiliary_assets` inside the router's
  read → mutate → `update` block.
- **Header heuristic**: penalizes requests missing an `Accept-Language` header.

Per-IP state (`IpState`: last-seen time, EWMA velocity, asset-miss flag, request count) is tracked in
**`SpaceSaver`** (`space_saver.rs`), a bounded-memory structure implementing the SpaceSaver/Stream-Summary
algorithm: it holds at most `capacity` IPs, indexed by both a count-ordered `BTreeSet` (to find the
minimum for eviction) and a time-ordered `BTreeSet` (to find stale entries to decay/prune). When a new
IP arrives and memory is full, it evicts the lowest-count IP and the new IP *inherits* that evicted
count + 1 — this is the standard SpaceSaver technique for bounding the undercount error on frequency
estimates. Counts decay via a half-life exponential model (`2^(-elapsed/half_life)`), evaluated lazily
on read rather than on a timer (half-life `space_saver_half_life_s`).

All router time comes from an injected `Clock` (`src/clock.rs`, `Arc<dyn Clock>`) shared with its
`SpaceSaver`: `SystemClock` by default, `ManualClock` via `HeuristicRouter::with_clock` for log
replays and tests. Never read `SystemTime`/`Instant` directly for heuristic logic.

`evaluate_request` is a two-phase read/mutate/write against `SpaceSaver`
(`get_or_add_and_evict` then `update`) — not a single atomic operation. Callers modifying this logic
should preserve that order.

### Cache engine (`src/cache_engine/mod.rs`)

`DtacCacheEngine<K, P>` shards state across independent `Mutex<CacheShard<K, P>>` instances, keyed
by `shard_of` (hash of the resource key), to reduce lock contention under concurrent access.
`EngineConfig.shards` is `"auto"` (the largest power of two ≤ `NUM_SHARDS` = 256 that leaves at
least `MIN_ENTRIES_PER_SHARD` = 64 entries per shard, so 256 from capacity 16 384) or a fixed count.
Each shard independently runs the same algorithm:

- **Ghost cache**: metadata-only (no payload) tracking for resources not currently in the main cache.
  Every miss first lands here rather than immediately entering the main cache — this prevents one-off/
  scan traffic from evicting genuinely popular resources ("cold-start mitigation"). Stored densely
  like the main cache (`ghost_map: HashMap<K, usize>` + `ghost_entries: Vec<(K, Metadata)>`) and
  bounded at `capacity_per_shard * ghost_multiplier`; a miss into a full ghost first runs the same
  tournament as the main cache (bot-inserted entries score ≈ 0 and lose first).
- **Promotion**: a ghost-cache entry is promoted into the main cache (fetching the real payload) once
  its score crosses `promotion_threshold` (default `PROMOTION_THRESHOLD`). The ghost entry is removed
  only when the promotion commits; it may already have been evicted by then, which is fine.
- **Scoring**: each hit increases a resource's EWMA score by `(1.0 - request_bot_prob)` — i.e. human
  requests (`bot_prob` near 0) contribute close to a full point, bot-like requests (`bot_prob` near 1)
  contribute close to nothing. This is the core mechanism connecting router output to cache behavior.
  `Metadata.last_accessed` is `u64` milliseconds on the engine's injected `Clock`;
  `Metadata::decayed_score` halves the score once per whole `decay_half_life_ms` and zeroes it after
  `decay_cutoff_half_lives`.
- **Eviction**: the free function `tournament_victim` samples `n_samples` (default 8) random entries
  from a dense `(key, meta)` slice and picks the lowest *currently decayed* score (not the lowest
  stored score), breaking ties on the oldest `last_accessed` — a randomized approximation of LFU/LRU
  that avoids maintaining a full priority queue. Main and ghost caches share it, plus
  `swap_remove_indexed`, which does the O(1) `Vec::swap_remove` and fixes up the moved entry's index
  in the key → index map.

All engine constants are in `EngineConfig` (`src/cache_engine/config.rs`, serde-deserializable,
validated by `DtacCacheEngine::with_config`). The defaults (4 h half-life, promotion threshold 1.5,
8 samples, ghost ×8) are plan 004's seed-0 calibration (`results/tuning.md`).

`DtacCacheEngine::with_eviction_listener` (or `set_eviction_listener`) takes an `Arc<dyn Fn(&K)>` called with every main-cache
victim *after* the shard lock is released (collect victims inside the lock, notify outside), and
`resident_keys()` lists main-cache keys (diagnostics only, like `debug_integrity_counts`).

`DtacCacheEngine::access` takes a `fetch_from_origin` closure and only calls it on an actual miss or
promotion — hits and ghost-hits return without touching the closure. The lock is held only for the
cache-state transition decision; the async `fetch_from_origin` call happens *outside* the lock
(`Action` enum marks what to do after releasing it), so origin fetches never block other shard access.

`DtacCache::builder(capacity).router(router).clock(clock).engine_config(cfg)?.eviction_listener(f).build()`
is how custom parts are injected; the builder passes one shared clock to both router and engine (the router's own
clock if `.clock` isn't called). `DtacCache::new(capacity)` is shorthand for the defaults.

### Binaries (`src/bin/`)

- **`dtac_standalone/`**: Axum-based reverse proxy. Wraps a single shared `DtacCache<String, Bytes>`
  in `Arc`, forwards the path and query to `--target` through one shared `reqwest::Client` on a miss
  (the cache key is the path), adds `X-Cache: HIT|MISS` (and `X-Bot-Probability` with
  `--debug-headers`), and exports OpenTelemetry metrics through a Prometheus registry at `/metrics`.
  `client_ip.rs` picks the client IP: the TCP peer, or, when the peer is a `--trusted-proxy`, the
  right-most untrusted `X-Forwarded-For` hop (malformed header → peer). `--clock replay-header`
  takes time from trusted `X-Replay-Time-Ms` headers so a networked replay runs on log time.
  `--port 0` prints the bound address as `Listening on <addr>`.
- **`origin_stub.rs`**: origin for the networked benchmark; returns `?size=N` zero bytes.
- **`replay_bench/`**: in-process benchmark harness (see Benchmarking pipeline above).
  - `main.rs`: CLI, `LogRecord`, the dispatch loop, `--json-out`. It keeps the NDJSON log in
    memory and dispatches `DtacCache::access_with_stats` calls (`AccessStats`: hit, `bot_prob`,
    router/engine time) as `JoinSet` tasks. Calls are paced by logged timestamps (scaled by
    `--speed`) and bounded by a semaphore (`--concurrency`). Each task gets a clone of its record,
    and finished tasks are reaped in the loop, so the log stays resident and the RSS delta
    measures the cache. `--clock log` (default) sets a shared `ManualClock` to each record's
    timestamp before its access, so `--speed` only affects run time; `--clock wall` reproduces
    the old wall-clock behaviour. `--engine-config`/`--router-config` (JSON),
    `--ghost-multiplier` and `--shards` set any constant. The pipeline (`run`, `dispatch`,
    `replay_record`) is generic over `BenchEngine`.
  - `metrics.rs`: `BenchMetrics` atomics, `Striped<T>` (per-record-sequence mutex stripes for
    the latency histograms and classifier samples), and the `Report` (Serialize) that is both
    printed and written to `--json-out`.
  - `engines.rs`: the `BenchEngine` trait (`access`, `resident_keys`, `set_eviction_listener`,
    `name`, `structure`, `configs`), `DtacEngine`, `NoScoreEngine` (the ablation), the sharded
    `BaselineLruEngine`, and `Sharded<S: ShardPolicy>`, which owns locking, shard choice and
    eviction notifications for single-threaded per-shard policies.
  - `sieve.rs` / `s3fifo.rs`: SIEVE (slab + index-linked FIFO, visited bit, moving hand) and
    S3-FIFO (small/main/ghost FIFOs, 2-bit frequency) as `ShardPolicy` implementations.
  - `http.rs`: `HttpEngine` for `--target-url`; reads `X-Cache`/`X-Bot-Probability`, no CPI.
  - `classifier.rs`: router quality (AUC by sorting, threshold metrics, histograms).
  - `pollution.rs`: the per-key `Ledger` behind eviction-by-admitter and the CPI. It records the
    label of each key's latest origin fetch and whether a human has requested the key since.
  - Payloads are slices of one preallocated buffer sized by each record's `size_bytes`.

### Observability

Both the router and cache engine take OpenTelemetry `Counter`/`Histogram` instruments from the global
meter (`opentelemetry::global::meter("dtac")`) at construction time, and emit `tracing` spans/events
(`#[instrument]`, `info!`/`debug!`) per request. The library itself (`tracing`, `opentelemetry`) has no
opinion on where telemetry goes; only the binaries wire up exporters (`tracing-subscriber`,
`opentelemetry-prometheus`, `opentelemetry_sdk`) — keep that split when adding new instrumentation
inside `src/` modules used by the library.
