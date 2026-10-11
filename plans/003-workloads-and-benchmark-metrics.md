# Plan 003 — Richer workloads + benchmark measurement

**Status:** not started
**Depends on:**
- 001, for the ASN fixture ranges (to keep humans out of them and to build the bot pool) and for
  `shy_page_threshold`.
- 002, because the log-timestamp clock makes the latency and decay numbers meaningful.

004 relies on the `--seed`, `.meta.json`, classifier metrics and `--json-out` added here.

## Context

The benchmark currently reports only hit rates (human/overall/bot), throughput, and an average combined
access time. Several metrics the thesis promises are missing. The README's "To implement" checklist and
thesis rows #21–#25 list them:

- eviction logging by origin, and a Cache Pollution Index (CPI)
- Byte Hit Rate (BHR). This is meaningless today because every payload is `--payload-size` bytes.
- split latency, `T_proxy` (router) vs `T_cache` (engine), with p50/p99
- memory consumption

The workload is also a single "naive scraper" model with no seed control. The README already admits that a
stealthy bot would evade the heuristics, and this plan adds a workload that shows exactly where. All of
these are changes to the generator and to `replay_bench`, so they belong together.

The current generator also has two structural flaws that undermine every result so far:

1. **The bot ratio is not stationary.** The bot stream fires at a fixed 20 ms cadence starting at the
   beginning of measurement, while humans fire at 80 ms. At "10% bots" the bot stream ends after about 3%
   of the measurement window, and the rest is pure human traffic. At 75% bots it covers about 75% of the
   window. So the ratios differ in timing as well as in bot share, and at low ratios the measured number is
   mostly post-burst recovery.
2. **Every human request comes from a fresh IP.** Per-IP heuristics separate humans from bots perfectly
   by construction, and the Shy Guys heuristic (001) can never produce a false positive.

Nothing measures the router on its own either. The README's objective-1 claim ("lightweight detector")
has no detection-quality numbers behind it.

## Part A — Generator (`scripts/generate_logs.py`)

- **CLI via `argparse`**. The defaults use the *new* model below. Add `--legacy` to reproduce today's
  output for comparison against the old README numbers.
  - `--seed` seeds both `random` and `numpy`.
  - `--out-dir`.
  - `--ratios`.
  - `--workload {naive,stealthy,both}`.
- **Metadata sidecar.** Next to each `.ndjson`, write a `.meta.json` file containing the seed, ratio,
  workload, every generator parameter and the generator's git commit. `replay_bench` reads it if it exists
  and copies it into `--json-out`. This replaces "parse the seed out of the filename". The NDJSON format
  stays unchanged, so old logs still replay.
- **Stationary bot mixing.** This replaces the two independent fixed-cadence streams.
  - **The same human stream for every ratio.** For a given seed, generate the human session stream once
    (same duration `T`, same requests) and reuse it in every ratio file. Human hit rates are then directly
    comparable across ratios, and across algorithms as paired samples.
  - **Bots spread over the same window.** For ratio `r` with `H` human requests, add
    `B = H · r / (1 − r)` bot requests spread over the same window `T`. The aggregate bot rate is `B / T`,
    so the bot share is about `r` in every slice of the timeline.
  - **Per-IP bot rate held constant across ratios.** The bot IP pool scales with the ratio:
    `pool = ceil((B / T) / bot_ip_rate)`, with `--bot-ip-rate` defaulting to the current ~3.6 req/s.
    Otherwise the velocity heuristic would see different per-IP rates at different ratios, a confound
    between "more bot traffic" and "faster bots". Keep the current mix: about 64% of pool IPs in the 001
    fixture's datacenter ranges and the rest in TEST-NET.
  - Each bot IP keeps the mechanical cadence (`gauss(1 / bot_ip_rate, small jitter)`).
  - Add a test: in each tenth of the measurement window, the bot share is within ±3 points of `r`.
- **Multi-page human sessions.** This replaces "one request per fresh IP".
  - Each session has one IP and visits a geometric number of pages (`--pages-per-session-mean`, default
    5), with exponential think time between pages (`--think-time-mean`, default 8 s).
  - Each page is drawn from the Zipf catalog.
  - Run enough sessions concurrently that the aggregate human request rate stays about where it is today
    (≈12.5 req/s), so decay dynamics aren't changed by accident.
  - **Favicon:** a *new visitor* (`1 − returning_frac`) fetches `/favicon.ico` once, on the first page.
    A *returning visitor* (`--returning-frac`, default 0.3) has it cached and never fetches it.
    Returning visitors who see ≥ `shy_page_threshold` pages are the realistic Shy Guys false positives.
  - Optional NAT: `--nat-frac` (default 0) puts that fraction of sessions behind a small set of shared
    IPs. Leave it off for the main results, and use it for one sensitivity run.
  - Human IPs must exclude the 001 fixture's datacenter ranges and the TEST-NET ranges, so the per-IP and
    ASN signals don't collide by chance.
  - Warm-up uses the same session model and is still human-only.
- **Variable payload sizes**: every record gets `size_bytes`.
  - Item size is deterministic per item id: a lognormal with a median of about 16 KB, clipped to
    [512 B, 2 MB], drawn from an `np.random.default_rng(item_id)`. Repeat requests for an item therefore
    always get the same size.
  - `/favicon.ico` is fixed at about 1 KB.
- **Stealthy bot workload**, written as `traffic_stealthy_{pct}pct_bot.ndjson`:
  - full browser headers (`Accept-Language`, `Sec-CH-UA`), copied from `HUMAN_USER_AGENTS`
  - IPs rotated from a large residential-looking pool (`random_public_ipv4()` with about 5 requests per IP)
  - exponential human-like inter-arrival times instead of the fixed ~20 ms cadence
  - one `/favicon.ico` per IP, which defeats the Shy Guys heuristic from 001
  - still samples the catalog uniformly, so it still pollutes the cache
  - mixed into the same shared human stream with the same stationary-mixing rule, so naive and stealthy
    files differ only in bot behaviour

  The existing naive files keep their names. Label stealthy records `"traffic_type": "bot"` and add
  `"bot_kind": "stealthy"`.
- **`scripts/test_generate_logs.py`**:
  - The same seed gives byte-identical files.
  - `size_bytes` is stable per item id and within bounds.
  - Stealthy bots carry `Accept-Language` and fetch at most one favicon per IP.
  - Stealthy IP cardinality is high.
  - Stationarity: the bot share in each tenth of the window is within ±3 points of the target ratio.
  - For a given seed, the human records are identical across all ratio files and both workloads.
  - Per-bot-IP request rate is about `--bot-ip-rate` at every ratio.
  - Session model: mean pages per session ≈ the flag; returning visitors never fetch the favicon; new
    visitors fetch it exactly once.
  - No human IP falls inside the fixture's datacenter ranges or TEST-NET.
  - `.meta.json` exists and round-trips the CLI arguments.
  - `--legacy` reproduces today's files byte-for-byte for a fixed seed.

## Part B — `replay_bench` measurement

Before adding these features, split the binary into a directory: `src/bin/replay_bench/main.rs`,
`metrics.rs` and `engines.rs`. It is already about 550 lines, and 004 adds more engines.

### Record handling

- `LogRecord` gains `#[serde(default)] size_bytes: Option<u64>` and `bot_kind: Option<String>`. Old logs
  still parse.
- The fetch closure returns a payload of `size_bytes` (falling back to `--payload-size`).
  - Reuse one preallocated max-size `Bytes` and `slice(..n)` it, so allocation doesn't distort the timing.

### Byte Hit Rate

- Add `human_bytes` and `human_miss_bytes` atomics, plus the same pair for all traffic.
- `BHR = 1 - miss_bytes / bytes`.
- Caveat to state in the README: capacity counts **entries, not bytes**, and no engine (DTAC included)
  takes object size into account when admitting or evicting. BHR here reflects how hits on popular items
  happen to correlate with size. It doesn't measure a size-aware policy. Don't present it as more than
  that.

### Router classifier metrics

These are the direct evidence for objectives 1 and 2, measured independently of the cache. They are also
what plan 004's calibration uses to choose the prior and weights.

- For every measurement request, record `(bot_prob, label, bot_kind)` from `access_with_stats`. DTAC
  only.
- Report, per workload file:
  - **ROC-AUC** of `bot_prob` against the ground-truth label.
  - **Precision, recall and FPR at a 0.5 threshold.**
  - **Mean `bot_prob` per class.** The cache uses `1 − bot_prob` directly, so the gap between the class
    means is what actually drives cache behaviour. Report it next to AUC.
  - **Human FPR split into new vs. returning visitors.** This shows the Shy Guys false positives.
  - A coarse **histogram** of `bot_prob` per class (20 bins), included in `--json-out` so plots can be
    made later.
- AUC is computed by sorting, with no new dependency. The `(f32, bool)` pairs fit in memory. At about
  10⁶ records that's roughly 8 MB.

### Split latency (`T_proxy` / `T_cache`)

- Library: add `DtacCache::access_with_stats(...) -> (P, AccessStats)`, where
  `AccessStats { hit: bool, bot_prob: f32, router_time: Duration, engine_time: Duration }`.
  - `access` becomes a thin wrapper that discards the stats.
  - It reuses the `was_hit` flag and the timing already measured in `DtacCache::access`
    (`src/dtac_cache.rs`).
- Bench:
  - Record each phase into an `hdrhistogram::Histogram<u64>` (new dependency, used only by the bench),
    in nanoseconds.
  - Keep one histogram per task batch, merged under a `Mutex` at the end, so recording doesn't become a
    lock bottleneck.
  - Report p50/p90/p99/max for `T_proxy`, `T_cache` and total.
  - Baselines report only `T_cache` and total.

### Eviction by origin + Cache Pollution Index

The metric is engine-agnostic and uses the log's ground-truth labels.

- **Admitter labels.** The bench keeps `admitted_by: DashMap/Mutex<HashMap<String, Origin>>`, set on every
  `fetch_from_origin` call to that request's label.
  - For LRU, a fetch *is* the admission.
  - For DTAC, the fetch that happens on promotion is the last fetch before the key enters the main cache,
    and hits never fetch.
  - So "label of the most recent fetch" identifies the *triggering* request for every engine. For DTAC
    that request isn't necessarily responsible for the admission: humans may have built up 1.9 points and
    a bot tipped the score over the threshold. That's why CPI below doesn't rely on this label alone.
- **Human-touched flag.** Next to `admitted_by`, keep a per-key `human_since_admit: bool`. It is reset to
  `label == human` on every fetch and set to `true` by any later human request for the key, hit or miss.
- **Eviction hook.** Add an optional eviction listener to `DtacCacheEngine`:
  - `with_eviction_listener(Arc<dyn Fn(&K) + Send + Sync>)`, called from the main-cache tournament after
    the shard lock is released. Collect victims in the locked section and invoke the listener afterwards.
  - Ghost evictions are not reported.
  - The baseline LRU switches from `put` to `push`, which returns the evicted pair.
  - The bench increments `evictions_{human,bot}_admitted`.
- **CPI.**
  - Add `resident_keys()` on both engines (locks each shard in turn; for diagnostics only, like
    `debug_integrity_counts`).
  - Every `--cpi-sample-every N` measurement requests (default 10 000), and at the end, compute
    **CPI = the share of resident keys with `human_since_admit == false`**. These are entries that no human
    has requested since they entered the cache, which is the capacity actually wasted on bot traffic.
    - A bot-triggered admission of an item that humans then use is **not** counted as pollution. A plain
      "bot-admitted share" would count it, which penalizes LRU unfairly whenever bots happen to hit popular
      items.
    - Also report the raw "bot-triggered share of resident keys" as a secondary number, labelled clearly,
      because it is what thesis §3.7.2 literally describes.
  - Report the mean and final CPI.
  - Update the thesis CPI definition (§3.7.2) to match. Note it in the README thesis table, row #23.

### Memory

- Report peak RSS (`VmHWM` from `/proc/self/status`, Linux only, `None` elsewhere) and the RSS delta from
  "records loaded" to "replay done".
- Report the structure sizes at the end:
  - main/ghost entry counts (from 002's debug counts)
  - `SpaceSaver` tracked IPs, via a new `HeuristicRouter::tracked_ips()`
- Note in the report that the in-memory log dominates absolute RSS. The delta is the meaningful number.
- **Payloads are excluded from RSS by construction.** All cached payloads are `slice`s of one shared
  `Bytes` buffer, so they cost almost nothing in RSS. Say so in the report, and also report **logical
  resident bytes** (the sum of `size_bytes` over `resident_keys()`). RSS delta then measures metadata
  overhead (ghost, SpaceSaver, maps), which is the number the thesis cares about when comparing DTAC with
  LRU.

### Machine-readable output

- `--json-out <path>` writes every reported metric plus the run config as JSON. Plan 004's experiment
  harness consumes it. The run config includes:
  - file, algorithm and capacity
  - the log's `.meta.json` contents, including the seed
  - clock mode, and the effective `RouterConfig`/`EngineConfig` from 001/002
  - the git commit, from `option_env!` or `git rev-parse` at runtime
- Also add `--quiet`, which suppresses the progress bar and the Prometheus dump for scripted runs.

## Tests

- Unit tests in the `replay_bench` modules:
  - BHR arithmetic.
  - Percentile extraction from a merged histogram.
  - Admitter-label map plus eviction counting on a hand-driven baseline LRU (extends the existing
    `baseline_lru_evicts_least_recently_used_within_a_shard` setup).
  - CPI on a tiny forced-full cache, including a case where a bot-admitted key that a human later hits is
    *not* counted.
  - Old-format records without `size_bytes` still parse.
  - AUC on hand-made inputs: perfectly separated gives 1.0, reversed gives 0.0, all tied gives 0.5.
- `tests/cache_engine_tests.rs`:
  - The eviction listener fires exactly once per main-cache eviction and never while a shard lock is held:
    the listener itself calls `debug_integrity_counts`, which would deadlock if it were.
  - `resident_keys().len() == debug_integrity_counts().0`.
- `tests/` (`DtacCache`): `access_with_stats` reports `hit` consistently with fetch-closure calls.

## Docs

- **README**:
  - New metrics in the report description: tiers stay as they are, with BHR beside hit rate, CPI and
    eviction-by-origin under "for reference", and latency/memory in their own section.
  - New generator and bench flags.
  - The stealthy workload and what it is meant to show.
  - The new traffic model: stationary mixing, a shared human stream, session parameters, and how `--legacy`
    relates to the old results.
  - A router-quality subsection (AUC, FPR/TPR, class-mean gap).
  - The BHR caveat and the payload-excluded-from-RSS caveat.
  - Tick the checklist items for split latency, memory, eviction logging/CPI, and variable payload/BHR.
  - Update thesis rows #21–#25.
- **CLAUDE.md**:
  - The benchmarking pipeline section: new flags, stealthy workload, `--json-out`.
  - The `replay_bench` module layout.

## Verification

1. `cargo test`, `cargo clippy --all-targets`, `python -m unittest scripts/test_generate_logs.py`.
2. `python scripts/generate_logs.py --seed 1 --workload both`, then check that `size_bytes` and the
   stealthy files appear.
3. Run `replay_bench --algorithm dtac --json-out /tmp/r.json` and `--algorithm lru` on the 75% naive file.
   - The JSON contains CPI, BHR, latency percentiles, memory and classifier metrics.
   - The `T_proxy` p99 is reported, and the thesis target is < 2 ms.
   - Don't expect DTAC's CPI to beat LRU's yet. The time constants and prior aren't calibrated until 004
     Part 0. A bad result here is information, not a bug in this plan.
4. Run the 75% stealthy file through DTAC. Router AUC should drop sharply compared with the naive file.
   That is the expected and documented limitation.
5. Classifier sanity check on the naive file: AUC is well above 0.5, and the human FPR for returning
   visitors is higher than for new visitors, because Shy Guys is doing what the session model was built
   to expose.
