# DTAC — Dynamic Traffic-Aware Cache

DTAC is a Rust cache that tries to resist bot/scraper cache pollution by scoring every request
with a lightweight "AI-Score" (a bot-probability estimate) and weighting cache admission/eviction
by that score, instead of treating every request as equally worth caching the way a textbook LRU
cache does.

This README covers the architecture, the benchmarking methodology built around it, the results of
comparing DTAC against a standard LRU baseline, and full instructions for building, testing, and
running the benchmark yourself.

## Contents

- [Architecture](#architecture)
- [Project layout](#project-layout)
- [Requirements](#requirements)
- [Building & testing](#building--testing)
- [Running the standalone proxy](#running-the-standalone-proxy)
- [Benchmarking](#benchmarking)
- [Benchmark results](#benchmark-results)
- [Known limitations](#known-limitations)

## Architecture

DTAC (`src/dtac_cache.rs`) composes two independently-testable halves:

### Heuristic router (`src/heuristic_router/`)

`HeuristicRouter::evaluate_request` scores each *request* with a bot probability using a log-odds
(logistic-regression-style) model: start from a baseline prior, add weighted contributions from a
few independent heuristics, then squash through a sigmoid into a `[0, 1]` bot score (`AiScore`):

- **Velocity**: per-IP request rate, tracked as a fixed-point EWMA with bitwise-shift decay. A
  graded penalty kicks in once an IP's rate crosses `BURST_THRESHOLD`.
- **ASN**: flags datacenter/hosting-provider IPs. Currently a stub (`AsnDatabase::is_datacenter`
  always returns `false`) — real ASN lookup isn't implemented.
- **"Shy guys"**: meant to penalize IPs that never fetch auxiliary assets like browsers do. The
  flag exists on `IpState` but nothing currently sets it to `true` — effectively dead for now.
- **Headers**: penalizes requests missing an `Accept-Language` header.

Per-IP state is tracked in `SpaceSaver` (`space_saver.rs`), a bounded-memory structure (the
SpaceSaver/Stream-Summary algorithm) so the router's memory use doesn't grow unbounded with
distinct IPs.

### Cache engine (`src/cache_engine/`)

`DtacCacheEngine<K, P>` shards state across 256 independent `Mutex`-protected shards (hashed by
resource key) to reduce lock contention. Each shard runs:

- **Ghost cache**: a metadata-only (no payload) entry for resources not yet in the main cache.
  Every miss lands here first, so a one-off scan doesn't evict genuinely popular content.
- **Promotion**: a ghost-cache entry is promoted into the real cache (fetching the payload) once
  its EWMA popularity score crosses `PROMOTION_THRESHOLD`.
- **Scoring**: each hit increases a resource's score by `1.0 - request_bot_prob` — human requests
  contribute close to a full point, bot-like requests contribute close to nothing. This is the
  mechanism that actually connects the router's output to caching behavior.
- **Eviction**: a randomized tournament — sample a few entries, evict whichever has the lowest
  *currently decayed* score. A cheap approximation of LFU/LRU that avoids a full priority queue.

### Binaries (`src/bin/`)

- **`dtac_standalone`** — an Axum reverse proxy wrapping a single shared `DtacCache`, forwarding
  misses upstream via `reqwest`, exporting Prometheus metrics at `/metrics`.
- **`replay_bench`** — an in-process benchmark harness (no real network calls) used for everything
  described below.

## Project layout

```
src/
  heuristic_router/   bot-score computation (velocity, ASN stub, header heuristics)
  cache_engine/        sharded randomized-eviction cache with ghost-cache admission
  dtac_cache.rs        combines the two + telemetry
  bin/
    dtac_standalone.rs reverse-proxy binary
    replay_bench.rs    benchmark replay tool (DTAC vs. a standard-LRU baseline)
scripts/
  generate_logs.py     synthetic traffic generator
  test_generate_logs.py
tests/                 integration tests (cache engine, heuristic router)
benchmark_logs/        generated NDJSON traffic logs (gitignored)
```

## Requirements

- **Rust** (stable toolchain; developed against 1.97). `rustup default stable` if you don't have
  one configured.
- **Python 3** with `numpy` (see `scripts/requirements.txt`) for the synthetic traffic generator.
  A `.venv` is the simplest path: `python -m venv .venv && .venv/bin/pip install -r scripts/requirements.txt`.

## Building & testing

```bash
cargo build                 # build the library + both binaries
cargo build --release       # release build (always use this for benchmarking)
cargo test                  # run all unit + integration tests
cargo test <name>           # run a single test (substring match), e.g. cargo test baseline_lru
cargo clippy --all-targets  # lint (this repo enables pedantic+nursery+cargo clippy groups)
```

## Running the standalone proxy

```bash
cargo run --bin dtac_standalone -- --capacity 1024 --target http://127.0.0.1:8081 --port 8080
```

Starts an HTTP server on `--port` that proxies every request to `--target` through `DtacCache`,
exposing Prometheus metrics at `/metrics`. The client's real peer IP (via `ConnectInfo`) feeds the
router's per-IP heuristics; it does not parse `X-Forwarded-For`, so put a trusted reverse proxy in
front of it if you need that.

Only `--capacity`, `--target`, and `--port` currently do anything — `--sketch-k`, `--cm-depth`, and
`--cm-width` are accepted but unused (vestigial from an earlier Count-Min-Sketch design; the router
actually uses the `SpaceSaver` structure described above, which takes no such parameters).

## Benchmarking

Benchmarking is two steps: generate synthetic traffic with Python, then replay it in-process
against a real cache engine with Rust (no actual network I/O, so results reflect cache behavior,
not network variance).

### 1. Generate traffic logs

```bash
.venv/bin/python scripts/generate_logs.py
```

Writes one NDJSON file per bot ratio into `benchmark_logs/`:
`traffic_mixed_{10,30,50,75}pct_bot.ndjson`. Each file is built from:

- A **Zipfian item catalog** (`CATALOG_SIZE = 100_000`, `ZIPF_ALPHA = 0.8`) — a small number of
  items account for most traffic, modeling real popularity skew.
- **Human traffic**: samples the Zipfian catalog, with realistic browser headers
  (`Accept-Language`, `Sec-CH-UA`), an exponential ~80ms inter-arrival gap, and a 40% chance of a
  follow-up `/favicon.ico` request. Each simulated browser *session* gets one consistent IP shared
  across its requests, with high IP cardinality across sessions (hundreds of distinct human IPs).
- **Bot traffic**: samples the *entire* catalog uniformly (a scraper doesn't care what's popular),
  strips `Accept-Language`/`Sec-CH-UA`, and fires at a mechanical ~20ms cadence. Bot IPs are drawn
  from a small fixed pool (`BOT_IP_POOL`, ~14 addresses) — modeling a handful of scraper boxes
  hammering repeatedly, which is what the router's per-IP velocity heuristic is actually designed
  to catch (a one-IP-per-request distributed botnet would evade that heuristic regardless of
  anything this benchmark does).
- **A warm-up prefix** (`WARMUP_REQUESTS = 50_000` per file): pure-human Zipfian traffic, tagged
  `"phase": "warmup"`, prepended on the same continuous timeline ahead of the
  `"phase": "measurement"` traffic described above. This exists so the benchmark measures
  steady-state cache behavior, not cold-start misses — see [Benchmark results](#benchmark-results).

### 2. Replay a log against the cache engine

```bash
cargo run --release --bin replay_bench -- \
  --file benchmark_logs/traffic_mixed_50pct_bot.ndjson \
  --capacity 10000 \
  --algorithm dtac
```

`replay_bench` loads the NDJSON file into memory, then dispatches every record paced by its
logged timestamp (so the router's time-based heuristics see realistic cadence, not an instant
firehose), bounded by a concurrency-limiting semaphore. Warm-up records run through the real cache
exactly like any other request (so the cache actually gets populated) but are excluded from all
reported metrics and timing; the clock resets the moment the first `"measurement"`-phase record is
dispatched.

#### CLI flags

| Flag | Default | Meaning |
|---|---|---|
| `-f, --file <PATH>` | *(required)* | NDJSON log to replay |
| `-c, --capacity <N>` | `10000` | Total cache capacity (divided across 256 shards) |
| `-j, --concurrency <N>` | `128` | Max in-flight requests at once |
| `-s, --payload-size <BYTES>` | `1024` | Simulated payload size returned on a miss |
| `--speed <MULTIPLIER>` | `10000.0` | Playback speed relative to logged timestamps. `1.0` = realtime (needed for the router's per-IP velocity heuristic to see a faithful cadence); higher replays faster. Defaults fast so iteration stays quick |
| `-a, --algorithm <dtac\|lru>` | `dtac` | Which cache engine to drive: DTAC's bot-aware engine, or a standard bot-unaware sharded LRU baseline (same shard count/capacity math as DTAC, for a fair comparison) |

#### Comparing DTAC against the LRU baseline

Run the same file through both algorithms and compare the printed reports:

```bash
cargo run --release --bin replay_bench -- --file benchmark_logs/traffic_mixed_50pct_bot.ndjson --capacity 10000 --algorithm dtac
cargo run --release --bin replay_bench -- --file benchmark_logs/traffic_mixed_50pct_bot.ndjson --capacity 10000 --algorithm lru
```

Each run prints a report with three clearly-labeled tiers, reflecting the project's actual
priority order:

1. **PRIMARY METRIC** — human cache hit rate
2. **SECONDARY METRIC** — overall cache hit rate
3. **FOR REFERENCE ONLY** — bot cache hit rate (not an optimization target; a low number isn't
   the goal, it just shouldn't come at human hit rate's expense)

## Benchmark results

All runs below: `--capacity 10000`, default `--speed`, on the warm-up-aware logs described above.

### Human hit rate (primary metric) — DTAC vs. standard LRU

| Bot ratio | DTAC | Standard LRU |
|---|---|---|
| 10% | **62.18%** | 61.37% |
| 30% | **61.87%** | 60.24% |
| 50% | 58.54% | 58.55% |
| 75% | **58.26%** | 52.76% |

### Overall hit rate (secondary) and bot hit rate (reference only)

| Bot ratio | DTAC overall | LRU overall | DTAC bot | LRU bot |
|---|---|---|---|---|
| 10% | 57.73% | 57.59% | 1.75% | 10.06% |
| 30% | 47.84% | 48.45% | 2.00% | 9.95% |
| 50% | 34.98% | 38.36% | 1.99% | 10.09% |
| 75% | 20.23% | 23.66% | 2.49% | 10.08% |

### Interpretation

- **On the primary metric, DTAC matches or beats standard LRU at every bot ratio tested**, and the
  gap grows in DTAC's favor as bot traffic increases. At 75% bot ratio, DTAC's human hit rate is
  5.5 points higher than LRU's: a plain LRU cache can't tell a bot's request from a human's, so
  under heavy bot pressure it burns a large share of its limited capacity on bot-polluted entries,
  crowding out popular human content. DTAC's bot-aware admission keeps that capacity available for
  genuinely popular content instead.
- **Overall hit rate** still favors LRU at higher bot ratios, but that entire gap is attributable
  to the bot column — LRU gets "credit" for cheaply serving bots from cache, which DTAC
  deliberately declines to do. Since bot hit rate isn't a goal here, that gap isn't a regression
  on anything this project is actually trying to optimize.
- **Why warm-up matters**: without it, DTAC's human hit rate measured ~5 points *lower* than LRU's
  across the board. That gap was a structural cold-start artifact, not a real weakness: DTAC's
  ghost-cache admission requires two prior misses (`score >= PROMOTION_THRESHOLD = 2.0`, and each
  human hit only adds `1.0 - bot_prob ≈ 1.0`) before any resource can register a hit, versus plain
  LRU inserting directly into the cache on the very first miss. A short, cold-cache benchmark run
  pays that one-time admission tax disproportionately; warming the cache first (and excluding that
  warm-up traffic from the measured numbers) reveals the actual steady-state behavior, which is
  what a long-running production cache would look like.

Reproduce these numbers yourself with the exact commands in
[Comparing DTAC against the LRU baseline](#comparing-dtac-against-the-lru-baseline) across each of
the four `traffic_mixed_*pct_bot.ndjson` files.

## Known limitations

- **ASN heuristic is a stub** (`AsnDatabase::is_datacenter` always returns `false`) — real
  datacenter/hosting-provider IP lookup isn't implemented.
- **"Shy guys" (missing-asset) heuristic is dead code** — `IpState.missing_auxiliary_assets` is
  read but never set to `true` anywhere, so it never contributes to the bot score today.
- **`dtac_standalone` doesn't parse `X-Forwarded-For`/`X-Real-IP`** — it uses the direct TCP peer
  address only. Deploying behind a reverse proxy without trusted-header handling means every
  request will appear to come from the proxy's IP, defeating the per-IP heuristics.
- **The synthetic benchmark is not a security evaluation** — the bot model here (fixed small IP
  pool, mechanical timing, missing headers) is deliberately the "naive scraper" case the velocity
  and header heuristics are designed to catch. A more sophisticated adversary (distributed IPs,
  randomized timing, spoofed headers) would evade these heuristics; this benchmark doesn't claim
  otherwise.
- **`lru = "0.12"` is a benchmark-tool-only dependency** (used solely by `replay_bench`'s baseline),
  not part of DTAC's own library or caching behavior.
