# Plan 004 — Stronger baselines, ablation, repeated experiments, networked run

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

## Part A — Ablation and new baselines (`src/bin/replay_bench/engines.rs`)

Extend the `Algorithm` enum. Every new engine uses the same `NUM_SHARDS` and capacity-per-shard math as
`BaselineLruEngine`, so only the policy differs.

- **`dtac-no-score`**: the ablation.
  - Drive a `DtacCacheEngine` directly with `request_bot_prob = 0.0` and no router call.
  - Same capacity, clock and ghost multiplier as `dtac`.
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
  (`dtac`, `dtac-no-score`, `lru`, `sieve`, `s3fifo`). Each axis is set with a CLI flag.
- **Steps**:
  1. Build `--release` once.
  2. For each seed, run `generate_logs.py --seed S --out-dir benchmark_logs/seed_S`.
  3. Run `replay_bench --quiet --json-out results/raw/<seed>_<workload>_<ratio>_<algo>.json` for each cell,
     sequentially so runs don't fight over CPU.
  4. Skip cells whose JSON already exists, so an interrupted sweep can resume.
- **Aggregation**:
  - Write `results/summary.csv` with mean, std and 95% CI (t-distribution, n = seeds) per (workload,
    ratio, algorithm) for: human hit rate, overall hit rate, BHR, bot hit rate, CPI, `T_proxy` p99, and
    peak RSS delta.
  - Write `results/summary.md` with ready-to-paste markdown tables in the same three-tier order as the
    README.
- Add `results/raw/` to `.gitignore`. Commit `results/summary.*` so the README numbers can be traced.
- Add a `--quick` flag (1 seed, 2 ratios) for smoke-testing the harness.
- Use only the standard library plus numpy, which is already in `scripts/requirements.txt`.
- Add `scripts/test_run_experiments.py` to unit-test the aggregation (mean/CI on fixed fake JSON) and
  resume-skip.

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
    `results/summary.md`, for both naive and stealthy workloads and all 5 algorithms.
  - Rewrite the Interpretation: what the ablation shows the AI-score contributes, and whether DTAC beats
    SIEVE/S3FIFO on human hit rate. Report it honestly either way.
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
   - `dtac-no-score` matches `dtac` on 0%-bot warm-up behaviour.
   - SIEVE and S3FIFO both beat LRU on the human hit rate at 75% naive bots, as the literature predicts.
   If either check fails, investigate the implementation before trusting any comparison.
4. Full sweep: `python scripts/run_experiments.py`. Confirm the CIs are tight enough to call the 50% ratio
   (a non-overlapping CI, or report it as a tie).
5. Optional Part C:
   - Run `origin_stub` + `dtac_standalone --trusted-proxy 127.0.0.1/32` + `replay_bench --target-url`
     on the 30% file.
   - The human hit rate should be within the CI of the in-process `dtac` result.
