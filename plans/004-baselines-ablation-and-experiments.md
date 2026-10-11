# Plan 004 — Calibration, stronger baselines, ablation, capacity sweep, repeated experiments, networked run

**Status:** not started
**Depends on:**
- 002: the clock and the bounded ghost cache.
- 003: `--seed`, `--json-out`, the `replay_bench` module split, the eviction hooks and `resident_keys`.
- 001 should land first so the final numbers include all four heuristics.

## Context

The README's evaluation currently rests on one run per bot ratio against plain LRU. The "To implement"
checklist and the objective-4 status list what is needed for the thesis claims to hold up:

- **An ablation.** Without it, nothing separates the effect of the AI-score from the effect of ghost-cache
  admission alone.
- **Scan-resistant baselines (SIEVE, S3FIFO).** Ch. 2 cites both as already strong under scans, so beating
  only LRU is a weak claim.
- **Repeated runs with different seeds, reported as mean ± spread.** The 50% ratio is currently a 0.01-point
  "tie" with no error bars.
- **Optionally, a networked run through `dtac_standalone`** to match the thesis's "over localhost" setup
  (row #19). That needs the proxy to see real client IPs, which means trusted `X-Forwarded-For` handling.

All of this is experiment infrastructure, plus the final refresh of the README results.

Two more gaps:

- **No calibration step.** Plans 001 and 002 made the router prior and weights and every time constant
  configurable, but nothing chooses them. On log time the old defaults are known to fail (see 002 Context).
- **Only one cache size.** Hit-rate-vs-capacity curves are the standard x-axis in the caching literature,
  and a result at one capacity can be an artefact of that capacity.

## Part 0 — Calibration on a held-out seed (do this first)

The constants have to be chosen on data the final results never see. Otherwise the reported improvement
is partly overfitting to the evaluation set.

- **Tuning seed = 0.** Evaluation seeds are `1..=5`. The harness refuses to run evaluation seeds while
  `results/tuning.md` doesn't exist, and refuses to include seed 0 in the summary.
- **Tune on the naive workload only.** The stealthy workload is the planned failure case. Tuning on it
  would blur the limitation it is meant to show.
- **Use ratios 30% and 75%, at capacity 10 000.** Check that the chosen values aren't badly off at the
  other capacities in the sweep (Part B). Don't re-tune per capacity.
- **What is swept**, coarse grid first, then refine around the best region:
  - Router (judged by plan 003's classifier metrics: AUC, class-mean gap, human FPR for returning
    visitors):
    - `prior_bot_rate`
    - `w_hdr`, `w_fav`, `w_asn`
    - `velocity_tick_ms`, `burst_threshold`
    - `shy_page_threshold`
  - Engine (judged by human hit rate, the primary metric, with the router fixed at the chosen values):
    - `decay_half_life_ms`
    - `promotion_threshold`
    - `ghost_multiplier`
    - `n_samples` (also settles thesis row #11)
- **Objective.** First maximise the router's class-mean gap, subject to returning-visitor FPR ≤ 5%. Then
  maximise human hit rate. Tune the two stages one after the other, not jointly, so each choice can be
  explained in the thesis.
- **Weights as log-likelihood ratios.** The thesis describes the weights as `ln(P(E|bot)/P(E|human))`.
  Report the empirical LLR of each heuristic on the tuning seed next to the chosen weight, but **don't**
  claim it as validation. The generator was written to trigger these heuristics, so the LLRs are
  circular. Say so in `results/tuning.md` and in the README.
- **Outputs:**
  - `results/tuning.md`: the sweep tables, the chosen values, and the reasoning for each.
  - Updated `Default` impls for `RouterConfig` and `EngineConfig`, so that the library defaults are the
    calibrated values.
  - Update 002's `default_config_misses_benchmark_bot_rate` test to match the new defaults.
- Script: `scripts/run_experiments.py tune ...`, a subcommand of the same harness, so the runner and JSON
  plumbing are reused.

## Part A — Ablation and new baselines (`src/bin/replay_bench/engines.rs`)

Extend the `Algorithm` enum. Every new engine uses the same shard count and capacity-per-shard math as
`BaselineLruEngine`, so only the policy differs.

- **Configurable shard count.** Today it's `NUM_SHARDS = 256` everywhere. At capacity 1 000 that's about
  4 entries per shard: S3FIFO's 10% small queue rounds to 0, and DTAC's 5-sample tournament covers the
  whole shard. Both make the capacity sweep meaningless at the low end.
  - Make the shard count an engine parameter (`EngineConfig.shards`, and the same for every baseline).
  - Default to `auto`: the largest power of two ≤ 256 that leaves at least 64 entries per shard.
  - Report the shard count in `--json-out`.
  - Library behaviour at capacity ≥ 16 384 is unchanged.
- **`dtac-no-score`**: the ablation.
  - Drive a `DtacCacheEngine` directly with a **constant** `request_bot_prob = sigmoid(l_0)`, the score a
    clean human request gets under the calibrated config. Don't call the router.
  - **Why not `0.0`**: with `bot_prob = 0` every request adds a full 1.0, which changes how fast items
    promote as well as removing discrimination. The ablation would then mix two effects. With a constant
    equal to the human baseline, human traffic behaves exactly as in `dtac`, and the only difference is
    that bots are no longer told apart.
  - Same capacity, clock, shards and `EngineConfig` as `dtac`.
  - It isolates "ghost admission + decayed-score tournament" from the AI-score.
- **`sieve`**: SIEVE (Zhang et al., NSDI '24). Per shard:
  - a FIFO of entries with a `visited` bit and a moving hand
  - on a hit, set `visited`
  - on eviction, the hand walks from tail to head, clearing `visited` bits until it finds an unvisited
    entry
  - insert at the head
  - Implement it with a `Vec` slab plus index-linked list (no `unsafe`, as the lint config requires).
- **`s3fifo`**: S3-FIFO (Yang et al., SOSP '23). Per shard:
  - small FIFO S (10% of capacity), main FIFO M (90%), ghost FIFO G (keys only, sized as M)
  - a 2-bit frequency counter capped at 3
  - on insert, a key found in G goes to M, otherwise to S
  - when S is evicted, an entry with freq > 1 moves to M and the rest go to G
  - M is evicted with reinsertion while freq > 0, decrementing as it goes
- All of them implement 003's eviction hook and `resident_keys`, so CPI and eviction-by-origin work for
  every algorithm.
- Turn the current `Engine` enum dispatch into a small `BenchEngine` trait (`access`, `resident_keys`,
  `set_eviction_listener`, `name`) now that there are 5 engines.
- Unit tests for each policy:
  - SIEVE: the worked example from the paper (visited entries survive one hand pass).
  - S3FIFO:
    - a one-hit-wonder scan never reaches M
    - a key re-requested after S-eviction goes from G into M
    - a frequency-capped M entry survives reinsertion
  - Capacity is never exceeded, and integrity holds after 100k random accesses.

## Part B — Experiment harness (`scripts/run_experiments.py`)

- **Grid**: seeds (default `1..=5`) × workloads (`naive`, `stealthy`) × ratios (10/30/50/75) × algorithms
  (`dtac`, `dtac-no-score`, `lru`, `sieve`, `s3fifo`) × **capacities** (default 1 000 / 5 000 / 10 000 /
  20 000, i.e. 1/5/10/20% of the catalog). Each axis is set with a CLI flag.
  - The full grid is 5 × 2 × 4 × 5 × 4 = 800 runs. Time one run at capacity 10 000 first and print an
    estimate before starting.
  - If that's too long, the default `--profile thesis` runs the capacity sweep only for the naive
    workload at 30% and 75%, and runs everything else only at 10 000. That's 5 seeds × 5 algorithms ×
    (naive: 2 ratios × 4 capacities + 2 ratios × 1; stealthy: 4 ratios × 1) = 350 runs. Each axis can
    still be overridden.
- Every cell records the run's metadata, including the git commit and both configs. The aggregator refuses
  to mix cells from different commits or configs unless `--allow-mixed` is passed.
- **Steps**:
  1. Build `--release` once.
  2. For each seed, run `generate_logs.py --seed S --out-dir benchmark_logs/seed_S`.
  3. Run `replay_bench --quiet --json-out results/raw/<seed>_<workload>_<ratio>_<algo>.json` for each cell,
     sequentially so runs don't fight over CPU.
  4. Skip cells whose JSON already exists, so an interrupted sweep can resume.
- **Aggregation**:
  - Write `results/summary.csv` with mean, std and 95% CI (t-distribution, n = seeds) per (workload,
    ratio, capacity, algorithm) for:
    - human hit rate, overall hit rate, BHR, bot hit rate
    - CPI
    - `T_proxy` p99
    - peak RSS delta, logical resident bytes, ghost entry count
  - **Paired differences.** Every algorithm sees the same human stream for a given seed (003), so also
    report `metric(dtac) − metric(X)` per seed, with mean and 95% CI of the *difference*, for each
    baseline X and for the ablation. This is far tighter than comparing two overlapping CIs, and it is the
    right test for "does DTAC beat X".
  - **Router quality table.** Per (workload, ratio): mean ± CI of AUC, class-mean gap, human FPR (new vs.
    returning visitors) and bot recall. Algorithm and capacity don't affect these, so take them from the
    `dtac` runs.
  - Write `results/summary.md` with ready-to-paste markdown tables in the same three-tier order as the
    README, plus one human-hit-rate-vs-capacity table per workload and ratio.
- Add `results/raw/` to `.gitignore`. Commit `results/summary.*` so the README numbers can be traced.
- Add a `--quick` flag (1 seed, 2 ratios) for smoke-testing the harness.
- Use only the standard library plus numpy, which is already in `scripts/requirements.txt`.
- Add `scripts/test_run_experiments.py` to unit-test:
  - the aggregation (mean/CI on fixed fake JSON)
  - paired-difference CIs
  - resume-skip
  - refusal to include seed 0, and refusal to mix commits

## Part C — Networked benchmark via `dtac_standalone` (optional, last)

- **`dtac_standalone`**:
  - Add `--trusted-proxy <CIDR>` (repeatable). The handler uses the right-most untrusted address from
    `X-Forwarded-For` only when the TCP peer is trusted, and otherwise the peer address. This closes the
    README's XFF limitation without making client IPs spoofable.
  - Remove the vestigial `--sketch-k`, `--cm-depth` and `--cm-width` flags that the README calls unused.
  - Replace `unwrap()` in `metrics_handler` and the `TcpListener::bind` call with proper error handling,
    because these are the pre-existing lint violations called out in CLAUDE.md.
  - Reuse one `reqwest::Client` in `AppState` instead of building one per miss (`proxy_handler` currently
    calls `reqwest::Client::new()` on every fetch).
- **New `src/bin/origin_stub.rs`**: a tiny Axum origin server that returns `size_bytes` zero bytes, with the
  size encoded in the path or a query parameter.
- **`replay_bench --target-url <URL>`**:
  - Instead of in-process engines, send each record as an HTTP request with `X-Forwarded-For: <record.ip>`
    and the record's headers.
  - Count hits via an `X-Cache: HIT|MISS` response header, which `dtac_standalone` adds using 003's
    `access_with_stats`.
  - The rest of the metric pipeline is reused.
- **Tests**:
  - Unit tests for XFF parsing: untrusted peer → peer IP; trusted peer → right-most untrusted hop;
    malformed header → peer IP.
  - An integration test that starts `origin_stub` and `dtac_standalone` on ephemeral ports and replays
    20 records to check that the hit/miss counts match an in-process run.

## Docs

- **README**:
  - Replace the Benchmark results section with the aggregated mean ± CI tables from
    `results/summary.md`, for both naive and stealthy workloads and all 5 algorithms. Lead with capacity
    10 000, then the capacity-sweep tables and the router quality table.
  - Rewrite the Interpretation, honestly either way:
    - what the ablation shows the AI-score contributes, using the paired-difference CI
    - whether DTAC beats SIEVE/S3FIFO on human hit rate, and at which capacities
    - how much of the stealthy-workload result is explained by the drop in router AUC
  - Summarize `results/tuning.md` (calibrated values, held-out seed, circular-LLR caveat) and link it.
  - Note the per-algorithm metadata overhead (DTAC's ghost, S3FIFO's ghost) next to the hit rates, so the
    comparison at equal entry capacity is read correctly.
  - Document `run_experiments.py`, the new `--algorithm` values, `--target-url`, `origin_stub` and
    `--trusted-proxy`.
  - Remove the XFF and vestigial-flags notes.
  - Update the thesis table (#19, #20, and any rows whose status changed) and tick the remaining checklist
    items.
  - Update the objective-4 status and the "human hit rate stays stable" row, using the new numbers.
- **CLAUDE.md**: update the benchmarking pipeline (harness, algorithms, networked mode) and the
  `dtac_standalone` description (trusted XFF, `X-Cache` header).

## Verification

1. `cargo test`, `cargo clippy --all-targets`, and the Python unit tests.
2. `python scripts/run_experiments.py --quick` completes and writes `results/summary.md`. Run it again and
   check that every cell is skipped (resume works).
3. Sanity-check the results:
   - On the **stealthy** workload, `dtac` and `dtac-no-score` are close, because the router can't tell
     bots apart there. On the **naive** workload, `dtac` is ahead. If the gap isn't larger on naive than on
     stealthy, the score isn't what is helping.
   - During warm-up (human-only), `dtac` and `dtac-no-score` promote the same keys. Check this with a
     unit test that replays a short human-only prefix through both and compares `resident_keys()`. It holds
     by construction now that the ablation uses `sigmoid(l_0)`.
   - SIEVE and S3FIFO both beat LRU on the human hit rate at 75% naive bots, as the literature predicts.
   If any check fails, investigate the implementation before trusting any comparison.
4. Full sweep: `python scripts/run_experiments.py`. Use the paired-difference CIs to decide each
   comparison, including the 50% ratio. If the CI of the difference includes 0, report it as a tie.
5. Check the capacity sweep: shard count is reported per cell, and no algorithm shows a cliff at 1 000
   that disappears at 5 000. A cliff like that would point back to a shard-size artefact.
6. Optional Part C:
   - Run `origin_stub` + `dtac_standalone --trusted-proxy 127.0.0.1/32` + `replay_bench --target-url`
     on the 30% file.
   - The human hit rate should be within the CI of the in-process `dtac` result.
