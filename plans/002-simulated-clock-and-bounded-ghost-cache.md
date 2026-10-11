# Plan 002 — Log-timestamp clock + bounded ghost cache

**Status:** not started
**Depends on:** nothing (can run before or after 001). Do this before 003/004, because it changes what the
benchmark numbers mean.

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
- **`HeuristicRouter`**: hold the same `Arc<dyn Clock>` and pass it into its `SpaceSaver`. The velocity math
  stays in whole seconds, as today.
- **`DtacCacheEngine` / `CacheShard`**:
  - Change `Metadata.last_accessed: Instant` to `u64` millis.
  - Compute `dt` seconds as `(now - last) / 1000` in both the hit path and `execute_eviction_tournament`.
    Pull the duplicated decay expression into one `fn decayed_score(meta, now_ms) -> f32`.
- **`DtacCache`**: replace the growing set of constructors (`new`, and 001's `with_router`) with a small
  builder: `DtacCache::builder(capacity).router(..).clock(..).build()`.
  - `new(capacity)` stays as shorthand for the defaults.
  - The builder passes one shared clock to both the router and the engine.

### `replay_bench`

- Add `--clock <log|wall>`, default `log`.
  - In `log` mode, build a shared `ManualClock` and call `clock.set_millis((record.timestamp * 1000.0) as u64)`
    in each task immediately before `access`.
  - `--speed` then only controls how fast the replay runs. It no longer changes the decay or velocity
    behaviour being measured.
  - `wall` reproduces today's behaviour for comparison.
- Print the clock mode in the run header.

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
- Add `pub const GHOST_CAPACITY_MULTIPLIER: usize = 4`, so the ghost capacity per shard is
  `capacity_per_shard * multiplier`. Add `DtacCacheEngine::with_options(capacity, ghost_multiplier, clock)`.
- On a miss, if the ghost is full, run the ghost tournament before inserting.
  - On promotion, remove the entry from the ghost (now an indexed removal with fixup).
  - The existing concurrent-promotion guard in `Action::Promote` stays correct if the ghost entry was evicted
    while the fetch was in flight. The commit path only *removes* from the ghost, so a missing key is fine.
- Extend `debug_integrity_counts` (or add `debug_ghost_counts`) so tests can check
  `ghost_entries.len() == ghost_map.len() <= ghost_cap`.
- `replay_bench`: add `--ghost-multiplier <N>` so the sensitivity of this new parameter can be measured.

## Tests

- **`clock.rs`**: `ManualClock` is monotonic under `set_millis` with smaller values, and `advance` works.
- **SpaceSaver** (existing tests converted to `ManualClock`):
  - Half-life decay after advancing 2 half-lives.
  - Pruning once a count decays to 0.
- **Router** (`tests/heuristic_router_tests.rs`): with a `ManualClock`:
  - 20 requests within 1 simulated second push velocity over `BURST_THRESHOLD` and raise the score.
  - The same 20 requests spread over 20 simulated seconds do not.
- **Engine** (`tests/cache_engine_tests.rs`):
  - Decay is driven by the injected clock: advance 40 s, and a hot entry's decayed score is 0 and it loses
    the tournament.
  - After 100k unique keys, each shard's ghost size is at most the bound, and the integrity counts still
    match.
  - Bot-scored ghost entries (`bot_prob = 1.0`) are evicted before human-scored ones under pressure.
  - The existing concurrent-promotion/no-orphans test still passes.
- **`replay_bench` unit test**: in log-clock mode, the clock value equals the last record's timestamp after
  dispatch.

## Docs

- **README**:
  - Remove the "ghost cache is unbounded" and "wall-clock time" Known-limitations bullets.
  - Document `--clock` and `--ghost-multiplier` in the CLI table.
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
3. Compare `--ghost-multiplier 1`, `4` and `16` at 75% bots. Record the human hit rate and choose the
   default from the data, keeping 4 unless the data clearly argues otherwise.
