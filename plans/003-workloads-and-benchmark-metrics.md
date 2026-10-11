# Plan 003 — Richer workloads + benchmark measurement

**Status:** not started
**Depends on:** 002 (the log-timestamp clock makes the latency and decay numbers meaningful). 004 relies on
the `--seed` and `--json-out` added here.

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

## Part A — Generator (`scripts/generate_logs.py`)

- **CLI via `argparse`** (defaults keep today's output):
  - `--seed` seeds both `random` and `numpy`.
  - `--out-dir`.
  - `--ratios`.
  - `--workload {naive,stealthy,both}`.
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

  The existing naive files keep their names. Label stealthy records `"traffic_type": "bot"` and add
  `"bot_kind": "stealthy"`.
- **`scripts/test_generate_logs.py`**:
  - The same seed gives byte-identical files.
  - `size_bytes` is stable per item id and within bounds.
  - Stealthy bots carry `Accept-Language` and fetch at most one favicon per IP.
  - Stealthy IP cardinality is high.

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
  - So "label of the most recent fetch" is the admitting request for every engine.
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
    CPI = bot-admitted share of resident keys.
  - Report the mean and final CPI.

### Memory

- Report peak RSS (`VmHWM` from `/proc/self/status`, Linux only, `None` elsewhere) and the RSS delta from
  "records loaded" to "replay done".
- Report the structure sizes at the end:
  - main/ghost entry counts (from 002's debug counts)
  - `SpaceSaver` tracked IPs, via a new `HeuristicRouter::tracked_ips()`
- Note in the report that the in-memory log dominates absolute RSS. The delta is the meaningful number.

### Machine-readable output

- `--json-out <path>` writes every reported metric plus the run config (file, algorithm, capacity, seed if
  present in the filename, clock mode, git commit from `option_env!`/`git rev-parse` at runtime) as JSON.
  Plan 004's experiment harness consumes it.
- Also add `--quiet`, which suppresses the progress bar and the Prometheus dump for scripted runs.

## Tests

- Unit tests in the `replay_bench` modules:
  - BHR arithmetic.
  - Percentile extraction from a merged histogram.
  - Admitter-label map plus eviction counting on a hand-driven baseline LRU (extends the existing
    `baseline_lru_evicts_least_recently_used_within_a_shard` setup).
  - CPI on a tiny forced-full cache.
  - Old-format records without `size_bytes` still parse.
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
   - DTAC's CPI should be well below LRU's.
   - The `T_proxy` p99 is reported, and the thesis target is < 2 ms.
4. Run the 75% stealthy file through DTAC. The bot hit rate and CPI should rise toward LRU's, which is the
   expected and documented limitation.
