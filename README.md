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
- [Thesis vs. implementation](#thesis-vs-implementation)

## Architecture

DTAC (`src/dtac_cache.rs`) composes two independently-testable halves:

### Heuristic router (`src/heuristic_router/`)

`HeuristicRouter::evaluate_request` scores each *request* with a bot probability using a log-odds
(logistic-regression-style) model: start from a baseline prior, add weighted contributions from a
few independent heuristics, then squash through a sigmoid into a `[0, 1]` bot score (`AiScore`):

- **Prior**: the baseline log-odds `l_0 = ln(p / (1 − p))` comes from an explicit assumed bot base
  rate `prior_bot_rate` (default `0.1`, so `l_0 ≈ −2.20`): "assume human until the evidence says
  otherwise". A clean human request scores `bot_prob ≈ 0.1`.
- **Velocity**: per-IP request rate, tracked as a fixed-point EWMA with bitwise-shift decay. A
  graded penalty `alpha · ln(velocity / burst_threshold)` kicks in once an IP's rate crosses
  `burst_threshold`.
- **ASN**: flags datacenter/hosting-provider IPs. `AsnDatabase` (`asn_db.rs`) loads iptoasn.com
  `ip2asn` TSV ranges and binary-searches them (`O(log n)`), then checks the ASN against a curated
  list of hosting/cloud providers (`DEFAULT_DATACENTER_ASNS`, overridable). With no database
  loaded (the default), the heuristic never fires.
- **"Shy guys"**: penalizes IPs that have made `shy_page_threshold` (default 5) page requests and
  have *never* fetched an auxiliary asset (favicon, CSS, JS, images, fonts). Browsers fetch those
  once and then cache them, so "never" rather than "not since the last page" is the signal; the
  threshold is a grace period so short human visits aren't penalized.
- **Headers**: penalizes requests missing an `Accept-Language` header.

All weights and thresholds live in `RouterConfig` (`config.rs`), which deserializes from JSON so
every constant can be changed without recompiling (see `replay_bench --router-config`).

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
  heuristic_router/   bot-score computation (velocity, ASN, "shy guys", header heuristics)
  cache_engine/        sharded randomized-eviction cache with ghost-cache admission
  dtac_cache.rs        combines the two + telemetry
  bin/
    dtac_standalone.rs reverse-proxy binary
    replay_bench.rs    benchmark replay tool (DTAC vs. a standard-LRU baseline)
    common/            CLI flags shared by both binaries (ASN database loading)
data/asn/
  ip2asn-fixture.tsv   small committed ip2asn fixture for tests/benchmarks (full downloads gitignored)
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

To enable the datacenter-ASN heuristic, pass one or more ip2asn files (see
[ASN data](#asn-data)):

```bash
cargo run --bin dtac_standalone -- --asn-db data/asn/ip2asn-v4.tsv --asn-db data/asn/ip2asn-v6.tsv
```

| Flag | Meaning |
|---|---|
| `--asn-db <PATH>` | iptoasn `ip2asn` TSV (v4, v6 or combined). Repeatable; files are merged. Without it, the ASN heuristic never fires |
| `--datacenter-asns <PATH>` | One ASN per line (`AS` prefix and `#` comments allowed), replacing the built-in datacenter list. Requires `--asn-db` |

Only `--capacity`, `--target`, `--port` and the ASN flags currently do anything — `--sketch-k`, `--cm-depth`, and
`--cm-width` are accepted but unused (vestigial from an earlier Count-Min-Sketch design; the router
actually uses the `SpaceSaver` structure described above, which takes no such parameters).

### ASN data

The repository ships only `data/asn/ip2asn-fixture.tsv`, a tiny fixture in real ip2asn format.
For real lookups, download the public-domain data from <https://iptoasn.com/data/>: either
`ip2asn-v4.tsv.gz` **and** `ip2asn-v6.tsv.gz`, or `ip2asn-combined.tsv.gz`. Gunzip them into
`data/asn/` (ignored by git) and pass each file with `--asn-db`.

The fixture's datacenter rows were written to cover the generator's own datacenter bot IPs
(`45.33.*`, `104.248.*`, `167.172.*`); its TEST-NET bot IPs (`198.51.100.*`, `203.0.113.*`) are
deliberately left unmatched. **ASN detection in the benchmark is therefore true by construction:**
it shows the plumbing works and how much a correct ASN signal helps the cache, not that ASN
detection works on real traffic.

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
| `--asn-db <PATH>` | *(none)* | iptoasn `ip2asn` TSV for the datacenter-ASN heuristic. Repeatable; files are merged. E.g. `--asn-db data/asn/ip2asn-fixture.tsv` |
| `--datacenter-asns <PATH>` | *(built-in list)* | One ASN per line, replacing the default datacenter list. Requires `--asn-db` |
| `--router-config <PATH>` | *(defaults)* | JSON object of `RouterConfig` fields (`prior_bot_rate`, `w_asn`, `w_fav`, `w_hdr`, `burst_threshold`, `alpha`, `shy_page_threshold`); omitted fields keep their defaults, unknown fields are rejected |
| `--prior-bot-rate <P>` | `0.1` | Router prior bot rate in `(0, 1)`; overrides the value from `--router-config`. `0.5` reproduces the original zero prior |

The effective router configuration is printed at startup. Setting `RUST_LOG` (e.g.
`RUST_LOG=dtac=info`) prints per-request router decisions to stderr; leave it unset for
throughput measurements.

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

> **These results predate plans 001–004** (working ASN and "Shy Guys" heuristics, the non-zero
> router prior, simulated time, new workloads) and will be replaced once those land. They were
> produced with the original zero prior and with the ASN and "Shy Guys" heuristics inactive.

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
  ghost-cache admission needs several human requests before a resource enters the main cache —
  each adds `1 − bot_prob_human` to its score and promotion requires `PROMOTION_THRESHOLD`, so it
  takes `ceil(PROMOTION_THRESHOLD / (1 − bot_prob_human))` requests (4 under the original zero
  prior, where `bot_prob_human = 0.5`; 3 under the current default prior of `0.1`) — versus plain
  LRU inserting directly into the cache on the very first miss. A short, cold-cache benchmark run
  pays that one-time admission tax disproportionately; warming the cache first (and excluding that
  warm-up traffic from the measured numbers) reveals the actual steady-state behavior, which is
  what a long-running production cache would look like.

Reproduce these numbers yourself with the exact commands in
[Comparing DTAC against the LRU baseline](#comparing-dtac-against-the-lru-baseline) across each of
the four `traffic_mixed_*pct_bot.ndjson` files.

## Known limitations

- **The "Shy guys" heuristic is trivially evaded** — it is IP-level, so a bot that fetches a single
  favicon (or any asset) once per IP is never flagged again.
- **The datacenter ASN list is curated, not complete** — `DEFAULT_DATACENTER_ASNS` covers major
  cloud/hosting providers only; bots on residential proxies or unlisted hosts are not caught. The
  benchmark's ASN signal is true by construction (see [ASN data](#asn-data)).
- **NAT / shared IPs** — all per-IP heuristics (velocity, "Shy guys") treat every client behind one
  address as a single client, so a busy NAT can look like a bot and a bot can hide among humans.
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
- **The ghost cache is unbounded** — entries are only removed on promotion, so every distinct
  resource ever requested (including every one-off bot scan URL) stays in `ghost_cache` forever.
- **Time-based logic runs on wall-clock time, not log timestamps** — the router uses `SystemTime`
  and the cache engine uses `Instant`, so `--speed` changes the experiment itself. At the default
  `--speed 10000`, very little wall-clock time passes during a replay, so score decay barely
  happens and velocity is measured on a compressed timeline.

## Thesis vs. implementation

This section compares the written thesis (Ch. 1 Introduction, Ch. 2 Literature Review, Ch. 3
Methodology) against the code in this repo, as of commit `a5cf120`.

### Differences between the thesis and the code

"Fix text" means the code is reasonable and the thesis should be reworded; "Implement" means the
thesis describes something worth building.

| # | Thesis says (section) | Code actually does | Action |
|---|---|---|---|
| 1 | DTAC is a "modified LRU" / "AI-Aware LRU" with a doubly linked list, MRU head vs. LRU tail insertion, bots "routed to the eviction tail" (Ch1 obj. 3, §3.1, §3.2.2, §3.3.2, §3.6.2, §3.7.2, §3.7.3) | No linked list, no head/tail. Ghost-cache admission + per-resource decayed score + random-sample eviction tournament (§3.4.2 describes this correctly) | Fix text |
| 2 | Scoring runs in O(1) (Ch1 obj. 2, scope, §2.4, §2.8, Alg. 1 line 5, §3.7.3) | `SpaceSaver` uses two `BTreeSet`s → O(log K) per request with fixed K | Fix text (or implement O(1) stream-summary buckets) |
| 3 | SpaceSaver uses "O(1) space" (§3.4.1) | O(K) space, bounded by `capacity` | Fix text |
| 4 | Strict bounded memory everywhere (scope §1.4.2) | Ghost cache is an unbounded `HashMap` | Implement (bound the ghost cache) |
| 5 | "Shy Guys" favicon heuristic heavily penalizes sessions (§3.2.2, §3.4.1, Alg. 1 heuristic C) | Flags an IP after `shy_page_threshold` (default 5) page requests with no auxiliary-asset fetch ever | Done |
| 6 | Cross-references datacenter ASN databases (§3.4.1, Alg. 1 heuristic B) | Offline iptoasn `ip2asn` lookup + curated datacenter-ASN list (`--asn-db`); benchmark uses a fixture that matches the generator's bot IPs by construction | Done |
| 7 | Penalties for "missing or misordered headers", `Sec-CH-UA`, User-Agent (§3.4.1, §3.5.2) | Only checks presence of `Accept-Language` | Fix text (or implement more header checks) |
| 8 | Weights are log-likelihood ratios `W_i = ln(P(E|Bot)/P(E|Human))`, "Naive Bayes" (§3.4.1, §3.4.3) | Hand-picked defaults `w_asn=1.5`, `w_fav=1.2`, `w_hdr=1.0` (configurable via `RouterConfig`); hand-weighted log-odds model. The prior is now an explicit, documented base rate (`prior_bot_rate = 0.1`, `l_0 = ln(p/(1−p))`) | Fix text (or estimate weights from data) |
| 9 | `ln` approximated with base-2 integer log / bit counting (§3.4.1 note) | Uses `f64::ln()` | Fix text (delete the note) |
| 10 | Recommended `v_τ = 15–30`, `α = 2.5–3.0` (§3.4.3) | `burst_threshold = 10.0`, `alpha = 2.0` (`RouterConfig` defaults) | Fix text or change constants |
| 11 | Derives `N = 14` as the default sample size (§3.4.3) | `N_SAMPLES = 5` | Pick one; make text and code agree |
| 12 | Eviction overhead "drops to zero" (§3.4.2) | O(N) per eviction | Fix text |
| 13 | Score appended as `X-Bot-Probability` header and forwarded to a separate cache (§3.2.3, §3.4.1) | Single process; `DtacCache::access` passes the score directly in memory | Fix text |
| 14 | Alg. 2: `fetch_from_origin` inside the shard lock; `score >> delta_t` on a float | Fetch happens outside the lock, with a re-check for concurrent promotion; decay is `score / 2^dt`, zeroed after 32 s | Fix text (the real design is better — describe it) |
| 15 | Data generator and replayer are "custom Rust scripts" (§3.3.3, §3.5) | Generator is Python (`scripts/generate_logs.py`); replayer is Rust | Fix text |
| 16 | Bots sample uniformly from a "separate subset" of the catalog (§3.5.2) | Bots sample uniformly from the **entire** catalog, overlapping with human items | Fix text |
| 17 | Bots identified via omitted favicon, missing headers, mechanical timing (§3.5.2) | True of the generator, but the router only sees velocity + `Accept-Language` (see #5, #7); bots use a fixed 14-IP pool | Fix text; state the 14-IP assumption |
| 18 | Warm-up runs "until the cache reached absolute maximum capacity" (§3.6.1) | Fixed 50,000 pure-human requests prepended to every log | Fix text |
| 19 | Replayer fires requests at the proxy over `localhost` (§3.3.3, §3.6.1) | `replay_bench` is in-process; no network | Fix text (or add a networked run against `dtac_standalone`) |
| 20 | The 10/30/50/75% logs are dispatched "concurrently" (§3.6.1) | One separate run per ratio | Fix text |
| 21 | Every eviction is logged with whether the evicted object was human or bot (§3.6.1) | Not tracked | Implement or drop |
| 22 | Byte Hit Rate (§3.7.1) | Not computed; payload size is fixed, so BHR would equal object hit rate | Implement variable payload sizes, or drop BHR |
| 23 | Cache Pollution Index (§3.7.2) | Not computed | Implement or drop |
| 24 | Proxy scoring overhead `T_proxy` and cache overhead `T_cache` measured separately (§3.7.3) | Router records an `eval_duration` histogram; `replay_bench` reports only combined average access time | Implement split timing |
| 25 | Memory consumption, read-path CPU overhead and "hit-density" evaluated (scope §1.4.2) | None measured; "hit-density" never defined | Implement or drop |
| 26 | Experiments run on a dedicated 8-core cloud bare-metal server (§3.3.1) | Not recorded in the repo | Confirm before submitting |
| 27 | Text is in past tense ("were executed", "was tracked") (Ch. 3 throughout) | Several of those steps have not been done (see above) | Fix text |
| 28 | Leftover `Gemini_Generated_Image_fuhyknfuhyknfuhy.png` reference and "Diagram 1/2" (§3.2) | Figures are labelled `fig:methodology`, `fig:architecture` | Fix text (use `\ref`) |

### Implementation checklist

**Already implemented**

- [x] Log-odds bot scorer with sigmoid output in `[0, 1]` (`HeuristicRouter::evaluate_request`)
- [x] Per-IP fixed-point EWMA velocity with bitwise-shift decay
- [x] Graded logarithmic velocity penalty above `burst_threshold`
- [x] `Accept-Language` header heuristic
- [x] "Shy Guys" heuristic: `missing_auxiliary_assets` set from per-IP page vs. auxiliary-asset fetch history
- [x] Real datacenter ASN lookup (offline iptoasn `ip2asn` database), with a fixture covering the generator's datacenter-range bot IPs so it is exercised
- [x] Configurable router constants (`RouterConfig`, `--router-config`) with an explicit prior bot rate
- [x] Bounded-memory per-IP state via SpaceSaver (evict-min, count inheritance, half-life decay)
- [x] 256-way lock-sharded cache engine
- [x] Metadata-only ghost cache with promotion at `PROMOTION_THRESHOLD = 2.0`
- [x] Per-resource score increment of `1 − bot_prob` with time decay
- [x] Random-sample eviction tournament (`N_SAMPLES = 5`) with O(1) `swap_remove`
- [x] Origin fetch outside the lock, safe against concurrent duplicate promotion
- [x] `DtacCache` wiring router → engine, with OpenTelemetry metrics and `tracing` spans
- [x] `dtac_standalone` Axum reverse proxy with real peer IP and Prometheus `/metrics`
- [x] Synthetic traffic generator: Zipf(α=0.8, 100k items) humans, uniform bots, 10/30/50/75% ratios
- [x] Human-only warm-up prefix, excluded from measurements
- [x] `replay_bench` with timestamp pacing, concurrency limit, human/bot/overall hit rates, throughput, average access time
- [x] Bot-unaware sharded LRU baseline (`--algorithm lru`)
- [x] DTAC-vs-LRU results at all four bot ratios ([Benchmark results](#benchmark-results))
- [x] Unit + integration tests for the router, SpaceSaver, cache engine and benchmark helpers

**To implement** (roughly in priority order for the thesis)

- [ ] Ablation run: DTAC with `bot_prob` forced to `0` — separates the effect of the AI score from the effect of ghost-cache admission alone
- [ ] Scan-resistant baselines: SIEVE and/or S3FIFO (Ch. 2 cites them as already strong under scans; beating only LRU is a weak claim)
- [ ] Bound the ghost cache (e.g. per-shard cap with FIFO eviction, as in S3FIFO's ghost queue)
- [ ] Drive time-based logic from log timestamps (or report results at `--speed 1.0`) so decay and velocity behave as designed
- [ ] Repeated runs with different seeds, reporting mean ± spread
- [ ] Split latency: `T_proxy` (router) and `T_cache` (engine) reported separately, with p50/p99, not just the average
- [ ] Memory measurement (peak RSS or structure sizes) for DTAC vs. LRU
- [ ] Eviction logging by origin (human vs. bot) and a Cache Pollution Index (share of main-cache entries first admitted by bot-scored requests)
- [ ] Variable payload sizes in the generator, so Byte Hit Rate means something (or drop BHR)
- [ ] Optional: networked benchmark through `dtac_standalone` to match the thesis's "over localhost" setup
- [ ] Optional: "stealthy bot" workload (headers present, rotating IPs, jittered timing) to show where the heuristics stop working

### Status of the research objectives

From Ch. 1 §1.3.2:

| Objective | Status | Notes |
|---|---|---|
| 1. Lightweight passive heuristic detector using HTTP header anomalies and structural requests | **Achieved (synthetic evaluation)** | Velocity, `Accept-Language`, the structural ("Shy Guys") heuristic and datacenter-ASN lookup all work. Caveats: the benchmark's ASN fixture matches the generator's bot IPs by construction, and the current workload cannot produce a "Shy Guys" false positive; classifier metrics on realistic sessions (plan 003) are needed before calling it plainly achieved. |
| 2. Real-time continuous AI-score in `[0, 1]` in O(1) | **Achieved except the O(1) claim** | The score is continuous and computed per request. Cost is O(log K) because of the `BTreeSet`s. True O(1) is possible with a stream-summary bucket list, but the half-life count decay makes it hard. Most practical route: reword to "bounded O(log K), independent of traffic volume". |
| 3. DTAC that modifies LRU eviction using the AI-score | **Achieved in substance, not as worded** | The AI-score does drive admission and eviction, but the design is not a modified LRU. Reword the objective rather than rebuilding to match it. |
| 4. Benchmark DTAC hit rates against standard LRU under simulated mixed workloads | **Achieved (basic version)** | Human hit rate: DTAC ≥ LRU at every ratio, +5.5 points at 75% bots. Missing: ablation, SIEVE/S3FIFO baselines, repeated runs. The 50% ratio is a tie. |

Other goals and claims from the scope and methodology:

| Goal / claim | Status |
|---|---|
| Bounded memory for client state (SpaceSaver) | Achieved |
| Bounded memory for the whole system | Not achieved — ghost cache is unbounded. Achievable. |
| Lock-sharded, concurrent cache | Achieved |
| Measure memory consumption and read-path CPU overhead | Not done. Achievable. |
| `T_proxy` below 2 ms | Not separately measured. Likely achievable: average combined access time is reported in µs. |
| Byte Hit Rate, Cache Pollution Index, eviction-by-origin logging | Not done. Achievable, but BHR needs variable payload sizes to mean anything. |
| Human hit rate stays stable regardless of bot volume (§3.7.1) | **Not achieved, and probably not achievable as stated.** DTAC's human hit rate still falls from 62.18% to 58.26% between 10% and 75% bots (LRU falls ~8.6 points). Reword to "degrades less than LRU". |
| CPI kept near 0% via tail insertion (§3.7.2) | **Not achievable as stated** — there is no tail insertion. A CPI could still be measured for the real design, but "near 0%" is not guaranteed. |
| Heuristic weights derived as real log-likelihood ratios | **Probably not achievable within scope.** Needs labelled real-world traffic. Estimating them from the synthetic generator would be circular, because the generator was written to trigger these exact heuristics. |
| Detecting sophisticated bots (rotating IPs, spoofed headers, human-like timing) — motivated in Ch. 1 §1.1.4 | **Not achievable with the current heuristics.** Velocity is per-IP, and the header check is trivially spoofed. State this as a limitation, not a goal. |
| "No existing literature" on bot-score-aware eviction (Ch. 2 §2.8) | Not verifiable — soften to "to the best of our knowledge". |
