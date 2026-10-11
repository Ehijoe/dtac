# Plan 002 — Log-timestamp clock + bounded ghost cache + configurable time constants

**Status:** not started
**Depends on:** 001, which introduces the `DtacCache` builder and `RouterConfig` that this plan extends.
Do this before 003/004, because it changes what the benchmark numbers mean.

## Context

This plan fixes the two remaining *correctness/fidelity* problems in the library itself. Both are listed in
the README "Known limitations" and "To implement" checklist.

1. **Time-based logic runs on wall-clock time, not log timestamps.**
   - These all read real time:
     - `HeuristicRouter::current_timestamp_seconds` (`src/heuristic_router/mod.rs`)
     - `SpaceSaver::now` (`space_saver.rs`)
     - the cache engine's `Instant::now()` calls (`src/cache_engine/mod.rs`)
   - At the default `--speed 10000`, a replay finishes in seconds, so score decay barely happens and
     velocity is measured on a compressed timeline. `--speed` changes the experiment itself instead of just
     its runtime.
2. **The ghost cache is unbounded.** `CacheShard.ghost_cache` is a `HashMap` whose entries are only removed
   on promotion, so every one-off bot scan URL stays forever. This contradicts the thesis's
   bounded-memory claim (thesis table row #4).

**Warning: switching to log time will break the current constants.** They only behaved reasonably because
`--speed 10000` squeezed the whole replay into a few wall-clock seconds:

- **Cache decay** halves a score every second and zeroes it after 32 s. Humans arrive at about 12.5 req/s
  over a 100k-item catalog. On log time, outside the top handful of items almost nothing collects the 3+
  hits needed to promote (see 001 Part C) before the score decays away. Tournaments then compare scores
  that are nearly all 0, which makes eviction effectively random.
- **Velocity** shifts right once per second. The naive bot stream fires every ~20 ms across 14 IPs, so each
  IP sends about 3.6 req/s and its EWMA settles near 7, below `BURST_THRESHOLD = 10`. On log time the
  velocity heuristic never fires on the benchmark's bots.

So this plan must also make every time constant a parameter (Part C). Choosing the values is done in plan
004's calibration step (Part 0), on the final workload from plan 003. Until then, expect DTAC's numbers on
`--clock log` to look bad. That is expected and is not a reason to tune the constants here.

## Part A — Injectable clock

### New `src/clock.rs`

- Add a `pub trait Clock: Send + Sync + Debug` with `fn now_millis(&self) -> u64`, in milliseconds since an
  arbitrary epoch.
- Add `SystemClock`, which uses `SystemTime` since `UNIX_EPOCH`. It is the default everywhere, so library
  behaviour is unchanged.
- Add `ManualClock(AtomicU64)` with:
  - `set_millis(u64)`, which uses `fetch_max` so concurrent replay tasks can never move time backwards
  - `advance(Duration)`, for tests
- Export it from `src/lib.rs` (`pub mod clock;`), with doc comments on every public item.

### Plumbing (all constructors keep their current signatures; add `with_clock` variants)

- **`SpaceSaver`**:
  - Add an `Arc<dyn Clock>` field and replace `Self::now()` with `self.clock.now_millis() / 1000`.
  - `decay_and_prune` already takes `now` and needs no change.
  - Existing tests that call `SpaceSaver::now()` switch to a `ManualClock`, which also makes the decay
    tests deterministic.
- **`HeuristicRouter`**: hold the same `Arc<dyn Clock>` and pass it into its `SpaceSaver`. Velocity is
  computed in configurable ticks (see Part C) instead of hard-coded whole seconds.
- **`DtacCacheEngine` / `CacheShard`**:
  - Change `Metadata.last_accessed: Instant` to `u64` millis.
  - Compute `dt` in half-life ticks as `(now - last) / decay_half_life_ms` in both the hit path and
    `execute_eviction_tournament`. Pull the duplicated decay expression into one
    `fn decayed_score(meta, now_ms, cfg) -> f32`.
- **`DtacCache`**: add `.clock(..)` and `.engine_config(..)` to the builder introduced in 001. The builder
  passes one shared clock to both the router and the engine.

### `replay_bench`

- Add `--clock <log|wall>`, default `log`.
  - In `log` mode, build a shared `ManualClock` and call `clock.set_millis((record.timestamp * 1000.0) as u64)`
    in each task immediately before `access`.
  - `--speed` then only controls how fast the replay runs. It no longer changes the decay or velocity
    behaviour being measured.
  - `wall` reproduces today's behaviour for comparison.
  - Known imprecision: with `fetch_max` across up to `--concurrency` in-flight tasks, a request can see a
    clock a few milliseconds ahead of its own timestamp, if a later record's task set it first. With
    half-lives of seconds this doesn't matter. Document it next to the flag rather than adding a
    per-request `access_at(now)` API.
- Print the clock mode and all time constants in the run header.

## Part B — Bounded, score-aware ghost cache

Use score-aware eviction instead of plain FIFO (S3FIFO-style). With a FIFO ghost, a uniform bot scan would
push genuinely popular human items out of the ghost before their second hit, which is a new pollution
vector. Bot-inserted ghost entries start with a score of about `1 - bot_prob ≈ 0`, so the same randomized
tournament used for the main cache evicts them first.

- Make the ghost storage dense, like the main cache: `ghost_map: HashMap<K, usize>` plus
  `ghost_entries: Vec<(K, Metadata)>`.
- Generalize the tournament into a free function over `&[(K, impl HasMeta)]` that returns the victim
  index. The main cache and the ghost cache then share the sampling, decay, `swap_remove` and index-fixup
  code. Today that logic lives only in `execute_eviction_tournament`.
- **Tie-break on the oldest `last_accessed`.** Once scores have decayed, many sampled entries tie at or near
  0, and today the first one sampled wins, which is random. Comparing `(decayed_score, last_accessed)` makes
  ties fall back to LRU order. This applies to both the main cache and the ghost cache.
- Add `pub const GHOST_CAPACITY_MULTIPLIER: usize = 4`, so the ghost capacity per shard is
  `capacity_per_shard * multiplier`. Add `DtacCacheEngine::with_options(capacity, ghost_multiplier, clock)`.
- On a miss, if the ghost is full, run the ghost tournament before inserting.
  - On promotion, remove the entry from the ghost (now an indexed removal with fixup).
  - The existing concurrent-promotion guard in `Action::Promote` stays correct if the ghost entry was evicted
    while the fetch was in flight. The commit path only *removes* from the ghost, so a missing key is fine.
- Extend `debug_integrity_counts` (or add `debug_ghost_counts`) so tests can check
  `ghost_entries.len() == ghost_map.len() <= ghost_cap`.
- `replay_bench`: add `--ghost-multiplier <N>` so the sensitivity of this new parameter can be measured.
- Fairness note for plan 004: the ghost stores only metadata, but at multiplier 4 it holds 4× as many keys
  as the main cache. Expose its entry count so 004 can report per-algorithm metadata overhead next to the
  hit rates.

## Part C — Parameterize every time constant

Nothing here chooses values. Defaults stay equal to today's behaviour, so that `--clock wall` still
reproduces the current numbers. Plan 004 Part 0 picks the real values.

- **`EngineConfig`** (`Debug, Clone`, documented, with a `Default` equal to today's values):
  - `decay_half_life_ms: u64`, default 1000. It replaces the hard-coded "halve per second".
  - `decay_cutoff_half_lives: u32`, default 32. It replaces the hard-coded `dt >= 32`.
  - `promotion_threshold: f32`, default 2.0. It replaces `PROMOTION_THRESHOLD`; keep the `const` as the
    documented default.
  - `ghost_multiplier: usize`, default 4.
  - `n_samples: usize`, default 5. It replaces the `N_SAMPLES` const, so thesis row #11 (5 vs 14) can be
    settled with a sweep instead of an argument.
- **`RouterConfig`** (from 001) gains:
  - `velocity_tick_ms: u64`, default 1000. One right-shift per tick replaces one shift per second.
  - `velocity_reset_ticks: u64`, default 10. It replaces the `delta_t > 10` reset.
  - `space_saver_half_life_s`.
  - `burst_threshold` and `alpha` are already in it.
- **`replay_bench`**: add `--engine-config <PATH>` (JSON), and accept 001's `--router-config`, so plan 004's
  calibration sweep can set any constant without adding one flag per constant. Write the effective configs
  into the run header and into 003's `--json-out`.

## Tests

- **`clock.rs`**: `ManualClock` is monotonic under `set_millis` with smaller values, and `advance` works.
- **SpaceSaver** (existing tests converted to `ManualClock`):
  - Half-life decay after advancing 2 half-lives.
  - Pruning once a count decays to 0.
- **Router** (`tests/heuristic_router_tests.rs`): with a `ManualClock`:
  - 20 requests within 1 simulated second push velocity over `BURST_THRESHOLD` and raise the score.
  - The same 20 requests spread over 20 simulated seconds do not.
  - A **regression test that documents the problem described in Context**: one IP at the benchmark's
    per-IP bot rate (about 3.6 req/s for 60 simulated seconds) does *not* trip the burst penalty under the
    default config. Name it so that it's obviously expected to change when 004 picks new defaults, e.g.
    `default_config_misses_benchmark_bot_rate`.
  - Changing `velocity_tick_ms` changes the outcome of the previous test as expected.
- **Engine** (`tests/cache_engine_tests.rs`):
  - Decay is driven by the injected clock: advance 40 s, and a hot entry's decayed score is 0 and it loses
    the tournament.
  - After 100k unique keys, each shard's ghost size is at most the bound, and the integrity counts still
    match.
  - Bot-scored ghost entries (`bot_prob = 1.0`) are evicted before human-scored ones under pressure.
  - Tie-break: when all sampled entries have decayed to 0, the one with the oldest `last_accessed` is
    evicted.
  - `decay_half_life_ms` is respected: with half-life 10 s, advancing 10 s halves the score.
  - `promotion_threshold` and `n_samples` from `EngineConfig` take effect.
  - The existing concurrent-promotion/no-orphans test still passes.
- **`replay_bench` unit test**: in log-clock mode, the clock value equals the last record's timestamp after
  dispatch.

## Docs

- **README**:
  - Remove the "ghost cache is unbounded" and "wall-clock time" Known-limitations bullets.
  - Document `--clock`, `--ghost-multiplier`, `--engine-config` and `--router-config` in the CLI table.
  - Add a short "Time constants" note: they are now parameters, and the defaults are provisional until
    plan 004's calibration.
  - Update the Architecture "Ghost cache" bullet.
  - Thesis table #4 → "Done", and correct row #14's decay description.
  - Tick both checklist items.
  - Add a note that results produced before this change used the wall clock. The results tables are
    refreshed in plan 004.
- **CLAUDE.md**:
  - Update the cache engine section (ghost bound and tournament, `u64` millis clock).
  - Update the router section to say time comes from an injected `Clock`.

## Verification

1. `cargo test`, then `cargo clippy --all-targets` with no new warnings.
2. Run `replay_bench --file benchmark_logs/traffic_mixed_75pct_bot.ndjson --clock log` at `--speed 10000`
   and again at `--speed 1000`. Hit rates should agree within noise, which shows speed no longer changes
   the experiment. The `--clock wall` runs at the same speeds should differ.
3. `--clock wall` with default configs reproduces the pre-002 numbers within noise. This confirms that
   parameterizing changed nothing by itself.
4. Record (don't fix) the `--clock log` result with default constants in the PR description. It is the
   "before" number that justifies plan 004's calibration.
5. Do **not** choose the ghost multiplier here. It is tuned along with the other constants in plan 004
   Part 0, on the final workload. Keep the default at 4.
