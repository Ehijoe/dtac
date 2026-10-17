# DTAC — Dynamic Traffic-Aware Cache

DTAC is a Rust cache that tries to resist bot/scraper cache pollution by scoring every request
with a lightweight "AI-Score" (a bot-probability estimate) and weighting cache admission/eviction
by that score, instead of treating every request as equally worth caching the way a textbook LRU
cache does.

This README covers the architecture, the benchmarking methodology built around it, the results of
comparing DTAC against LRU, SIEVE, S3-FIFO and an ablation of itself over five seeds, and full
instructions for building, testing, and running the benchmark yourself.

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
  rate `prior_bot_rate` (default `0.02`, so `l_0 ≈ −3.89`): "assume human until the evidence says
  otherwise". A clean human request scores `bot_prob ≈ 0.02`.
- **Velocity**: per-IP request rate, tracked as a fixed-point EWMA with bitwise-shift decay. A
  graded penalty `alpha · ln(velocity / burst_threshold)` kicks in once an IP's rate crosses
  `burst_threshold`.
- **ASN**: flags datacenter/hosting-provider IPs. `AsnDatabase` (`asn_db.rs`) loads iptoasn.com
  `ip2asn` TSV ranges and binary-searches them (`O(log n)`), then checks the ASN against a curated
  list of hosting/cloud providers (`DEFAULT_DATACENTER_ASNS`, overridable). With no database
  loaded (the default), the heuristic never fires.
- **"Shy guys"**: penalizes IPs that have made `shy_page_threshold` (default 12) page requests and
  have *never* fetched an auxiliary asset (favicon, CSS, JS, images, fonts). Browsers fetch those
  once and then cache them, so "never" rather than "not since the last page" is the signal; the
  threshold is a grace period so short human visits aren't penalized.
- **Headers**: penalizes requests missing an `Accept-Language` header.

All weights and thresholds live in `RouterConfig` (`config.rs`), which deserializes from JSON so
every constant can be changed without recompiling (see `replay_bench --router-config`). The
defaults were **calibrated on a held-out seed** (see [Calibration](#calibration)).
`evaluate_request_detailed` also reports which heuristics fired, so the benchmark can report each
heuristic's firing rate per class.

Per-IP state is tracked in `SpaceSaver` (`space_saver.rs`), a bounded-memory structure (the
SpaceSaver/Stream-Summary algorithm) so the router's memory use doesn't grow unbounded with
distinct IPs.

### Cache engine (`src/cache_engine/`)

`DtacCacheEngine<K, P>` shards state across independent `Mutex`-protected shards (hashed by
resource key) to reduce lock contention. The shard count (`EngineConfig.shards`) defaults to
`auto`: the largest power of two up to 256 that leaves at least 64 entries per shard, so 256 shards
from capacity 16 384 up, 128 at 10 000 and 8 at 1 000. Tiny shards would distort the policies: a
5-sample tournament over 4 entries is an exhaustive search. Each shard runs:

- **Ghost cache**: a metadata-only (no payload) entry for resources not yet in the main cache.
  Every miss lands here first, so a one-off scan doesn't evict genuinely popular content. The
  ghost is bounded at `ghost_multiplier` (default 8) times the main-cache capacity per shard. When
  it is full, a miss first evicts a ghost entry with the same score-aware tournament as the main
  cache. Bot-inserted entries start near score 0 and lose first, so a uniform bot scan cannot push
  popular human items out of the ghost before their second hit (plain FIFO would let it).
- **Promotion**: a ghost-cache entry is promoted into the real cache (fetching the payload) once
  its EWMA popularity score crosses `promotion_threshold` (default `PROMOTION_THRESHOLD = 1.5`, so
  two clean human requests promote an item).
- **Scoring**: each hit increases a resource's score by `1.0 - request_bot_prob` — human requests
  contribute close to a full point, bot-like requests contribute close to nothing. This is the
  mechanism that actually connects the router's output to caching behavior.
- **Eviction**: a randomized tournament — sample `n_samples` (default 8) entries, evict whichever
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

**Time constants.** These are parameters, not hard-coded values: `velocity_tick_ms`,
`velocity_reset_ticks` and `space_saver_half_life_s` in `RouterConfig`, and `decay_half_life_ms`,
`decay_cutoff_half_lives`, `promotion_threshold`, `n_samples` and `ghost_multiplier` in
`EngineConfig`. The original values (halve a score every second, one velocity shift per second)
were only reasonable while `--speed 10000` squeezed a replay into a few wall-clock seconds. On log
time, scores decayed away before most items could promote, and the velocity heuristic never fired
on the benchmark's bots. The calibrated defaults are a 4 h score half-life and 5 s velocity ticks
with a burst threshold of 3 (see [Calibration](#calibration)).

### Binaries (`src/bin/`)

- **`dtac_standalone`** — an Axum reverse proxy wrapping a single shared `DtacCache`, forwarding
  misses upstream via one shared `reqwest` client, marking responses `X-Cache: HIT|MISS` and
  exporting Prometheus metrics at `/metrics`. It reads the client IP from `X-Forwarded-For` only
  when the peer is a `--trusted-proxy`.
- **`replay_bench`** — the benchmark harness used for everything described below. It replays a
  log in process against `dtac`, the `dtac-no-score` ablation, `lru`, `sieve` or `s3fifo`, or over
  HTTP against a running `dtac_standalone` (`--target-url`).
- **`origin_stub`** — a zero-byte origin server for the networked benchmark.

## Project layout

```
src/
  heuristic_router/   bot-score computation (velocity, ASN, "shy guys", header heuristics)
  cache_engine/        sharded randomized-eviction cache with ghost-cache admission
  dtac_cache.rs        combines the two + telemetry
  clock.rs             injectable time source (system clock, or manual/log-driven)
  bin/
    dtac_standalone/   reverse-proxy binary
      main.rs            Axum proxy, X-Cache header, replay clock
      client_ip.rs       trusted-proxy X-Forwarded-For handling
    origin_stub.rs     zero-byte origin for the networked benchmark
    replay_bench/      benchmark replay tool
      main.rs            CLI, log loading, dispatch loop, --json-out
      metrics.rs         counters, hit/byte-hit rates, latency histograms, memory, the report
      engines.rs         the BenchEngine trait; DTAC, the no-score ablation, sharded LRU
      sieve.rs           SIEVE baseline
      s3fifo.rs          S3-FIFO baseline
      http.rs            --target-url: replay over HTTP against dtac_standalone
      classifier.rs      the router scored as a classifier (ROC-AUC, FPR/TPR, heuristic LLRs)
      pollution.rs       eviction-by-origin counts and the Cache Pollution Index
    common/            CLI flags shared by both binaries (ASN database loading)
data/asn/
  ip2asn-fixture.tsv   small committed ip2asn fixture for tests/benchmarks (full downloads gitignored)
scripts/
  generate_logs.py     synthetic traffic generator
  run_experiments.py   calibration, the experiment grid and its aggregation
  test_generate_logs.py, test_run_experiments.py
results/
  tuning.md, tuning.json   calibration on seed 0 (committed)
  summary.md, summary.csv  aggregated results over seeds 1-5 (committed)
  raw/, tuning_raw/        one replay_bench JSON per run (gitignored)
tests/                 integration tests (cache engine, heuristic router, DtacCache builder, networked replay)
benchmark_logs/        generated NDJSON traffic logs (gitignored)
```

## Requirements

- **Rust** (stable toolchain; developed against 1.97). `rustup default stable` if you don't have
  one configured.
- **Python 3** with `numpy` (see `scripts/requirements.txt`) for the synthetic traffic generator.
  A `.venv` is the simplest path: `python -m venv .venv && .venv/bin/pip install -r scripts/requirements.txt`.

## Building & testing

```bash
cargo build                 # build the library + the binaries
cargo build --release       # release build (always use this for benchmarking)
cargo test                  # run all unit + integration tests
cargo test <name>           # run a single test (substring match), e.g. cargo test baseline_lru
cargo clippy --all-targets  # lint (this repo enables pedantic+nursery+cargo clippy groups)
(cd scripts && python -m unittest test_generate_logs test_run_experiments)   # Python tests
```

## Running the standalone proxy

```bash
cargo run --bin dtac_standalone -- --capacity 1024 --target http://127.0.0.1:8081 --port 8080
```

Starts an HTTP server on `--port` that proxies every request to `--target` through `DtacCache`,
exposing Prometheus metrics at `/metrics`. The cache key is the path; the origin is asked for the
path and query string. Every response carries `X-Cache: HIT` or `X-Cache: MISS`.

**Client IP.** By default the router sees the TCP peer address. Behind reverse proxies, list them
with `--trusted-proxy <CIDR>` (repeatable, e.g. `--trusted-proxy 10.0.0.0/8`). For a request from a
trusted peer, the client is the right-most `X-Forwarded-For` hop that isn't itself a trusted proxy,
or the left-most hop if all of them are. Hops to the left of that one were written by the client
and can't be trusted. From an untrusted peer, or with a missing or malformed header, the client is
the peer. A client therefore can't spoof its IP past the allowlist.

To enable the datacenter-ASN heuristic, pass one or more ip2asn files (see
[ASN data](#asn-data)):

```bash
cargo run --bin dtac_standalone -- --asn-db data/asn/ip2asn-v4.tsv --asn-db data/asn/ip2asn-v6.tsv
```

| Flag | Meaning |
|---|---|
| `-c, --capacity <N>` | Cache capacity in entries (default 1024) |
| `-t, --target <URL>` | Origin to forward misses to (default `http://127.0.0.1:8081`) |
| `-p, --port <PORT>` | Port (default 8080); `0` picks a free one. The bound address is printed as `Listening on <addr>` |
| `--bind <ADDR>` | Address to listen on (default `0.0.0.0`) |
| `--trusted-proxy <CIDR>` | A reverse proxy whose `X-Forwarded-For` is believed. Repeatable |
| `--asn-db <PATH>` | iptoasn `ip2asn` TSV (v4, v6 or combined). Repeatable; files are merged. Without it, the ASN heuristic never fires |
| `--datacenter-asns <PATH>` | One ASN per line (`AS` prefix and `#` comments allowed), replacing the built-in datacenter list. Requires `--asn-db` |
| `--clock <system\|replay-header>` | Benchmarking only. `replay-header`: the cache's time is the latest `X-Replay-Time-Ms` from a trusted proxy, so a replay runs on log time. Needs `--trusted-proxy` |
| `--debug-headers` | Benchmarking only: add `X-Bot-Probability: <score>` to responses |

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
    it. Returning visitors who view `shy_page_threshold` (12 since calibration) or more pages
    are the realistic "Shy Guys" false positives. Records carry `"visitor": "new" | "returning"`.
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

The pre-plan-004 results came from that legacy model. The old script took no seed, so those
exact files can't be regenerated, but `--legacy` reproduces the model. The current
[Benchmark results](#benchmark-results) use the default model.

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
| `-c, --capacity <N>` | `10000` | Total cache capacity in entries, divided evenly across the shards |
| `--shards <N\|auto>` | `auto` | Lock shards, for every algorithm: `auto` (largest power of two ≤ 256 leaving ≥ 64 entries per shard) or a count. Overrides `shards` from `--engine-config`. The resolved count is printed and written to `--json-out` |
| `-j, --concurrency <N>` | `128` | Max in-flight requests at once |
| `-s, --payload-size <BYTES>` | `1024` | Payload size returned on a miss for records without `size_bytes` (older logs). Records with `size_bytes` get exactly that size, as a slice of one shared zeroed buffer |
| `--speed <MULTIPLIER>` | `10000.0` | Playback speed relative to logged timestamps. `1.0` = realtime; higher replays faster. With `--clock log` it only changes the run time; with `--clock wall` it also compresses the timeline that decay and velocity see |
| `--clock <log\|wall>` | `log` | Time source for decay and velocity. `log`: a simulated clock set to each record's timestamp before its access. Because the clock never moves backwards, a request can see a time a few ms ahead of its own when a later record's task set it first (up to `--concurrency` tasks are in flight); with half-lives of seconds this is negligible. `wall`: the real clock, reproducing pre-plan-002 behaviour |
| `--engine-config <PATH>` | *(defaults)* | JSON object of `EngineConfig` fields (`decay_half_life_ms`, `decay_cutoff_half_lives`, `promotion_threshold`, `n_samples`, `ghost_multiplier`, `shards`); omitted fields keep their defaults, unknown fields are rejected |
| `--ghost-multiplier <N>` | `8` | Ghost-cache capacity as a multiple of main-cache capacity; overrides the value from `--engine-config` |
| `-a, --algorithm <A>` | `dtac` | The engine to drive. `dtac`: router + DTAC engine. `dtac-no-score`: the ablation, DTAC's engine with every request scored `sigmoid(l_0)` (a clean human request under the router config), so bots can't be told apart. `lru`: bot-unaware sharded LRU. `sieve`: SIEVE (NSDI '24). `s3fifo`: S3-FIFO (SOSP '23). All share the shard count and capacity split, so only the policy differs |
| `--target-url <URL>` | *(none)* | Replay over HTTP against a running `dtac_standalone` instead (see [Networked benchmark](#networked-benchmark)) |
| `--asn-db <PATH>` | *(none)* | iptoasn `ip2asn` TSV for the datacenter-ASN heuristic. Repeatable; files are merged. E.g. `--asn-db data/asn/ip2asn-fixture.tsv` |
| `--datacenter-asns <PATH>` | *(built-in list)* | One ASN per line, replacing the default datacenter list. Requires `--asn-db` |
| `--router-config <PATH>` | *(defaults)* | JSON object of `RouterConfig` fields (`prior_bot_rate`, `w_asn`, `w_fav`, `w_hdr`, `burst_threshold`, `alpha`, `shy_page_threshold`, `velocity_tick_ms`, `velocity_reset_ticks`, `space_saver_half_life_s`); omitted fields keep their defaults, unknown fields are rejected |
| `--prior-bot-rate <P>` | `0.02` | Router prior bot rate in `(0, 1)`; overrides the value from `--router-config`. `0.5` reproduces the original zero prior |
| `--cpi-sample-every <N>` | `10000` | Sample the Cache Pollution Index every N measurement requests, and once more at the end |
| `--json-out <PATH>` | *(none)* | Write every reported metric plus the run configuration (file, algorithm, capacity, clock, effective router/engine configs, the log's `.meta.json`, git commit) as JSON |
| `-q, --quiet` | off | Hide the progress bar and the Prometheus dump, for scripted runs |

The clock mode and the effective router and engine configurations are printed at startup. Setting
`RUST_LOG` (e.g. `RUST_LOG=dtac=info`) prints per-request router decisions to stderr; leave it
unset for throughput measurements.

#### Comparing algorithms on one log

Run the same file through each algorithm and compare the printed reports:

```bash
cargo run --release --bin replay_bench -- --file benchmark_logs/traffic_mixed_50pct_bot.ndjson --capacity 10000 --algorithm dtac
cargo run --release --bin replay_bench -- --file benchmark_logs/traffic_mixed_50pct_bot.ndjson --capacity 10000 --algorithm sieve
```

For anything you'd report, use the experiment harness below instead: one run per algorithm is
one sample, and the differences between algorithms are often smaller than the spread between
seeds.

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
  - Per heuristic: how often it fires on humans and on bots, and the empirical log-likelihood
    ratio `ln(P(fired|bot)/P(fired|human))` (with +0.5 smoothing).
  - `--json-out` adds per-label stats and a 20-bin `bot_prob` histogram per class.

**Byte Hit Rate caveat**: capacity counts **entries, not bytes**, and no engine (DTAC included)
takes object size into account when admitting or evicting. BHR here reflects how hits on popular
items happen to correlate with size. It doesn't measure a size-aware policy.

### 3. Run the experiment grid

`scripts/run_experiments.py` runs calibration, the full grid of replays and their aggregation.
It only needs the standard library (the generator it calls needs numpy):

```bash
.venv/bin/python scripts/run_experiments.py tune      # calibration on seed 0 -> results/tuning.md
.venv/bin/python scripts/run_experiments.py --quick   # smoke test: seed 1, 30%/75%, capacity 10 000 (20 runs)
.venv/bin/python scripts/run_experiments.py           # the thesis grid (350 runs, about 25 min)
.venv/bin/python scripts/run_experiments.py aggregate # re-aggregate results/raw/ only
```

- **Grid.** Seeds (`--seeds`, default 1–5) × workloads (`--workloads naive stealthy`) × ratios
  (`--ratios`, 10/30/50/75%) × algorithms (`--algorithms`, all five) × capacities
  (`--capacities`, 1 000 / 5 000 / 10 000 / 20 000, i.e. 1–20% of the catalog). The default
  `--profile thesis` sweeps capacity only for the naive workload at 30% and 75% and runs
  everything else at `--primary-capacity` 10 000: 350 runs. `--profile full` runs all 800.
  `--dry-run` lists the cells.
- **Steps.** It builds `replay_bench --release` once, generates each seed's logs into
  `benchmark_logs/seed_<S>/` if they are missing, and runs each cell
  (`replay_bench --quiet --asn-db data/asn/ip2asn-fixture.tsv --json-out
  results/raw/<seed>_<workload>_<pct>_<algorithm>_<capacity>.json`) one at a time, so runs don't
  compete for CPU. The first run is timed to print an estimate. Cells whose JSON already exists
  are skipped, so an interrupted sweep resumes.
- **Safety.** Seed 0 is the calibration seed: evaluation seeds are refused until
  `results/tuning.md` exists, and seed 0 is never aggregated. Every cell records its git commit
  (with `-dirty` for uncommitted changes) and both configs, and the aggregation refuses to mix
  commits or configs unless `--allow-mixed`.
- **Outputs.**
  - `results/summary.csv`: mean, std and 95% CI (t-distribution, n = seeds) per (workload, ratio,
    capacity, algorithm) for human/overall/bot hit rate, human/overall BHR, CPI, `T_proxy` p99,
    RSS delta, logical resident bytes and entry counts. It also holds the paired differences
    and the router quality rows.
  - **Paired differences**: every algorithm sees the same human stream for a given seed, so
    `metric(dtac) − metric(X)` is computed per seed and its mean and 95% CI reported. This is
    much tighter than comparing two overlapping CIs, and it is the right test for "does DTAC
    beat X". A difference whose CI includes 0 is reported as a tie.
  - `results/summary.md`: the same as markdown tables, which the results below are copied from.

### Networked benchmark

To match the thesis's over-localhost setup, the same log can be replayed through the real proxy:

```bash
cargo build --release
target/release/origin_stub --port 8081 &
RUST_LOG=warn target/release/dtac_standalone --port 8080 --capacity 10000 \
    --target http://127.0.0.1:8081 --trusted-proxy 127.0.0.1/32 \
    --clock replay-header --debug-headers --asn-db data/asn/ip2asn-fixture.tsv &
target/release/replay_bench --file benchmark_logs/seed_1/traffic_mixed_30pct_bot.ndjson \
    --target-url http://127.0.0.1:8080 --json-out /tmp/networked.json
```

Each record becomes a `GET <path>?size=<size_bytes>` with the record's headers,
`X-Forwarded-For: <record ip>` and `X-Replay-Time-Ms: <record timestamp>`. The proxy believes
both because the bench's address is a trusted proxy. With `--clock replay-header` it runs on the
log's timeline exactly as `--clock log` does in process. `origin_stub` answers with that many
zero bytes. Hits come from the proxy's `X-Cache` header and scores from `X-Bot-Probability`, so
the hit rates, BHR, latency and classifier sections are all reported. Residency isn't visible
over HTTP, so there is no CPI, and any failed request fails the run.
`tests/networked_tests.rs` checks that a 20-record networked replay gives exactly the in-process
hit and miss counts.

## Benchmark results

**Setup.** Every number below is the mean over **five seeds (1–5)** with its 95% confidence
interval (t-distribution, n = 5), from `results/summary.md`. That file was generated by
`scripts/run_experiments.py` from 350 runs at commit `05a67b9`, with the calibrated defaults (see
[Calibration](#calibration)), `--clock log`, the ASN fixture and the default thesis profile.
"Paired" columns are `dtac − X` computed **per seed** and then averaged, with the CI of that
difference. **Bold** means the CI excludes 0; "tie" means it includes 0. Each seed's human stream
is identical across algorithms and ratios (plan 003), which is what makes the pairing valid.
Hit rates are in %, differences in percentage points.

### Calibration

The router and engine constants were calibrated on **seed 0**, which no reported result uses,
on the naive workload at 30% and 75% bots and capacity 10 000. The router was tuned first
(maximise the class-mean gap with returning-visitor FPR ≤ 5%), then the engine (maximise human
hit rate). See [`results/tuning.md`](results/tuning.md) for every sweep table and the reasoning
behind each value.

| Constant | Old default | Calibrated |
|---|---|---|
| `prior_bot_rate` | 0.1 | 0.02 |
| `w_hdr` / `w_fav` / `w_asn` | 1.0 / 1.2 / 1.5 | 4 / 2 / 3 |
| `velocity_tick_ms` / `burst_threshold` | 1000 / 10 | 5000 / 3 |
| `shy_page_threshold` | 5 | 12 |
| `decay_half_life_ms` | 1 s | 4 h |
| `promotion_threshold` | 2.0 | 1.5 |
| `n_samples` | 5 | 8 |
| `ghost_multiplier` | 4 | 8 |

Caveats, all spelled out in `tuning.md`:

- Several values sit at the edge of their grid. The objective saturates on a generator built to
  trip these heuristics.
- The 4 h half-life makes DTAC close to LFU, which suits this generator's **stationary** Zipf
  popularity but wouldn't suit a drifting one.
- The constants were not re-tuned per capacity. The check at 20 000 already flagged that a lower
  promotion threshold would do about 2 points better there.
- The empirical log-likelihood ratios of the heuristics are reported next to the weights, but
  they are **circular**: the generator was written to trigger these heuristics. They don't
  validate the weights as `ln(P(E|bot)/P(E|human))`.

### Naive workload, capacity 10 000

Human hit rate (primary metric):

| Bot ratio | dtac | dtac-no-score | lru | sieve | s3fifo |
|---|---|---|---|---|---|
| 10% | 58.29 ± 0.13 | 57.95 ± 0.13 | 52.08 ± 0.16 | 57.09 ± 0.09 | 57.32 ± 0.21 |
| 30% | 58.27 ± 0.12 | 57.12 ± 0.19 | 49.37 ± 0.26 | 56.30 ± 0.08 | 56.53 ± 0.18 |
| 50% | 58.25 ± 0.14 | 55.92 ± 0.13 | 46.35 ± 0.19 | 55.41 ± 0.17 | 55.50 ± 0.13 |
| 75% | 58.22 ± 0.09 | 53.89 ± 0.19 | 41.12 ± 0.26 | 54.32 ± 0.16 | 53.09 ± 0.15 |

Paired difference in human hit rate, `dtac − X`:

| Bot ratio | vs dtac-no-score | vs lru | vs sieve | vs s3fifo |
|---|---|---|---|---|
| 10% | **+0.33 [+0.29, +0.37]** | **+6.21 [+6.11, +6.30]** | **+1.19 [+1.08, +1.31]** | **+0.97 [+0.82, +1.11]** |
| 30% | **+1.15 [+1.03, +1.28]** | **+8.90 [+8.75, +9.05]** | **+1.97 [+1.90, +2.03]** | **+1.74 [+1.66, +1.83]** |
| 50% | **+2.33 [+2.25, +2.41]** | **+11.90 [+11.78, +12.03]** | **+2.84 [+2.69, +2.99]** | **+2.75 [+2.70, +2.80]** |
| 75% | **+4.33 [+4.12, +4.55]** | **+17.10 [+16.88, +17.33]** | **+3.90 [+3.74, +4.05]** | **+5.13 [+4.99, +5.28]** |

Overall hit rate (secondary):

| Bot ratio | dtac | dtac-no-score | lru | sieve | s3fifo |
|---|---|---|---|---|---|
| 10% | 53.31 ± 0.16 | 53.03 ± 0.14 | 47.84 ± 0.14 | 52.31 ± 0.11 | 52.52 ± 0.20 |
| 30% | 43.49 ± 0.12 | 42.76 ± 0.17 | 37.48 ± 0.24 | 42.34 ± 0.10 | 42.48 ± 0.17 |
| 50% | 33.71 ± 0.08 | 32.74 ± 0.11 | 28.10 ± 0.09 | 32.62 ± 0.09 | 32.65 ± 0.08 |
| 75% | 21.47 ± 0.07 | 20.81 ± 0.04 | 17.73 ± 0.07 | 21.00 ± 0.04 | 20.71 ± 0.06 |

Human byte hit rate (secondary):

| Bot ratio | dtac | dtac-no-score | lru | sieve | s3fifo |
|---|---|---|---|---|---|
| 10% | 53.11 ± 0.25 | 52.67 ± 0.25 | 46.04 ± 0.33 | 51.73 ± 0.29 | 51.95 ± 0.44 |
| 30% | 53.08 ± 0.23 | 51.68 ± 0.47 | 42.96 ± 0.29 | 50.78 ± 0.28 | 50.99 ± 0.41 |
| 50% | 53.05 ± 0.23 | 50.51 ± 0.29 | 39.43 ± 0.31 | 49.77 ± 0.38 | 49.88 ± 0.32 |
| 75% | 53.07 ± 0.17 | 48.12 ± 0.69 | 33.54 ± 0.42 | 48.56 ± 0.53 | 47.20 ± 0.28 |

Bot hit rate (reference only, not a target):

| Bot ratio | dtac | dtac-no-score | lru | sieve | s3fifo |
|---|---|---|---|---|---|
| 10% | 9.12 ± 0.49 | 9.27 ± 0.50 | 10.12 ± 0.42 | 9.86 ± 0.46 | 9.83 ± 0.42 |
| 30% | 9.20 ± 0.21 | 9.47 ± 0.19 | 9.93 ± 0.26 | 9.95 ± 0.20 | 9.92 ± 0.19 |
| 50% | 9.29 ± 0.10 | 9.69 ± 0.13 | 9.95 ± 0.09 | 9.95 ± 0.10 | 9.92 ± 0.11 |
| 75% | 9.28 ± 0.08 | 9.83 ± 0.08 | 9.97 ± 0.08 | 9.95 ± 0.04 | 9.97 ± 0.05 |

### Stealthy workload, capacity 10 000

Human hit rate (primary metric):

| Bot ratio | dtac | dtac-no-score | lru | sieve | s3fifo |
|---|---|---|---|---|---|
| 10% | 57.96 ± 0.15 | 58.04 ± 0.16 | 52.34 ± 0.21 | 57.21 ± 0.14 | 57.40 ± 0.21 |
| 30% | 57.34 ± 0.09 | 57.31 ± 0.16 | 50.03 ± 0.22 | 56.52 ± 0.06 | 56.70 ± 0.14 |
| 50% | 56.34 ± 0.10 | 56.32 ± 0.13 | 47.27 ± 0.23 | 55.71 ± 0.10 | 55.82 ± 0.17 |
| 75% | 54.40 ± 0.11 | 54.36 ± 0.09 | 42.29 ± 0.27 | 54.53 ± 0.22 | 53.81 ± 0.12 |

Paired difference in human hit rate, `dtac − X`:

| Bot ratio | vs dtac-no-score | vs lru | vs sieve | vs s3fifo |
|---|---|---|---|---|
| 10% | **-0.08 [-0.13, -0.03]** | **+5.62 [+5.51, +5.73]** | **+0.75 [+0.61, +0.89]** | **+0.56 [+0.48, +0.65]** |
| 30% | +0.03 [-0.06, +0.12] (tie) | **+7.31 [+7.17, +7.46]** | **+0.82 [+0.77, +0.87]** | **+0.64 [+0.54, +0.73]** |
| 50% | +0.02 [-0.03, +0.07] (tie) | **+9.07 [+8.91, +9.23]** | **+0.64 [+0.50, +0.77]** | **+0.52 [+0.37, +0.67]** |
| 75% | +0.04 [-0.07, +0.15] (tie) | **+12.11 [+11.88, +12.35]** | -0.13 [-0.31, +0.05] (tie) | **+0.59 [+0.41, +0.77]** |

Overall hit rate (secondary):

| Bot ratio | dtac | dtac-no-score | lru | sieve | s3fifo |
|---|---|---|---|---|---|
| 10% | 54.93 ± 0.13 | 55.01 ± 0.15 | 49.92 ± 0.19 | 54.31 ± 0.13 | 54.47 ± 0.19 |
| 30% | 48.48 ± 0.07 | 48.46 ± 0.08 | 43.48 ± 0.15 | 48.01 ± 0.06 | 48.12 ± 0.03 |
| 50% | 42.05 ± 0.05 | 42.04 ± 0.08 | 37.63 ± 0.13 | 41.86 ± 0.04 | 41.94 ± 0.08 |
| 75% | 34.56 ± 0.02 | 34.56 ± 0.03 | 31.62 ± 0.10 | 34.71 ± 0.07 | 34.51 ± 0.03 |

### Capacity sweep (naive workload)

Human hit rate against capacity, which covers 1–20% of the 100 000-item catalog. Shard counts
come from `--shards auto`.

30% bots:

| Capacity | Shards | dtac | dtac-no-score | lru | sieve | s3fifo | dtac − dtac-no-score | dtac − lru | dtac − sieve | dtac − s3fifo |
|---|---|---|---|---|---|---|---|---|---|---|
| 1,000 | 8 ± 0 | 39.41 ± 0.18 | 39.17 ± 0.24 | 27.61 ± 0.12 | 38.27 ± 0.38 | 39.48 ± 0.22 | **+0.24 [+0.10, +0.38]** | **+11.80 [+11.72, +11.88]** | **+1.14 [+0.89, +1.39]** | **-0.07 [-0.12, -0.01]** |
| 5,000 | 64 ± 0 | 51.95 ± 0.16 | 50.91 ± 0.13 | 40.92 ± 0.22 | 49.89 ± 0.19 | 50.74 ± 0.14 | **+1.04 [+0.98, +1.10]** | **+11.03 [+10.95, +11.11]** | **+2.06 [+1.94, +2.18]** | **+1.21 [+1.15, +1.27]** |
| 10,000 | 128 ± 0 | 58.27 ± 0.12 | 57.12 ± 0.19 | 49.37 ± 0.26 | 56.30 ± 0.08 | 56.53 ± 0.18 | **+1.15 [+1.03, +1.28]** | **+8.90 [+8.75, +9.05]** | **+1.97 [+1.90, +2.03]** | **+1.74 [+1.66, +1.83]** |
| 20,000 | 256 ± 0 | 61.50 ± 0.20 | 62.56 ± 0.19 | 60.50 ± 0.24 | 64.48 ± 0.24 | 63.74 ± 0.14 | **-1.07 [-1.18, -0.95]** | **+1.00 [+0.77, +1.23]** | **-2.99 [-3.24, -2.73]** | **-2.24 [-2.46, -2.01]** |

75% bots:

| Capacity | Shards | dtac | dtac-no-score | lru | sieve | s3fifo | dtac − dtac-no-score | dtac − lru | dtac − sieve | dtac − s3fifo |
|---|---|---|---|---|---|---|---|---|---|---|
| 1,000 | 8 ± 0 | 39.38 ± 0.18 | 37.62 ± 0.22 | 22.07 ± 0.09 | 37.71 ± 0.28 | 37.75 ± 0.33 | **+1.76 [+1.65, +1.87]** | **+17.32 [+17.17, +17.47]** | **+1.67 [+1.45, +1.89]** | **+1.64 [+1.45, +1.82]** |
| 5,000 | 64 ± 0 | 51.85 ± 0.15 | 48.43 ± 0.17 | 33.26 ± 0.19 | 48.36 ± 0.17 | 47.61 ± 0.14 | **+3.42 [+3.23, +3.61]** | **+18.59 [+18.48, +18.71]** | **+3.49 [+3.32, +3.65]** | **+4.24 [+4.03, +4.44]** |
| 10,000 | 128 ± 0 | 58.22 ± 0.09 | 53.89 ± 0.19 | 41.12 ± 0.26 | 54.32 ± 0.16 | 53.09 ± 0.15 | **+4.33 [+4.12, +4.55]** | **+17.10 [+16.88, +17.33]** | **+3.90 [+3.74, +4.05]** | **+5.13 [+4.99, +5.28]** |
| 20,000 | 256 ± 0 | 61.49 ± 0.22 | 61.01 ± 0.14 | 52.21 ± 0.22 | 62.10 ± 0.11 | 60.42 ± 0.13 | **+0.49 [+0.27, +0.71]** | **+9.28 [+8.93, +9.64]** | **-0.61 [-0.84, -0.38]** | **+1.08 [+0.85, +1.30]** |

### Router quality

From the `dtac` runs (the router doesn't depend on the algorithm or the capacity):

| Workload | Bot ratio | ROC-AUC | Class-mean gap | Human FPR new % | Human FPR returning % | Bot recall % |
|---|---|---|---|---|---|---|
| naive | 10% | 1.000 ± 0.000 | 0.976 ± 0.000 | 0.00 ± 0.00 | 0.00 ± 0.00 | 100.00 ± 0.00 |
| naive | 30% | 1.000 ± 0.000 | 0.975 ± 0.000 | 0.00 ± 0.00 | 0.00 ± 0.00 | 100.00 ± 0.00 |
| naive | 50% | 1.000 ± 0.000 | 0.976 ± 0.000 | 0.00 ± 0.00 | 0.00 ± 0.00 | 100.00 ± 0.00 |
| naive | 75% | 1.000 ± 0.000 | 0.976 ± 0.000 | 0.00 ± 0.00 | 0.00 ± 0.00 | 100.00 ± 0.00 |
| stealthy | 10% | 0.485 ± 0.002 | -0.003 ± 0.000 | 0.00 ± 0.00 | 0.00 ± 0.00 | 0.00 ± 0.00 |
| stealthy | 30% | 0.485 ± 0.001 | -0.003 ± 0.000 | 0.00 ± 0.00 | 0.00 ± 0.00 | 0.00 ± 0.00 |
| stealthy | 50% | 0.485 ± 0.001 | -0.003 ± 0.000 | 0.00 ± 0.00 | 0.00 ± 0.00 | 0.00 ± 0.00 |
| stealthy | 75% | 0.485 ± 0.001 | -0.003 ± 0.000 | 0.00 ± 0.00 | 0.00 ± 0.00 | 0.00 ± 0.00 |

### Pollution and metadata overhead

At 75% bots, capacity 10 000. CPI is the share of resident entries no human has requested since
admission. Every algorithm holds the same 9 984 main entries (capacity split across 128 shards),
so the comparison is at **equal entry capacity**. DTAC and S3-FIFO also keep metadata-only ghost
entries on top of that, and DTAC's ghost is the largest.

Naive:

| Bot ratio | Algorithm | CPI % | T_proxy p99 µs | RSS delta MiB | Logical resident MiB | Main entries | Ghost entries |
|---|---|---|---|---|---|---|---|
| 75% | dtac | 0.0 ± 0.0 | 22.8 ± 6.4 | 32.5 ± 0.1 | 258.0 ± 2.6 | 9984 ± 0 | 79530 ± 24 |
| 75% | dtac-no-score | 47.6 ± 0.3 | — | 20.4 ± 0.4 | 256.0 ± 5.2 | 9984 ± 0 | 44883 ± 262 |
| 75% | lru | 78.9 ± 0.1 | — | 4.8 ± 0.5 | 257.8 ± 4.5 | 9984 ± 0 | — |
| 75% | sieve | 37.4 ± 0.5 | — | 4.9 ± 0.1 | 256.6 ± 2.7 | 9984 ± 0 | — |
| 75% | s3fifo | 55.3 ± 0.6 | — | 9.1 ± 0.2 | 256.7 ± 4.4 | 9984 ± 0 | 8928 ± 9 |

Stealthy:

| Bot ratio | Algorithm | CPI % | T_proxy p99 µs | RSS delta MiB | Logical resident MiB | Main entries | Ghost entries |
|---|---|---|---|---|---|---|---|
| 75% | dtac | 43.9 ± 0.5 | 35.9 ± 10.5 | 28.9 ± 0.1 | 256.0 ± 3.6 | 9984 ± 0 | 44926 ± 149 |
| 75% | dtac-no-score | 44.1 ± 0.6 | — | 19.7 ± 0.2 | 256.5 ± 2.3 | 9984 ± 0 | 44861 ± 95 |
| 75% | lru | 74.9 ± 0.1 | — | 4.6 ± 0.3 | 257.0 ± 2.7 | 9984 ± 0 | — |
| 75% | sieve | 35.7 ± 0.6 | — | 5.0 ± 0.1 | 258.0 ± 4.3 | 9984 ± 0 | — |
| 75% | s3fifo | 50.9 ± 0.6 | — | 9.1 ± 0.1 | 258.0 ± 2.2 | 9984 ± 0 | 8932 ± 6 |

`T_proxy` is the router's time per request (DTAC only). The RSS delta is cache metadata plus the
bench's own bookkeeping, and it is the same bookkeeping for every algorithm, so the differences
between rows are the engines'. DTAC's extra ~28 MiB over LRU at 75% naive is its ghost cache
(about 80 000 entries at `ghost_multiplier` 8) and the router's `SpaceSaver`.

### Networked run

`origin_stub` + `dtac_standalone --trusted-proxy 127.0.0.1/32 --clock replay-header` +
`replay_bench --target-url` on seed 1's 30% naive log (capacity 10 000) gave a human hit rate of
**58.29%** against 58.32% in process for the same seed. Both fall within the five-seed CI of
58.27 ± 0.12. The overall hit rate was 43.44% against 43.46%, and the router's class-mean gap
0.975 in both. Over HTTP on localhost the total access time was p50 2.8 ms and p99 9.7 ms at about
33 000 req/s, against p99 0.2 ms in process. That cost is the network and HTTP stack, not the
cache.

### Interpretation

- **The AI-score is what makes DTAC beat the baselines on naive bots, and its effect grows with
  the bot share.** Against its own ablation (same engine, every request scored as a clean human),
  DTAC gains +0.33 points at 10% bots, +1.15 at 30%, +2.33 at 50% and +4.33 at 75%, with every CI
  excluding 0. Without the score, the engine's human hit rate drops from 57.95% to 53.89% as bots
  go from 10% to 75%. With it, the rate stays at **58.2–58.3% at every ratio**: on this workload,
  bots barely touch DTAC's cache residency at all.
- **The ablation alone is already competitive with SIEVE and S3-FIFO.** Ghost admission plus the
  long-memory decayed-score tournament roughly matches the scan-resistant baselines. It is ahead
  of both at 10–50% and between them at 75%. So DTAC's margin over SIEVE (+1.2 to +3.9) and
  S3-FIFO (+1.0 to +5.1) at capacity 10 000 is mostly the AI-score's contribution.
- **On the stealthy workload the score contributes nothing, as expected.** The router's AUC is
  0.485 (no better than chance), and `dtac` and `dtac-no-score` tie at 30–75%. At 10% the ablation
  is very slightly ahead (−0.08). DTAC still beats LRU everywhere and S3-FIFO by about 0.5–0.6
  points, and ties SIEVE at 75%. That small edge is the ghost/LFU engine, not bot detection. The
  ablation-vs-DTAC gap is +4.33 on naive at 75% and +0.04 on stealthy, which is the check that
  the score, not something else, is what helps on naive.
- **DTAC's advantage depends on capacity.** At 1 000–10 000 entries it beats every baseline on
  naive traffic (except S3-FIFO at 1 000 / 30%, −0.07, essentially a tie). At **20 000 it loses**
  to SIEVE (−3.0 at 30%, −0.6 at 75%) and to S3-FIFO and its own ablation at 30%. It still beats
  LRU there, by +1.0 and +9.3. Two causes are visible in the data:
  - DTAC was calibrated at 10 000, and the calibration's own capacity check showed promotion
    threshold 1.5 is about 2 points short of the best there.
  - With 20% of the catalog cached, the scan-resistant baselines lose little to bots anyway, so
    DTAC's requirement of two human requests before admitting an item costs more than it saves.
- **No shard-size artefact.** No algorithm shows a cliff at 1 000 that disappears at 5 000. The
  1 000-entry runs use 8 shards of 125 entries.
- **SIEVE and S3-FIFO beat LRU, as the literature predicts**, by 12–13 points at 75% naive bots.
  So does DTAC, by 17.1 points.
- **Overall hit rate** follows the human hit rate. DTAC leads on it too on the naive workload,
  even though it serves bots slightly less (9.3% vs ≈ 10% bot hit rate), because humans dominate
  the hits.

## Known limitations

- **The "Shy guys" heuristic is trivially evaded**: it works per IP, so a bot that fetches a
  single favicon (or any asset) once per IP is never flagged again. The stealthy workload does
  exactly this.
- **Shy Guys false positives are real but mild**: returning visitors never fetch the favicon,
  so the heuristic flags them once they have viewed `shy_page_threshold` (12) pages. Over seeds
  1–5 their mean `bot_prob` was 0.030 against 0.021 for new visitors. The penalty alone stays
  below 0.5, so the human FPR at 0.5 was 0% for both. The calibrated velocity constants also fire
  on about 4% of human requests (fast multi-page sessions), again without crossing 0.5.
- **The datacenter ASN list is curated, not complete** — `DEFAULT_DATACENTER_ASNS` covers major
  cloud/hosting providers only; bots on residential proxies or unlisted hosts are not caught. The
  benchmark's ASN signal is true by construction (see [ASN data](#asn-data)).
- **NAT / shared IPs** — all per-IP heuristics (velocity, "Shy guys") treat every client behind one
  address as a single client, so a busy NAT can look like a bot and a bot can hide among humans.
- **The synthetic benchmark is not a security evaluation.** The naive bot model (small IP pool,
  mechanical timing, missing headers) is deliberately the "naive scraper" case that the
  velocity and header heuristics are designed to catch. The stealthy workload (browser headers,
  rotating residential IPs, human-like timing, one favicon per IP) evades all four heuristics.
  Over seeds 1–5 the router's ROC-AUC is 1.000 on naive bots and 0.485 on stealthy ones
  (slightly worse than chance: stealthy bots score a mean 0.021 against 0.023 for humans), and
  nothing is flagged at 0.5. On that workload DTAC ties its own no-score ablation. This is the
  expected, documented limitation, not a bug.
- **The calibrated constants fit the synthetic generator.** They were chosen on a held-out seed
  of the same generator, which prevents overfitting to the evaluation seeds but not to the
  generator itself. In particular the 4 h score half-life relies on stationary popularity, and
  the header and ASN weights rely on the generator's clean separation. The constants were tuned
  at capacity 10 000 only, and at 20 000 DTAC loses to SIEVE and S3-FIFO.
- **`lru = "0.12"` is a benchmark-tool-only dependency** (used solely by `replay_bench`'s baseline),
  not part of DTAC's own library or caching behavior.

## Thesis vs. implementation

This section compares the written thesis (Ch. 1 Introduction, Ch. 2 Literature Review, Ch. 3
Methodology) against the code in this repo, as of plan 004 (commit `05a67b9` for the results).

### Differences between the thesis and the code

"Fix text" means the code is reasonable and the thesis should be reworded; "Implement" means the
thesis describes something worth building.

| # | Thesis says (section) | Code actually does | Action |
|---|---|---|---|
| 1 | DTAC is a "modified LRU" / "AI-Aware LRU" with a doubly linked list, MRU head vs. LRU tail insertion, bots "routed to the eviction tail" (Ch1 obj. 3, §3.1, §3.2.2, §3.3.2, §3.6.2, §3.7.2, §3.7.3) | No linked list, no head/tail. Ghost-cache admission + per-resource decayed score + random-sample eviction tournament (§3.4.2 describes this correctly) | Fix text |
| 2 | Scoring runs in O(1) (Ch1 obj. 2, scope, §2.4, §2.8, Alg. 1 line 5, §3.7.3) | `SpaceSaver` uses two `BTreeSet`s → O(log K) per request with fixed K | Fix text (or implement O(1) stream-summary buckets) |
| 3 | SpaceSaver uses "O(1) space" (§3.4.1) | O(K) space, bounded by `capacity` | Fix text |
| 4 | Strict bounded memory everywhere (scope §1.4.2) | Ghost cache bounded per shard at `ghost_multiplier` × main capacity, evicted by the score-aware tournament | Done |
| 5 | "Shy Guys" favicon heuristic heavily penalizes sessions (§3.2.2, §3.4.1, Alg. 1 heuristic C) | Flags an IP after `shy_page_threshold` page requests with no auxiliary-asset fetch ever (default 12, calibrated) | Done |
| 6 | Cross-references datacenter ASN databases (§3.4.1, Alg. 1 heuristic B) | Offline iptoasn `ip2asn` lookup + curated datacenter-ASN list (`--asn-db`); benchmark uses a fixture that matches the generator's bot IPs by construction | Done |
| 7 | Penalties for "missing or misordered headers", `Sec-CH-UA`, User-Agent (§3.4.1, §3.5.2) | Only checks presence of `Accept-Language` | Fix text (or implement more header checks) |
| 8 | Weights are log-likelihood ratios `W_i = ln(P(E|Bot)/P(E|Human))`, "Naive Bayes" (§3.4.1, §3.4.3) | Weights calibrated on a held-out seed for class separation under an FPR constraint (`w_asn=3`, `w_fav=2`, `w_hdr=4`, `prior_bot_rate=0.02`; `results/tuning.md`). The empirical LLRs are reported beside them but are circular (the generator was built to trigger the heuristics) and much larger (up to +12) | Fix text: describe calibrated weights, not estimated LLRs |
| 9 | `ln` approximated with base-2 integer log / bit counting (§3.4.1 note) | Uses `f64::ln()` | Fix text (delete the note) |
| 10 | Recommended `v_τ = 15–30`, `α = 2.5–3.0` (§3.4.3) | Calibrated `burst_threshold = 3` with 5 s velocity ticks; `alpha = 2.0` (not swept) | Fix text to the calibrated values |
| 11 | Derives `N = 14` as the default sample size (§3.4.3) | Calibrated `N_SAMPLES = 8`; 14 is within 0.03 points of it on this workload | Fix text (state N = 8, calibrated) |
| 12 | Eviction overhead "drops to zero" (§3.4.2) | O(N) per eviction | Fix text |
| 13 | Score appended as `X-Bot-Probability` header and forwarded to a separate cache (§3.2.3, §3.4.1) | Single process; `DtacCache::access` passes the score directly in memory | Fix text |
| 14 | Alg. 2: `fetch_from_origin` inside the shard lock; `score >> delta_t` on a float | Fetch happens outside the lock, with a re-check for concurrent promotion; decay is stepwise, `score / 2^⌊Δt / decay_half_life_ms⌋`, zeroed after `decay_cutoff_half_lives` half-lives (calibrated 4 h and 32) | Fix text (the real design is better — describe it) |
| 15 | Data generator and replayer are "custom Rust scripts" (§3.3.3, §3.5) | Generator is Python (`scripts/generate_logs.py`); replayer is Rust | Fix text |
| 16 | Bots sample uniformly from a "separate subset" of the catalog (§3.5.2) | Bots sample uniformly from the **entire** catalog, overlapping with human items | Fix text |
| 17 | Bots identified via omitted favicon, missing headers, mechanical timing (§3.5.2) | True of the naive generator; the router sees velocity, `Accept-Language`, the favicon (Shy Guys) and datacenter ASN. The naive pool grows with the ratio at a fixed 3.6 req/s per IP (about 64% datacenter, the rest TEST-NET); a stealthy workload shows the heuristics failing | Fix text; state the per-IP-rate assumption and the stealthy result |
| 18 | Warm-up runs "until the cache reached absolute maximum capacity" (§3.6.1) | Fixed 50,000 pure-human requests prepended to every log | Fix text |
| 19 | Replayer fires requests at the proxy over `localhost` (§3.3.3, §3.6.1) | Reported results are in-process (`replay_bench`). A networked mode (`--target-url` → `dtac_standalone` → `origin_stub`) exists and matched the in-process human hit rate (58.29% vs 58.32%) | Fix text: in-process for the grid, one networked check |
| 20 | The 10/30/50/75% logs are dispatched "concurrently" (§3.6.1) | One separate run per (seed, ratio, algorithm, capacity), run sequentially by `run_experiments.py`; 5 seeds, mean ± 95% CI | Fix text |
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
- [x] Lock-sharded cache engine (up to 256 shards, auto-sized)
- [x] Metadata-only ghost cache with promotion at `PROMOTION_THRESHOLD` (calibrated 1.5)
- [x] Per-resource score increment of `1 − bot_prob` with time decay
- [x] Random-sample eviction tournament (`N_SAMPLES`, calibrated 8) with O(1) `swap_remove`
- [x] Origin fetch outside the lock, safe against concurrent duplicate promotion
- [x] `DtacCache` wiring router → engine, with OpenTelemetry metrics and `tracing` spans
- [x] `dtac_standalone` Axum reverse proxy with real peer IP and Prometheus `/metrics`
- [x] Synthetic traffic generator: Zipf(α=0.8, 100k items) humans, uniform bots, 10/30/50/75% ratios
- [x] Human-only warm-up prefix, excluded from measurements
- [x] `replay_bench` with timestamp pacing, concurrency limit, human/bot/overall hit rates, throughput, average access time
- [x] Bot-unaware sharded LRU baseline (`--algorithm lru`)
- [x] Results against LRU, SIEVE, S3-FIFO and the ablation at all four bot ratios, both workloads, five seeds ([Benchmark results](#benchmark-results))
- [x] Unit + integration tests for the router, SpaceSaver, cache engine and benchmark helpers

**To implement** (roughly in priority order for the thesis)

- [x] Ablation run (`dtac-no-score`): DTAC's engine with a constant `sigmoid(l_0)` score (not `0`, which would also change promotion speed) — separates the AI score from ghost admission
- [x] Scan-resistant baselines: SIEVE and S3-FIFO
- [x] Calibration of every router/engine constant on a held-out seed (`results/tuning.md`)
- [x] Configurable shard count (`auto` keeps ≥ 64 entries per shard) and a capacity sweep (1 000–20 000)
- [x] Bound the ghost cache (per-shard cap, evicted by the score-aware tournament rather than FIFO so bot scans can't flush it)
- [x] Drive time-based logic from log timestamps (`replay_bench --clock log`, an injected `Clock`), with every time constant configurable
- [x] Repeated runs with different seeds (1–5), mean ± 95% CI and paired-difference CIs (`scripts/run_experiments.py`)
- [x] Split latency: `T_proxy` (router) and `T_cache` (engine) reported separately, with p50/p90/p99/max
- [x] Memory measurement (peak RSS, RSS delta and structure sizes) for DTAC vs. LRU
- [x] Eviction logging by origin (human vs. bot admitter) and a Cache Pollution Index (share of resident entries no human has requested since admission)
- [x] Variable payload sizes in the generator, with Byte Hit Rate
- [x] Router classifier metrics: ROC-AUC, precision/recall/FPR, class-mean gap, new vs. returning visitor FPR
- [x] Stationary bot mixing, a shared human stream across ratios, multi-page human sessions, seeds and `.meta.json` sidecars, `--json-out`
- [x] Optional: networked benchmark through `dtac_standalone` (trusted `X-Forwarded-For`, `X-Cache`, `origin_stub`, `replay_bench --target-url`)
- [x] Optional: "stealthy bot" workload (browser headers, rotating IPs, human-like timing) to show where the heuristics stop working

### Status of the research objectives

From Ch. 1 §1.3.2:

| Objective | Status | Notes |
|---|---|---|
| 1. Lightweight passive heuristic detector using HTTP header anomalies and structural requests | **Achieved against naive bots; fails against stealthy ones** | Velocity, `Accept-Language`, the structural ("Shy Guys") heuristic and datacenter-ASN lookup all work. With the calibrated constants, over seeds 1–5, the router separates naive bots perfectly (ROC-AUC 1.000, class-mean gap 0.976, recall 100% and FPR 0% at 0.5, `T_proxy` p99 23–39 µs). Returning visitors score slightly higher than new ones (0.030 vs 0.021) without crossing 0.5. Against stealthy bots, AUC is 0.485. Caveat: the ASN fixture matches the generator's bot IPs by construction. |
| 2. Real-time continuous AI-score in `[0, 1]` in O(1) | **Achieved except the O(1) claim** | The score is continuous and computed per request. Cost is O(log K) because of the `BTreeSet`s. True O(1) is possible with a stream-summary bucket list, but the half-life count decay makes it hard. Most practical route: reword to "bounded O(log K), independent of traffic volume". |
| 3. DTAC that modifies LRU eviction using the AI-score | **Achieved in substance, not as worded** | The AI-score does drive admission and eviction, but the design is not a modified LRU. Reword the objective rather than rebuilding to match it. |
| 4. Benchmark DTAC hit rates against standard LRU under simulated mixed workloads | **Achieved, with a capacity caveat** | Five seeds, paired CIs. Naive workload, capacity 10 000: DTAC beats LRU by +6.2 to +17.1 points, SIEVE by +1.2 to +3.9, S3-FIFO by +1.0 to +5.1 and its no-score ablation by +0.3 to +4.3, every CI excluding 0. Stealthy workload: DTAC ties its ablation (the router is at chance) and still beats LRU. At capacity 20 000 DTAC loses to SIEVE and S3-FIFO. |

Other goals and claims from the scope and methodology:

| Goal / claim | Status |
|---|---|
| Bounded memory for client state (SpaceSaver) | Achieved |
| Bounded memory for the whole system | Achieved — the ghost cache is now bounded per shard (`ghost_multiplier` × main capacity) |
| Lock-sharded, concurrent cache | Achieved |
| Measure memory consumption and read-path CPU overhead | Measured: peak RSS, RSS delta, structure sizes, and per-phase latency percentiles |
| `T_proxy` below 2 ms | Achieved in-process: `T_proxy` p99 23–39 µs across the capacity-10 000 runs (mean over seeds). Over localhost HTTP the whole round trip is p99 9.7 ms, dominated by the network stack |
| Byte Hit Rate, Cache Pollution Index, eviction-by-origin logging | Implemented (see [Comparing algorithms on one log](#comparing-algorithms-on-one-log)). BHR is size-unaware by design. The CPI definition differs from §3.7.2 (row #23) |
| Human hit rate stays stable regardless of bot volume (§3.7.1) | **Achieved for bots the router detects, not for those it can't.** Naive workload, capacity 10 000: DTAC's human hit rate is 58.29% at 10% bots and 58.22% at 75% (LRU falls 11 points, SIEVE 2.8). Stealthy workload: it falls from 57.96% to 54.40%, about as much as SIEVE's. Reword to "stable when the bots are detectable". |
| CPI kept near 0% via tail insertion (§3.7.2) | There is no tail insertion, but on the naive workload DTAC's CPI is 0.0% at every ratio (LRU 14–79%, SIEVE 7–37%): detected bots never promote anything. On the stealthy workload it is 5–44%, close to SIEVE's. |
| Heuristic weights derived as real log-likelihood ratios | **Probably not achievable within scope.** Needs labelled real-world traffic. Estimating them from the synthetic generator would be circular, because the generator was written to trigger these exact heuristics. |
| Detecting sophisticated bots (rotating IPs, spoofed headers, human-like timing) — motivated in Ch. 1 §1.1.4 | **Not achievable with the current heuristics.** Velocity is per-IP, and the header check is trivially spoofed. State this as a limitation, not a goal. |
| "No existing literature" on bot-score-aware eviction (Ch. 2 §2.8) | Not verifiable — soften to "to the best of our knowledge". |
