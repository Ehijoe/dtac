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
  Every miss lands here first, so a one-off scan doesn't evict genuinely popular content. The
  ghost is bounded at `ghost_multiplier` (default 4) times the main-cache capacity per shard. When
  it is full, a miss first evicts a ghost entry with the same score-aware tournament as the main
  cache. Bot-inserted entries start near score 0 and lose first, so a uniform bot scan cannot push
  popular human items out of the ghost before their second hit (plain FIFO would let it).
- **Promotion**: a ghost-cache entry is promoted into the real cache (fetching the payload) once
  its EWMA popularity score crosses `promotion_threshold` (default `PROMOTION_THRESHOLD = 2.0`).
- **Scoring**: each hit increases a resource's score by `1.0 - request_bot_prob` — human requests
  contribute close to a full point, bot-like requests contribute close to nothing. This is the
  mechanism that actually connects the router's output to caching behavior.
- **Eviction**: a randomized tournament — sample `n_samples` (default 5) entries, evict whichever
  has the lowest *currently decayed* score, breaking ties by the oldest last access (so once scores
  have decayed to 0, eviction falls back to LRU order instead of being random). A cheap
  approximation of LFU/LRU that avoids a full priority queue.

All engine constants live in `EngineConfig` (`cache_engine/config.rs`, JSON-deserializable; see
`replay_bench --engine-config`).

### Time

Every time-based decision reads an injected `Clock` (`src/clock.rs`): router velocity,
`SpaceSaver` count decay and cache score decay. `SystemClock` (the default) is the wall clock.
`ManualClock` only moves when told to, which lets `replay_bench --clock log` run on the log's own
timestamps and lets tests script time. `DtacCache::builder(..).clock(..)` hands one clock to both
the router and the engine.

**Time constants.** These are now parameters, not hard-coded values: `velocity_tick_ms`,
`velocity_reset_ticks` and `space_saver_half_life_s` in `RouterConfig`, and `decay_half_life_ms`,
`decay_cutoff_half_lives`, `promotion_threshold`, `n_samples` and `ghost_multiplier` in
`EngineConfig`. Their defaults reproduce the original behaviour: halve a score every second, zero
it after 32 s, one velocity shift per second. The defaults are **provisional** until plan 004's
calibration. They were only reasonable when `--speed 10000` squeezed a replay into a few wall-clock
seconds. On log time, cache scores decay away before most items collect enough hits to promote, and
the velocity heuristic never fires on the benchmark's bots (about 3.6 req/s per IP).

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
  clock.rs             injectable time source (system clock, or manual/log-driven)
  bin/
    dtac_standalone.rs reverse-proxy binary
    replay_bench/      benchmark replay tool (DTAC vs. a standard-LRU baseline)
      main.rs            CLI, log loading, dispatch loop, --json-out
      metrics.rs         counters, hit/byte-hit rates, latency histograms, memory, the report
      engines.rs         the engines a log can be replayed against (DTAC, sharded LRU)
      classifier.rs      the router scored as a classifier (ROC-AUC, FPR/TPR)
      pollution.rs       eviction-by-origin counts and the Cache Pollution Index
    common/            CLI flags shared by both binaries (ASN database loading)
data/asn/
  ip2asn-fixture.tsv   small committed ip2asn fixture for tests/benchmarks (full downloads gitignored)
scripts/
  generate_logs.py     synthetic traffic generator
  test_generate_logs.py
tests/                 integration tests (cache engine, heuristic router, DtacCache builder)
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
.venv/bin/python scripts/generate_logs.py --seed 1            # both workloads, all four ratios
.venv/bin/python scripts/generate_logs.py --seed 1 --legacy   # the pre-plan-003 traffic model
```

Writes one NDJSON file per workload and bot ratio into `--out-dir` (default `benchmark_logs/`):
`traffic_mixed_{10,30,50,75}pct_bot.ndjson` (naive bots) and
`traffic_stealthy_{10,30,50,75}pct_bot.ndjson` (stealthy bots). Next to each file it writes a
`.meta.json` sidecar with the seed, the ratio, the workload, every generator argument, derived
counts (window, bot pool size, ...) and the generator's git commit. `replay_bench` copies it
into `--json-out`. The same `--seed` always produces byte-identical files.

**Traffic model** (the default since plan 003):

- **Catalog**: Zipfian (`--catalog-size 100000`, `--zipf-alpha 0.8`). Every item has a fixed
  size, drawn once per item id from a lognormal with a median of 16 KB and clipped to
  [512 B, 2 MB] (`--item-size-*`). The same item has the same `size_bytes` in every file, for
  every seed. `/favicon.ico` is 1 KB.
- **Humans: one shared stream per seed.** The same human records appear in every ratio file
  and in both workloads, so human hit rates are directly comparable across ratios and, as
  paired samples, across algorithms. Humans arrive as browser **sessions** (Poisson arrivals):
  one IP per session, a geometric number of pages (`--pages-per-session-mean 5`) drawn from the
  Zipf catalog, and exponential think time between pages (`--think-time-mean 8` s). Sessions
  are frequent enough to keep the aggregate rate at `--human-rate 12.5` req/s, about the old
  model's rate, so decay dynamics stay comparable.
  - A **new visitor** fetches `/favicon.ico` once, right after the first page.
    A **returning visitor** (`--returning-frac 0.3`) already has it cached and never fetches
    it. Returning visitors who view `shy_page_threshold` (5) or more pages are the realistic
    "Shy Guys" false positives. Records carry `"visitor": "new" | "returning"`.
  - `--nat-frac` (default 0) puts that share of sessions behind `--nat-ips` (16) shared IPs,
    for a sensitivity run.
  - Human IPs never fall in the ASN fixture's datacenter ranges or in TEST-NET, so per-IP and
    ASN signals can't collide with bot IPs by chance.
- **A warm-up prefix**: the first `--warmup-requests` (≈50 000) human requests are tagged
  `"phase": "warmup"`. No bots run during warm-up. The benchmark measures steady-state
  behaviour, not cold-start misses; see [Benchmark results](#benchmark-results).
- **Bots: spread over the same window.** For ratio `r` and `H` human measurement requests
  (`--human-requests`, ≈100 000), a file gets `B = H·r/(1−r)` bot requests spread over the
  whole measurement window, so the bot share is about `r` in every tenth of the timeline.
  Bot records carry `"bot_kind"`.
  - **Naive** (`traffic_mixed_*`): mechanical scrapers with stripped headers that sample the
    *entire* catalog uniformly. The IP pool grows with the ratio,
    `ceil((B/T) / --bot-ip-rate)` IPs, so each IP always sends at `--bot-ip-rate` (3.6 req/s,
    ±5% jitter) and only the number of IPs changes with the ratio, not their speed. IPs run in
    30 s on/off cycles to hit the exact aggregate rate. About 64% of the pool sits in the ASN
    fixture's datacenter ranges and the rest in TEST-NET (unmatched by any ASN).
  - **Stealthy** (`traffic_stealthy_*`): bots that imitate browser sessions. They send full
    browser headers (`Accept-Language`, `Sec-CH-UA`, a human User-Agent) from fresh
    residential-looking IPs, about 5 requests per IP (`--stealthy-requests-per-ip`), with
    exponential think times. Each IP fetches one `/favicon.ico`, which defeats the Shy Guys
    heuristic. They still scan the catalog uniformly, so they still pollute the cache. Session
    starts are spread evenly over the window, so the bot share stays stationary. This workload
    shows where the heuristics stop working: on it the router is no better than chance (see
    [Known limitations](#known-limitations)).

**`--legacy`** reproduces the original generator byte for byte for a given `--seed` (the test
suite checks this against hashes taken from the original script). It is naive-only, with a
fixed 200 000-request measurement phase (`--total-requests`). It has two flaws the default
model fixes:

1. The bot ratio was not stationary. Bots fired every 20 ms and humans every 80 ms, both
   starting with measurement, so at "10% bots" the bot stream ended after about 3% of the
   window.
2. Every human request came from a fresh IP, so per-IP heuristics separated humans perfectly
   by construction.

The results in [Benchmark results](#benchmark-results) come from that legacy model. The old
script took no seed, so those exact files can't be regenerated, but `--legacy` reproduces the
model.

### 2. Replay a log against the cache engine

```bash
cargo run --release --bin replay_bench -- \
  --file benchmark_logs/traffic_mixed_50pct_bot.ndjson \
  --capacity 10000 \
  --algorithm dtac
```

`replay_bench` loads the NDJSON file into memory, then dispatches every record paced by its
logged timestamp (scaled by `--speed`), bounded by a concurrency-limiting semaphore. With the
default `--clock log`, the router and engine read time from a simulated clock that is set to each
record's timestamp just before its access. Decay and velocity are therefore measured on the log's
timeline, and `--speed` only changes how long the replay takes. Warm-up records run through the real cache
exactly like any other request (so the cache actually gets populated) but are excluded from all
reported metrics and timing; the clock resets the moment the first `"measurement"`-phase record is
dispatched.

#### CLI flags

| Flag | Default | Meaning |
|---|---|---|
| `-f, --file <PATH>` | *(required)* | NDJSON log to replay |
| `-c, --capacity <N>` | `10000` | Total cache capacity (divided across 256 shards) |
| `-j, --concurrency <N>` | `128` | Max in-flight requests at once |
| `-s, --payload-size <BYTES>` | `1024` | Payload size returned on a miss for records without `size_bytes` (older logs). Records with `size_bytes` get exactly that size, as a slice of one shared zeroed buffer |
| `--speed <MULTIPLIER>` | `10000.0` | Playback speed relative to logged timestamps. `1.0` = realtime; higher replays faster. With `--clock log` it only changes the run time; with `--clock wall` it also compresses the timeline that decay and velocity see |
| `--clock <log\|wall>` | `log` | Time source for decay and velocity. `log`: a simulated clock set to each record's timestamp before its access. Because the clock never moves backwards, a request can see a time a few ms ahead of its own when a later record's task set it first (up to `--concurrency` tasks are in flight); with half-lives of seconds this is negligible. `wall`: the real clock, reproducing pre-plan-002 behaviour |
| `--engine-config <PATH>` | *(defaults)* | JSON object of `EngineConfig` fields (`decay_half_life_ms`, `decay_cutoff_half_lives`, `promotion_threshold`, `n_samples`, `ghost_multiplier`); omitted fields keep their defaults, unknown fields are rejected |
| `--ghost-multiplier <N>` | `4` | Ghost-cache capacity as a multiple of main-cache capacity; overrides the value from `--engine-config` |
| `-a, --algorithm <dtac\|lru>` | `dtac` | Which cache engine to drive: DTAC's bot-aware engine, or a standard bot-unaware sharded LRU baseline (same shard count/capacity math as DTAC, for a fair comparison) |
| `--asn-db <PATH>` | *(none)* | iptoasn `ip2asn` TSV for the datacenter-ASN heuristic. Repeatable; files are merged. E.g. `--asn-db data/asn/ip2asn-fixture.tsv` |
| `--datacenter-asns <PATH>` | *(built-in list)* | One ASN per line, replacing the default datacenter list. Requires `--asn-db` |
| `--router-config <PATH>` | *(defaults)* | JSON object of `RouterConfig` fields (`prior_bot_rate`, `w_asn`, `w_fav`, `w_hdr`, `burst_threshold`, `alpha`, `shy_page_threshold`, `velocity_tick_ms`, `velocity_reset_ticks`, `space_saver_half_life_s`); omitted fields keep their defaults, unknown fields are rejected |
| `--prior-bot-rate <P>` | `0.1` | Router prior bot rate in `(0, 1)`; overrides the value from `--router-config`. `0.5` reproduces the original zero prior |
| `--cpi-sample-every <N>` | `10000` | Sample the Cache Pollution Index every N measurement requests, and once more at the end |
| `--json-out <PATH>` | *(none)* | Write every reported metric plus the run configuration (file, algorithm, capacity, clock, effective router/engine configs, the log's `.meta.json`, git commit) as JSON |
| `-q, --quiet` | off | Hide the progress bar and the Prometheus dump, for scripted runs |

The clock mode and the effective router and engine configurations are printed at startup. Setting
`RUST_LOG` (e.g. `RUST_LOG=dtac=info`) prints per-request router decisions to stderr; leave it
unset for throughput measurements.

#### Comparing DTAC against the LRU baseline

Run the same file through both algorithms and compare the printed reports:

```bash
cargo run --release --bin replay_bench -- --file benchmark_logs/traffic_mixed_50pct_bot.ndjson --capacity 10000 --algorithm dtac
cargo run --release --bin replay_bench -- --file benchmark_logs/traffic_mixed_50pct_bot.ndjson --capacity 10000 --algorithm lru
```

Each run prints a report with three clearly-labeled tiers, reflecting the project's actual
priority order:

1. **PRIMARY METRIC**: human cache hit rate, with the human **Byte Hit Rate** beside it.
2. **SECONDARY METRIC**: overall cache hit rate and overall BHR.
3. **FOR REFERENCE ONLY**: bot cache hit rate and BHR. This is not an optimization target. A
   low number isn't the goal; it just shouldn't come at the human hit rate's expense.

After the tiers, the report has these sections:

- **Latency**: p50/p90/p99/max in µs for `T_proxy` (router), `T_cache` (engine, including the
  instant origin fetch) and the whole access. Each phase is recorded in nanoseconds into
  `hdrhistogram` histograms (2 significant digits), striped across 32 mutexes and merged at the
  end. The baselines have no router, so they report only `T_cache` and the total. DTAC's total
  also includes `DtacCache`'s own OpenTelemetry recording, which runs after both phase timers
  stop, so it is larger than `T_proxy + T_cache`.
- **Memory**: peak RSS (`VmHWM`, Linux only), the RSS delta over the replay, main/ghost entry
  counts, `SpaceSaver` tracked IPs and **logical resident bytes** (the sum of `size_bytes` over
  resident keys). The in-memory log dominates absolute RSS, so the delta is the meaningful
  number. The log stays loaded across both readings, so the delta measures what the cache
  accumulated (ghost entries, `SpaceSaver`, maps) plus the bench's own bookkeeping (CPI ledger,
  classifier samples). **Payloads are excluded from RSS by construction**: every cached payload
  is a slice of one shared buffer, which is why logical resident bytes are reported separately.
- **For reference: cache pollution**, from the log's ground-truth labels and engine-agnostic:
  - **Evictions by admitter**: main-cache evictions split by whether a human or a bot request
    triggered the victim's admission (the label of its most recent origin fetch). DTAC reports
    evictions through `DtacCacheEngine::with_eviction_listener`; ghost evictions are not
    counted.
  - **Cache Pollution Index (CPI)**: the share of resident entries that **no human has requested
    since they entered the cache**, i.e. the capacity actually wasted on bot traffic. It is
    reported as the mean over samples and the final value. A bot-triggered admission of an item
    humans then use is *not* pollution. For DTAC, the request that tips a score over the
    promotion threshold isn't necessarily responsible for the admission; humans may have
    contributed most of the score.
  - **Bot-triggered share**: the raw share of resident entries whose admission a bot request
    triggered. It is reported next to the CPI, clearly labelled, because it is what thesis
    §3.7.2 literally describes. It penalizes LRU whenever bots happen to fetch popular items.
- **Router as a classifier** (DTAC only), measured independently of the cache:
  - **ROC-AUC** of `bot_prob` against the ground-truth label, computed by sorting with ties
    averaged.
  - Precision, recall and FPR at `bot_prob > 0.5`.
  - The mean `bot_prob` per class and the **gap** between the class means. The cache adds
    `1 − bot_prob` per hit, so this gap is what actually drives cache behaviour.
  - The human FPR split into **new vs. returning visitors** (Shy Guys false positives).
  - `--json-out` adds per-label stats and a 20-bin `bot_prob` histogram per class.

**Byte Hit Rate caveat**: capacity counts **entries, not bytes**, and no engine (DTAC included)
takes object size into account when admitting or evicting. BHR here reflects how hits on popular
items happen to correlate with size. It doesn't measure a size-aware policy.

## Benchmark results

All runs below: `--capacity 10000`, default `--speed`, on the warm-up-aware logs described above.

> **These results predate plans 001–004** (working ASN and "Shy Guys" heuristics, the non-zero
> router prior, simulated time, new workloads) and will be replaced once those land. They were
> produced with the original zero prior and with the ASN and "Shy Guys" heuristics inactive.
> They also ran on the **wall clock** (before plan 002 added `--clock log`, now the default), so
> decay and velocity ran on a timeline compressed by `--speed`. Rerun with `--clock wall` to
> compare like for like. They also used the **legacy traffic model** (non-stationary bot ratio,
> one fresh IP per human request; regenerate it with `generate_logs.py --legacy`). Plan 004
> refreshes these tables.
>
> **Preliminary numbers on the plan-003 model are worse for DTAC.** In plan 003's verification
> run (seed 1, 75% naive bots, `--capacity 10000`, `--asn-db data/asn/ip2asn-fixture.tsv`,
> default configs), DTAC's human hit rate was 20.5% against LRU's 41.2%, with only 23 entries in
> DTAC's main cache. With the default 1 s score half-life, an item needs about three human
> requests within a few seconds to be promoted, and multi-page sessions with 8 s think times
> rarely deliver that. LRU's CPI was 79.6% (bot-triggered share 81.1%). The router itself
> separated naive bots perfectly (AUC 1.000, `T_proxy` p99 ≈ 35 µs). The time constants and the
> prior are not calibrated until plan 004 Part 0, so treat this as a calibration problem to
> solve there, not as a final result.

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

- **The "Shy guys" heuristic is trivially evaded**: it works per IP, so a bot that fetches a
  single favicon (or any asset) once per IP is never flagged again. The stealthy workload does
  exactly this.
- **Shy Guys false positives are real but mild**: returning visitors never fetch the favicon,
  so the heuristic flags them once they have viewed 5 pages. In plan 003's verification run
  their mean `bot_prob` was 0.17 against 0.10 for new visitors. The penalty alone stays below
  the 0.5 threshold, so the human FPR at 0.5 was 0% for both.
- **The datacenter ASN list is curated, not complete** — `DEFAULT_DATACENTER_ASNS` covers major
  cloud/hosting providers only; bots on residential proxies or unlisted hosts are not caught. The
  benchmark's ASN signal is true by construction (see [ASN data](#asn-data)).
- **NAT / shared IPs** — all per-IP heuristics (velocity, "Shy guys") treat every client behind one
  address as a single client, so a busy NAT can look like a bot and a bot can hide among humans.
- **`dtac_standalone` doesn't parse `X-Forwarded-For`/`X-Real-IP`** — it uses the direct TCP peer
  address only. Deploying behind a reverse proxy without trusted-header handling means every
  request will appear to come from the proxy's IP, defeating the per-IP heuristics.
- **The synthetic benchmark is not a security evaluation.** The naive bot model (small IP pool,
  mechanical timing, missing headers) is deliberately the "naive scraper" case that the
  velocity and header heuristics are designed to catch. The stealthy workload (browser headers,
  rotating residential IPs, human-like timing, one favicon per IP) evades all four heuristics.
  In plan 003's verification run the router's ROC-AUC fell from 1.000 on naive bots to 0.446 on
  stealthy ones (worse than chance: stealthy bots scored a mean 0.105 against 0.124 for
  humans), and nothing was flagged at 0.5. This is the expected, documented limitation, not a
  bug.
- **`lru = "0.12"` is a benchmark-tool-only dependency** (used solely by `replay_bench`'s baseline),
  not part of DTAC's own library or caching behavior.

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
| 4 | Strict bounded memory everywhere (scope §1.4.2) | Ghost cache bounded per shard at `ghost_multiplier` × main capacity, evicted by the score-aware tournament | Done |
| 5 | "Shy Guys" favicon heuristic heavily penalizes sessions (§3.2.2, §3.4.1, Alg. 1 heuristic C) | Flags an IP after `shy_page_threshold` (default 5) page requests with no auxiliary-asset fetch ever | Done |
| 6 | Cross-references datacenter ASN databases (§3.4.1, Alg. 1 heuristic B) | Offline iptoasn `ip2asn` lookup + curated datacenter-ASN list (`--asn-db`); benchmark uses a fixture that matches the generator's bot IPs by construction | Done |
| 7 | Penalties for "missing or misordered headers", `Sec-CH-UA`, User-Agent (§3.4.1, §3.5.2) | Only checks presence of `Accept-Language` | Fix text (or implement more header checks) |
| 8 | Weights are log-likelihood ratios `W_i = ln(P(E|Bot)/P(E|Human))`, "Naive Bayes" (§3.4.1, §3.4.3) | Hand-picked defaults `w_asn=1.5`, `w_fav=1.2`, `w_hdr=1.0` (configurable via `RouterConfig`); hand-weighted log-odds model. The prior is now an explicit, documented base rate (`prior_bot_rate = 0.1`, `l_0 = ln(p/(1−p))`) | Fix text (or estimate weights from data) |
| 9 | `ln` approximated with base-2 integer log / bit counting (§3.4.1 note) | Uses `f64::ln()` | Fix text (delete the note) |
| 10 | Recommended `v_τ = 15–30`, `α = 2.5–3.0` (§3.4.3) | `burst_threshold = 10.0`, `alpha = 2.0` (`RouterConfig` defaults) | Fix text or change constants |
| 11 | Derives `N = 14` as the default sample size (§3.4.3) | `N_SAMPLES = 5` | Pick one; make text and code agree |
| 12 | Eviction overhead "drops to zero" (§3.4.2) | O(N) per eviction | Fix text |
| 13 | Score appended as `X-Bot-Probability` header and forwarded to a separate cache (§3.2.3, §3.4.1) | Single process; `DtacCache::access` passes the score directly in memory | Fix text |
| 14 | Alg. 2: `fetch_from_origin` inside the shard lock; `score >> delta_t` on a float | Fetch happens outside the lock, with a re-check for concurrent promotion; decay is stepwise, `score / 2^⌊Δt / decay_half_life_ms⌋`, zeroed after `decay_cutoff_half_lives` half-lives (defaults 1 s and 32, provisional) | Fix text (the real design is better — describe it) |
| 15 | Data generator and replayer are "custom Rust scripts" (§3.3.3, §3.5) | Generator is Python (`scripts/generate_logs.py`); replayer is Rust | Fix text |
| 16 | Bots sample uniformly from a "separate subset" of the catalog (§3.5.2) | Bots sample uniformly from the **entire** catalog, overlapping with human items | Fix text |
| 17 | Bots identified via omitted favicon, missing headers, mechanical timing (§3.5.2) | True of the naive generator; the router sees velocity, `Accept-Language`, the favicon (Shy Guys) and datacenter ASN. The naive pool grows with the ratio at a fixed 3.6 req/s per IP (about 64% datacenter, the rest TEST-NET); a stealthy workload shows the heuristics failing | Fix text; state the per-IP-rate assumption and the stealthy result |
| 18 | Warm-up runs "until the cache reached absolute maximum capacity" (§3.6.1) | Fixed 50,000 pure-human requests prepended to every log | Fix text |
| 19 | Replayer fires requests at the proxy over `localhost` (§3.3.3, §3.6.1) | `replay_bench` is in-process; no network | Fix text (or add a networked run against `dtac_standalone`) |
| 20 | The 10/30/50/75% logs are dispatched "concurrently" (§3.6.1) | One separate run per ratio | Fix text |
| 21 | Every eviction is logged with whether the evicted object was human or bot (§3.6.1) | `replay_bench` counts main-cache evictions by the label of the request that triggered the victim's admission (via `DtacCacheEngine::with_eviction_listener` and the LRU baseline's `push`); ghost evictions are not counted | Done; fix text to "by the admitting request's label" |
| 22 | Byte Hit Rate (§3.7.1) | Computed per class; items have deterministic lognormal sizes (median 16 KB). Capacity counts entries and no engine is size-aware, so BHR only reflects how hits correlate with size | Done; state the caveat |
| 23 | Cache Pollution Index (§3.7.2) | Computed as the share of resident entries **no human has requested since admission**, sampled every `--cpi-sample-every` requests; the literal "share admitted by bot requests" is reported beside it as the bot-triggered share | Done; **update the §3.7.2 definition** to match (the literal one penalizes LRU whenever bots fetch popular items) |
| 24 | Proxy scoring overhead `T_proxy` and cache overhead `T_cache` measured separately (§3.7.3) | `DtacCache::access_with_stats` times both phases; `replay_bench` reports p50/p90/p99/max of each | Done |
| 25 | Memory consumption, read-path CPU overhead and "hit-density" evaluated (scope §1.4.2) | Peak RSS, RSS delta over the replay and structure sizes are reported; read-path cost is the `T_proxy`/`T_cache` latency; "hit-density" is still undefined | Done except hit-density (define or drop) |
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
- [x] Bound the ghost cache (per-shard cap, evicted by the score-aware tournament rather than FIFO so bot scans can't flush it)
- [x] Drive time-based logic from log timestamps (`replay_bench --clock log`, an injected `Clock`), with every time constant configurable
- [ ] Repeated runs with different seeds, reporting mean ± spread
- [x] Split latency: `T_proxy` (router) and `T_cache` (engine) reported separately, with p50/p90/p99/max
- [x] Memory measurement (peak RSS, RSS delta and structure sizes) for DTAC vs. LRU
- [x] Eviction logging by origin (human vs. bot admitter) and a Cache Pollution Index (share of resident entries no human has requested since admission)
- [x] Variable payload sizes in the generator, with Byte Hit Rate
- [x] Router classifier metrics: ROC-AUC, precision/recall/FPR, class-mean gap, new vs. returning visitor FPR
- [x] Stationary bot mixing, a shared human stream across ratios, multi-page human sessions, seeds and `.meta.json` sidecars, `--json-out`
- [ ] Optional: networked benchmark through `dtac_standalone` to match the thesis's "over localhost" setup
- [x] Optional: "stealthy bot" workload (browser headers, rotating IPs, human-like timing) to show where the heuristics stop working

### Status of the research objectives

From Ch. 1 §1.3.2:

| Objective | Status | Notes |
|---|---|---|
| 1. Lightweight passive heuristic detector using HTTP header anomalies and structural requests | **Achieved against naive bots; fails against stealthy ones** | Velocity, `Accept-Language`, the structural ("Shy Guys") heuristic and datacenter-ASN lookup all work. On the plan-003 session workload the router separates naive bots perfectly (ROC-AUC 1.000, recall 99.99% and FPR 0% at 0.5, `T_proxy` p99 ≈ 35 µs). Returning visitors score higher than new ones (0.17 vs 0.10) without crossing 0.5. Against stealthy bots, AUC is 0.446. Caveat: the ASN fixture matches the generator's bot IPs by construction. |
| 2. Real-time continuous AI-score in `[0, 1]` in O(1) | **Achieved except the O(1) claim** | The score is continuous and computed per request. Cost is O(log K) because of the `BTreeSet`s. True O(1) is possible with a stream-summary bucket list, but the half-life count decay makes it hard. Most practical route: reword to "bounded O(log K), independent of traffic volume". |
| 3. DTAC that modifies LRU eviction using the AI-score | **Achieved in substance, not as worded** | The AI-score does drive admission and eviction, but the design is not a modified LRU. Reword the objective rather than rebuilding to match it. |
| 4. Benchmark DTAC hit rates against standard LRU under simulated mixed workloads | **Achieved (basic version)** | Human hit rate: DTAC ≥ LRU at every ratio, +5.5 points at 75% bots. Missing: ablation, SIEVE/S3FIFO baselines, repeated runs. The 50% ratio is a tie. |

Other goals and claims from the scope and methodology:

| Goal / claim | Status |
|---|---|
| Bounded memory for client state (SpaceSaver) | Achieved |
| Bounded memory for the whole system | Achieved — the ghost cache is now bounded per shard (`ghost_multiplier` × main capacity) |
| Lock-sharded, concurrent cache | Achieved |
| Measure memory consumption and read-path CPU overhead | Measured: peak RSS, RSS delta, structure sizes, and per-phase latency percentiles |
| `T_proxy` below 2 ms | Achieved in-process: `T_proxy` p99 ≈ 35 µs in plan 003's verification run (75% naive bots) |
| Byte Hit Rate, Cache Pollution Index, eviction-by-origin logging | Implemented (see [Comparing DTAC against the LRU baseline](#comparing-dtac-against-the-lru-baseline)). BHR is size-unaware by design. The CPI definition differs from §3.7.2 (row #23) |
| Human hit rate stays stable regardless of bot volume (§3.7.1) | **Not achieved, and probably not achievable as stated.** DTAC's human hit rate still falls from 62.18% to 58.26% between 10% and 75% bots (LRU falls ~8.6 points). Reword to "degrades less than LRU". |
| CPI kept near 0% via tail insertion (§3.7.2) | **Not achievable as stated** — there is no tail insertion. A CPI could still be measured for the real design, but "near 0%" is not guaranteed. |
| Heuristic weights derived as real log-likelihood ratios | **Probably not achievable within scope.** Needs labelled real-world traffic. Estimating them from the synthetic generator would be circular, because the generator was written to trigger these exact heuristics. |
| Detecting sophisticated bots (rotating IPs, spoofed headers, human-like timing) — motivated in Ch. 1 §1.1.4 | **Not achievable with the current heuristics.** Velocity is per-IP, and the header check is trivially spoofed. State this as a limitation, not a goal. |
| "No existing literature" on bot-score-aware eviction (Ch. 2 §2.8) | Not verifiable — soften to "to the best of our knowledge". |
