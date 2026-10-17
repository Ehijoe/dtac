"""Experiment harness for plan 004: repeated replay_bench runs and their aggregation.

`run` (the default subcommand) generates one log set per seed, replays every cell of the
grid (seed x workload x ratio x algorithm x capacity) through `replay_bench --json-out`,
one at a time so runs don't compete for CPU, and then aggregates them:

- `results/raw/<seed>_<workload>_<pct>_<algorithm>_<capacity>.json`: one file per cell.
  Existing files are skipped, so an interrupted sweep resumes where it stopped.
- `results/summary.csv`: mean, std and 95% CI (t-distribution over seeds) of each metric
  per (workload, ratio, capacity, algorithm), the paired differences `dtac - X` per seed,
  and the router's classifier metrics.
- `results/summary.md`: the same as ready-to-paste markdown tables.

`aggregate` redoes only the aggregation, over every file in `results/raw/`.

Seed 0 is reserved for calibration (`tune`, plan 004 part 0): evaluation seeds are refused
until `results/tuning.md` exists, and seed 0 is never aggregated into the summary.

Only the standard library is used here; the log generator needs numpy.
"""

import argparse
import csv
import io
import json
import math
import os
import statistics
import subprocess
import sys
import time
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
BENCH = REPO / "target" / "release" / "replay_bench"
GENERATOR = REPO / "scripts" / "generate_logs.py"
ASN_DB = REPO / "data" / "asn" / "ip2asn-fixture.tsv"
RESULTS_DIR = REPO / "results"
LOGS_DIR = REPO / "benchmark_logs"

ALGORITHMS = ["dtac", "dtac-no-score", "lru", "sieve", "s3fifo"]
BASELINES = [a for a in ALGORITHMS if a != "dtac"]
WORKLOADS = ["naive", "stealthy"]
RATIOS = [0.10, 0.30, 0.50, 0.75]
CAPACITIES = [1_000, 5_000, 10_000, 20_000]
PRIMARY_CAPACITY = 10_000
# The thesis profile sweeps capacity only for these naive ratios.
SWEEP_RATIOS = [0.30, 0.75]
EVALUATION_SEEDS = [1, 2, 3, 4, 5]
TUNING_SEED = 0

FILE_PREFIXES = {"naive": "traffic_mixed", "stealthy": "traffic_stealthy"}

# Two-sided 95% quantiles of Student's t, by degrees of freedom.
T_975 = {
    1: 12.706, 2: 4.303, 3: 3.182, 4: 2.776, 5: 2.571, 6: 2.447, 7: 2.365, 8: 2.306,
    9: 2.262, 10: 2.228, 11: 2.201, 12: 2.179, 13: 2.160, 14: 2.145, 15: 2.131,
    16: 2.120, 17: 2.110, 18: 2.101, 19: 2.093, 20: 2.086, 21: 2.080, 22: 2.074,
    23: 2.069, 24: 2.064, 25: 2.060, 26: 2.056, 27: 2.052, 28: 2.048, 29: 2.045,
    30: 2.042,
}


class HarnessError(Exception):
    """A refusal or misconfiguration the user has to fix."""


def ratio_pct(ratio: float) -> int:
    """The ratio as a whole percentage, as in the generator's file names."""
    return round(ratio * 100)


@dataclass(frozen=True, order=True)
class Cell:
    """One replay_bench run."""

    seed: int
    workload: str
    ratio: float
    algorithm: str
    capacity: int

    @property
    def name(self) -> str:
        return f"{self.seed}_{self.workload}_{ratio_pct(self.ratio)}_{self.algorithm}_{self.capacity}"


def log_path(logs_dir: Path, seed: int, workload: str, ratio: float) -> Path:
    """Where the generator writes the log for one (seed, workload, ratio)."""
    return logs_dir / f"seed_{seed}" / f"{FILE_PREFIXES[workload]}_{ratio_pct(ratio)}pct_bot.ndjson"


def plan_cells(seeds, workloads, ratios, algorithms, capacities, profile,
               primary_capacity=PRIMARY_CAPACITY) -> list[Cell]:
    """The cells of the grid, sorted.

    `full` is the whole product. `thesis` sweeps `capacities` only for the naive workload
    at the `SWEEP_RATIOS`, and runs every other (workload, ratio) at `primary_capacity`.
    """
    cells = set()
    for seed in seeds:
        for workload in workloads:
            for ratio in ratios:
                sweep = profile == "full" or (
                    workload == "naive" and any(math.isclose(ratio, r) for r in SWEEP_RATIOS)
                )
                for capacity in capacities if sweep else [primary_capacity]:
                    for algorithm in algorithms:
                        cells.add(Cell(seed, workload, ratio, algorithm, capacity))
    return sorted(cells)


def check_tuned(seeds, results_dir: Path) -> None:
    """Refuses evaluation seeds until calibration has written `tuning.md`."""
    evaluation = [s for s in seeds if s != TUNING_SEED]
    if evaluation and not (results_dir / "tuning.md").exists():
        raise HarnessError(
            f"{results_dir / 'tuning.md'} does not exist: calibrate on seed {TUNING_SEED} "
            f"first (`run_experiments.py tune`) before running evaluation seeds {evaluation}"
        )


def raw_path(raw_dir: Path, cell: Cell) -> Path:
    return raw_dir / f"{cell.name}.json"


def pending_cells(cells, raw_dir: Path) -> list[Cell]:
    """The cells without a result file yet."""
    return [c for c in cells if not raw_path(raw_dir, c).exists()]


def ensure_logs(seed: int, workloads, ratios, logs_dir: Path, python: str = sys.executable) -> None:
    """Generates the seed's logs for `workloads` x `ratios` unless all of them exist."""
    missing = [
        w for w in workloads
        if not all(log_path(logs_dir, seed, w, r).exists() for r in ratios)
    ]
    if not missing:
        return
    workload = missing[0] if len(missing) == 1 else "both"
    cmd = [
        python, str(GENERATOR), "--seed", str(seed),
        "--out-dir", str(logs_dir / f"seed_{seed}"),
        "--workload", workload, "--ratios", *[str(r) for r in ratios],
    ]
    print(f"Generating seed {seed} logs: {' '.join(cmd[1:])}", flush=True)
    subprocess.run(cmd, check=True, cwd=REPO)


def bench_command(cell: Cell, logs_dir: Path, json_out: Path, extra_args=(),
                  bench: Path = BENCH) -> list[str]:
    return [
        str(bench), "--quiet",
        "--file", str(log_path(logs_dir, cell.seed, cell.workload, cell.ratio)),
        "--capacity", str(cell.capacity),
        "--algorithm", cell.algorithm,
        "--asn-db", str(ASN_DB),
        "--json-out", str(json_out),
        *extra_args,
    ]


def run_cell(cell: Cell, out: Path, logs_dir: Path, extra_args=(), bench: Path = BENCH) -> float:
    """Runs one cell into `out`, atomically, and returns its wall time in seconds."""
    out.parent.mkdir(parents=True, exist_ok=True)
    partial = out.with_suffix(".json.partial")
    cmd = bench_command(cell, logs_dir, partial, extra_args, bench)
    start = time.monotonic()
    proc = subprocess.run(cmd, cwd=REPO, capture_output=True, text=True)
    if proc.returncode != 0:
        partial.unlink(missing_ok=True)
        raise HarnessError(
            f"replay_bench failed for {cell.name} (exit {proc.returncode}):\n{proc.stderr[-2000:]}"
        )
    os.replace(partial, out)
    return time.monotonic() - start


def build_bench() -> None:
    print("Building replay_bench (release)...", flush=True)
    subprocess.run(["cargo", "build", "--release", "--bin", "replay_bench"], check=True, cwd=REPO)


def run_cells(cells, raw_dir: Path, logs_dir: Path, extra_args=(), bench: Path = BENCH) -> None:
    """Runs every pending cell, timing the first to print an estimate for the rest."""
    todo = pending_cells(cells, raw_dir)
    skipped = len(cells) - len(todo)
    print(f"{len(cells)} cells: {skipped} already done, {len(todo)} to run.", flush=True)
    if not todo:
        return
    # Time a primary-capacity cell first: it is the typical run.
    todo.sort(key=lambda c: (c.capacity != PRIMARY_CAPACITY, c))
    for i, cell in enumerate(todo, 1):
        elapsed = run_cell(cell, raw_path(raw_dir, cell), logs_dir, extra_args, bench)
        if i == 1:
            print(
                f"First run took {elapsed:.1f} s; estimated total "
                f"{elapsed * len(todo) / 60:.1f} min for {len(todo)} runs.",
                flush=True,
            )
        print(f"[{i}/{len(todo)}] {cell.name} ({elapsed:.1f} s)", flush=True)


# --- Aggregation ---------------------------------------------------------------------------

def _get(d, *path):
    """`d[path[0]][path[1]]...`, or None if any step is missing or null."""
    for key in path:
        if d is None:
            return None
        d = d.get(key)
    return d


# Metric name -> path into the --json-out document's `results`.
METRICS = {
    "human_hit_rate": ("human", "hit_rate"),
    "overall_hit_rate": ("overall", "hit_rate"),
    "human_byte_hit_rate": ("human", "byte_hit_rate"),
    "overall_byte_hit_rate": ("overall", "byte_hit_rate"),
    "bot_hit_rate": ("bot", "hit_rate"),
    "cpi": ("pollution", "cpi_mean"),
    "bot_triggered_share": ("pollution", "bot_triggered_share_mean"),
    "t_proxy_p99_us": ("latency", "proxy", "p99_us"),
    "t_total_p99_us": ("latency", "total", "p99_us"),
    "rss_delta_bytes": ("memory", "rss_delta_bytes"),
    "logical_resident_bytes": ("memory", "logical_resident_bytes"),
    "main_entries": ("memory", "main_entries"),
    "ghost_entries": ("memory", "ghost_entries"),
    "shards": ("memory", "shards"),
}

# Metrics the paired `dtac - X` differences are reported for.
PAIRED_METRICS = [
    "human_hit_rate", "overall_hit_rate", "human_byte_hit_rate", "bot_hit_rate", "cpi",
]

ROUTER_METRICS = {
    "roc_auc": ("classifier", "roc_auc"),
    "mean_gap": ("classifier", "mean_gap"),
    "fpr_new_visitors": ("classifier", "fpr_new_visitors"),
    "fpr_returning_visitors": ("classifier", "fpr_returning_visitors"),
    "recall": ("classifier", "recall"),
}


@dataclass
class Record:
    """One cell's result file."""

    cell: Cell
    doc: dict

    def metric(self, name: str):
        path = METRICS.get(name) or ROUTER_METRICS[name]
        return _get(self.doc["results"], *path)


def record_from_doc(doc: dict) -> Record:
    run = doc["run"]
    meta = run.get("log_meta") or {}
    if "seed" not in meta or "workload" not in meta or "ratio" not in meta:
        raise HarnessError(f"{run.get('file')}: the log's .meta.json has no seed/workload/ratio")
    cell = Cell(meta["seed"], meta["workload"], meta["ratio"], run["algorithm"], run["capacity"])
    return Record(cell, doc)


def load_records(raw_dir: Path) -> list[Record]:
    records = []
    for path in sorted(raw_dir.glob("*.json")):
        with path.open(encoding="utf-8") as f:
            records.append(record_from_doc(json.load(f)))
    return records


def _canonical(value) -> str:
    return json.dumps(value, sort_keys=True)


def check_consistent(records, allow_mixed: bool = False) -> dict:
    """Refuses records from different commits or configs unless `allow_mixed`.

    Returns what they share: `{"git_commit", "router_config", "engine_config"}`.
    """
    variants = {
        "git_commit": {r.doc["run"].get("git_commit") for r in records},
        "router_config": {
            _canonical(r.doc["run"]["router_config"]) for r in records
            if r.doc["run"].get("router_config") is not None
        },
        "engine_config": {
            _canonical(r.doc["run"]["engine_config"]) for r in records
            if r.doc["run"].get("engine_config") is not None
        },
    }
    mixed = {k: v for k, v in variants.items() if len(v) > 1}
    if mixed and not allow_mixed:
        details = "; ".join(f"{k}: {sorted(map(str, v))}" for k, v in mixed.items())
        raise HarnessError(
            f"results mix different {', '.join(mixed)} ({details}); rerun the stale cells "
            "or pass --allow-mixed"
        )
    shared = {}
    for key, values in variants.items():
        if len(values) == 1:
            (value,) = values
            shared[key] = value if key == "git_commit" else json.loads(value)
        else:
            shared[key] = "mixed" if values else None
    return shared


def mean_ci(values):
    """`(n, mean, std, ci95_half_width)`; std and the CI are None for fewer than 2 values."""
    values = [v for v in values if v is not None]
    n = len(values)
    if n == 0:
        return 0, None, None, None
    mean = statistics.fmean(values)
    if n < 2:
        return n, mean, None, None
    std = statistics.stdev(values)
    t = T_975.get(n - 1, 1.960)
    return n, mean, std, t * std / math.sqrt(n)


def _group(records, key):
    groups = defaultdict(list)
    for r in records:
        groups[key(r)].append(r)
    return groups


def cell_stats(records):
    """Rows of per-(workload, ratio, capacity, algorithm) statistics for every metric."""
    rows = []
    groups = _group(records, lambda r: (r.cell.workload, r.cell.ratio, r.cell.capacity, r.cell.algorithm))
    for (workload, ratio, capacity, algorithm), group in sorted(groups.items()):
        for metric in METRICS:
            n, mean, std, ci = mean_ci(r.metric(metric) for r in group)
            if n:
                rows.append({
                    "kind": "cell", "workload": workload, "ratio": ratio, "capacity": capacity,
                    "algorithm": algorithm, "baseline": "", "metric": metric,
                    "n": n, "mean": mean, "std": std, "ci95": ci,
                })
    return rows


def paired_differences(records, metrics=PAIRED_METRICS, reference="dtac"):
    """Rows of `metric(reference) - metric(X)` per seed, summarized over seeds.

    Every algorithm sees the same human stream for a given seed, so the per-seed
    difference removes the between-seed variance that two separate CIs would include.
    """
    rows = []
    by_point = _group(records, lambda r: (r.cell.workload, r.cell.ratio, r.cell.capacity))
    for (workload, ratio, capacity), group in sorted(by_point.items()):
        by_algo_seed = {(r.cell.algorithm, r.cell.seed): r for r in group}
        algorithms = sorted({r.cell.algorithm for r in group} - {reference})
        for other in algorithms:
            for metric in metrics:
                diffs = []
                for (algo, seed), rec in by_algo_seed.items():
                    if algo != reference or (other, seed) not in by_algo_seed:
                        continue
                    mine, theirs = rec.metric(metric), by_algo_seed[(other, seed)].metric(metric)
                    if mine is not None and theirs is not None:
                        diffs.append(mine - theirs)
                n, mean, std, ci = mean_ci(diffs)
                if n:
                    rows.append({
                        "kind": "paired", "workload": workload, "ratio": ratio,
                        "capacity": capacity, "algorithm": reference, "baseline": other,
                        "metric": metric, "n": n, "mean": mean, "std": std, "ci95": ci,
                    })
    return rows


def router_quality(records, primary_capacity=PRIMARY_CAPACITY):
    """Rows of the router's classifier metrics per (workload, ratio), from the `dtac` runs.

    The router doesn't depend on the algorithm or the capacity, so only one capacity is
    used: the primary one if present, else the smallest.
    """
    rows = []
    dtac = [r for r in records if r.cell.algorithm == "dtac"]
    for (workload, ratio), group in sorted(_group(dtac, lambda r: (r.cell.workload, r.cell.ratio)).items()):
        capacities = {r.cell.capacity for r in group}
        capacity = primary_capacity if primary_capacity in capacities else min(capacities)
        group = [r for r in group if r.cell.capacity == capacity]
        for metric in ROUTER_METRICS:
            n, mean, std, ci = mean_ci(r.metric(metric) for r in group)
            if n:
                rows.append({
                    "kind": "router", "workload": workload, "ratio": ratio, "capacity": capacity,
                    "algorithm": "dtac", "baseline": "", "metric": metric,
                    "n": n, "mean": mean, "std": std, "ci95": ci,
                })
    return rows


@dataclass
class Summary:
    rows: list
    shared: dict
    seeds: list


def aggregate(records, allow_mixed: bool = False, primary_capacity=PRIMARY_CAPACITY) -> Summary:
    """Summarizes evaluation records. Refuses seed 0 and (by default) mixed commits/configs."""
    if not records:
        raise HarnessError("no results to aggregate")
    tuning = sorted({r.cell.name for r in records if r.cell.seed == TUNING_SEED})
    if tuning:
        raise HarnessError(
            f"seed {TUNING_SEED} is the calibration seed and is never part of the summary: "
            f"{', '.join(tuning[:5])}{' ...' if len(tuning) > 5 else ''}"
        )
    shared = check_consistent(records, allow_mixed)
    rows = cell_stats(records) + paired_differences(records) + router_quality(records, primary_capacity)
    return Summary(rows, shared, sorted({r.cell.seed for r in records}))


CSV_FIELDS = ["kind", "workload", "ratio", "capacity", "algorithm", "baseline", "metric",
              "n", "mean", "std", "ci95", "ci95_low", "ci95_high"]


def summary_csv(summary: Summary) -> str:
    out = io.StringIO()
    writer = csv.DictWriter(out, fieldnames=CSV_FIELDS, lineterminator="\n")
    writer.writeheader()
    for row in summary.rows:
        ci = row["ci95"]
        writer.writerow({
            **row,
            "ci95_low": None if ci is None else row["mean"] - ci,
            "ci95_high": None if ci is None else row["mean"] + ci,
        })
    return out.getvalue()


# --- Markdown -------------------------------------------------------------------------------

def _index(rows):
    return {(r["kind"], r["workload"], r["ratio"], r["capacity"], r["algorithm"], r["baseline"], r["metric"]): r
            for r in rows}


def _pct(row, digits=2) -> str:
    if row is None:
        return "—"
    if row["ci95"] is None:
        return f"{row['mean'] * 100:.{digits}f}"
    return f"{row['mean'] * 100:.{digits}f} ± {row['ci95'] * 100:.{digits}f}"


def _diff(row) -> str:
    """A paired difference in percentage points, marked as a tie if its CI contains 0."""
    if row is None:
        return "—"
    mean = row["mean"] * 100
    if row["ci95"] is None:
        return f"{mean:+.2f}"
    lo, hi = mean - row["ci95"] * 100, mean + row["ci95"] * 100
    text = f"{mean:+.2f} [{lo:+.2f}, {hi:+.2f}]"
    return f"{text} (tie)" if lo <= 0 <= hi else f"**{text}**"


def _num(row, scale=1.0, digits=1) -> str:
    if row is None or row["mean"] is None:
        return "—"
    if row["ci95"] is None:
        return f"{row['mean'] * scale:.{digits}f}"
    return f"{row['mean'] * scale:.{digits}f} ± {row['ci95'] * scale:.{digits}f}"


def _table(header, rows) -> list[str]:
    lines = ["| " + " | ".join(header) + " |", "|" + "---|" * len(header)]
    lines += ["| " + " | ".join(row) + " |" for row in rows]
    return lines + [""]


def _algorithms_in(rows):
    present = {r["algorithm"] for r in rows if r["kind"] == "cell"}
    return [a for a in ALGORITHMS if a in present] + sorted(present - set(ALGORITHMS))


def summary_markdown(summary: Summary, primary_capacity=PRIMARY_CAPACITY) -> str:
    rows = summary.rows
    idx = _index(rows)
    algorithms = _algorithms_in(rows)
    workloads = [w for w in WORKLOADS if any(r["workload"] == w for r in rows)]
    cell = lambda w, ratio, cap, algo, metric: idx.get(("cell", w, ratio, cap, algo, "", metric))
    ratios_at = lambda w, cap: sorted({r["ratio"] for r in rows if r["kind"] == "cell"
                                       and r["workload"] == w and r["capacity"] == cap})
    n_seeds = len(summary.seeds)

    lines = [
        "# Benchmark summary",
        "",
        "Generated by `scripts/run_experiments.py` from `results/raw/`. Do not edit by hand.",
        "",
        f"- Seeds: {', '.join(map(str, summary.seeds))} (n = {n_seeds} per cell; "
        "± is the 95% CI of the mean, t-distribution)",
        f"- Commit: `{summary.shared.get('git_commit')}`",
        f"- Router config: `{_canonical(summary.shared.get('router_config'))}`",
        f"- Engine config: `{_canonical(summary.shared.get('engine_config'))}`",
        "- Paired differences are `dtac − X` per seed, in percentage points, with the 95% CI "
        "of the difference; **bold** differences exclude 0, \"tie\" ones include it.",
        "",
    ]

    tiers = [
        ("Human hit rate (primary metric), %", "human_hit_rate"),
        ("Overall hit rate (secondary), %", "overall_hit_rate"),
        ("Human byte hit rate (secondary), %", "human_byte_hit_rate"),
        ("Bot hit rate (reference only), %", "bot_hit_rate"),
    ]
    for workload in workloads:
        ratios = ratios_at(workload, primary_capacity)
        if not ratios:
            continue
        lines += [f"## {workload.capitalize()} workload, capacity {primary_capacity:,}", ""]
        for title, metric in tiers:
            lines += [f"### {title}", ""]
            lines += _table(
                ["Bot ratio"] + algorithms,
                [[f"{ratio_pct(r)}%"] + [_pct(cell(workload, r, primary_capacity, a, metric)) for a in algorithms]
                 for r in ratios],
            )
        others = [a for a in algorithms if a != "dtac"]
        for title, metric in [("Paired difference in human hit rate, dtac − X (points)", "human_hit_rate"),
                              ("Paired difference in overall hit rate, dtac − X (points)", "overall_hit_rate")]:
            lines += [f"### {title}", ""]
            lines += _table(
                ["Bot ratio"] + [f"vs {a}" for a in others],
                [[f"{ratio_pct(r)}%"] + [_diff(idx.get(("paired", workload, r, primary_capacity, "dtac", a, metric)))
                                        for a in others]
                 for r in ratios],
            )
        lines += ["### Pollution, latency and metadata overhead", ""]
        overhead_rows = []
        for r in ratios:
            for a in algorithms:
                get = lambda m: cell(workload, r, primary_capacity, a, m)
                overhead_rows.append([
                    f"{ratio_pct(r)}%", a, _pct(get("cpi"), 1),
                    _num(get("t_proxy_p99_us")),
                    _num(get("rss_delta_bytes"), 1 / 2**20),
                    _num(get("logical_resident_bytes"), 1 / 2**20),
                    _num(get("main_entries"), digits=0),
                    _num(get("ghost_entries"), digits=0),
                ])
        lines += _table(
            ["Bot ratio", "Algorithm", "CPI %", "T_proxy p99 µs", "RSS delta MiB",
             "Logical resident MiB", "Main entries", "Ghost entries"],
            overhead_rows,
        )

    # Capacity sweeps: every (workload, ratio) with more than one capacity.
    sweeps = sorted({(r["workload"], r["ratio"]) for r in rows if r["kind"] == "cell"})
    sweep_lines = []
    for workload, ratio in sweeps:
        capacities = sorted({r["capacity"] for r in rows if r["kind"] == "cell"
                             and r["workload"] == workload and r["ratio"] == ratio})
        if len(capacities) < 2:
            continue
        sweep_lines += [f"### {workload.capitalize()}, {ratio_pct(ratio)}% bots: human hit rate (%) vs. capacity", ""]
        table_rows = []
        for cap in capacities:
            shards = next((cell(workload, ratio, cap, a, "shards") for a in algorithms
                           if cell(workload, ratio, cap, a, "shards")), None)
            table_rows.append(
                [f"{cap:,}", _num(shards, digits=0)]
                + [_pct(cell(workload, ratio, cap, a, "human_hit_rate")) for a in algorithms]
                + [_diff(idx.get(("paired", workload, ratio, cap, "dtac", a, "human_hit_rate")))
                   for a in algorithms if a != "dtac"]
            )
        sweep_lines += _table(
            ["Capacity", "Shards"] + algorithms + [f"dtac − {a}" for a in algorithms if a != "dtac"],
            table_rows,
        )
    if sweep_lines:
        lines += ["## Capacity sweeps", ""] + sweep_lines

    router_rows = sorted({(r["workload"], r["ratio"]) for r in rows if r["kind"] == "router"})
    if router_rows:
        lines += ["## Router quality (from the dtac runs)", ""]
        get = lambda w, ratio, m: next((r for r in rows if r["kind"] == "router" and r["workload"] == w
                                        and r["ratio"] == ratio and r["metric"] == m), None)
        lines += _table(
            ["Workload", "Bot ratio", "ROC-AUC", "Class-mean gap", "Human FPR new %",
             "Human FPR returning %", "Bot recall %"],
            [[w, f"{ratio_pct(ratio)}%", _num(get(w, ratio, "roc_auc"), digits=3),
              _num(get(w, ratio, "mean_gap"), digits=3), _pct(get(w, ratio, "fpr_new_visitors")),
              _pct(get(w, ratio, "fpr_returning_visitors")), _pct(get(w, ratio, "recall"))]
             for w, ratio in router_rows],
        )
    return "\n".join(lines)


def write_summary(summary: Summary, results_dir: Path, primary_capacity=PRIMARY_CAPACITY) -> None:
    results_dir.mkdir(parents=True, exist_ok=True)
    (results_dir / "summary.csv").write_text(summary_csv(summary), encoding="utf-8")
    (results_dir / "summary.md").write_text(summary_markdown(summary, primary_capacity), encoding="utf-8")
    print(f"Wrote {results_dir / 'summary.csv'} and {results_dir / 'summary.md'}")


# --- CLI ------------------------------------------------------------------------------------

def add_grid_args(p: argparse.ArgumentParser) -> None:
    p.add_argument("--seeds", type=int, nargs="+", default=EVALUATION_SEEDS)
    p.add_argument("--workloads", nargs="+", choices=WORKLOADS, default=WORKLOADS)
    p.add_argument("--ratios", type=float, nargs="+", default=RATIOS)
    p.add_argument("--algorithms", nargs="+", choices=ALGORITHMS, default=ALGORITHMS)
    p.add_argument("--capacities", type=int, nargs="+", default=CAPACITIES)
    p.add_argument("--primary-capacity", type=int, default=PRIMARY_CAPACITY,
                   help="capacity every (workload, ratio) runs at (default %(default)s)")
    p.add_argument("--profile", choices=["thesis", "full"], default="thesis",
                   help="thesis: sweep capacities only for naive at 30%%/75%%; full: every combination")
    p.add_argument("--quick", action="store_true",
                   help="smoke test: seed 1, ratios 0.3 and 0.75, primary capacity only")


def add_common_args(p: argparse.ArgumentParser) -> None:
    p.add_argument("--results-dir", type=Path, default=RESULTS_DIR)
    p.add_argument("--logs-dir", type=Path, default=LOGS_DIR)
    p.add_argument("--allow-mixed", action="store_true",
                   help="aggregate results from different commits or configs anyway")


def parse_args(argv=None) -> argparse.Namespace:
    argv = list(sys.argv[1:] if argv is None else argv)
    if not argv or argv[0] not in {"run", "aggregate", "-h", "--help"}:
        argv.insert(0, "run")
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = parser.add_subparsers(dest="command", required=True)

    run = sub.add_parser("run", help="run the grid and aggregate it (default)")
    add_grid_args(run)
    add_common_args(run)
    run.add_argument("--dry-run", action="store_true", help="print the cells and exit")
    run.add_argument("--no-build", action="store_true", help="don't run cargo build first")

    agg = sub.add_parser("aggregate", help="aggregate every file in results/raw/")
    add_common_args(agg)
    agg.add_argument("--primary-capacity", type=int, default=PRIMARY_CAPACITY)

    return parser.parse_args(argv)


def grid_from_args(args) -> list[Cell]:
    seeds, ratios, capacities = args.seeds, args.ratios, args.capacities
    if args.quick:
        seeds, ratios, capacities = [1], [0.30, 0.75], [args.primary_capacity]
    return plan_cells(seeds, args.workloads, ratios, args.algorithms, capacities,
                      args.profile, args.primary_capacity)


def cmd_run(args) -> None:
    cells = grid_from_args(args)
    seeds = sorted({c.seed for c in cells})
    check_tuned(seeds, args.results_dir)
    if TUNING_SEED in seeds:
        raise HarnessError(f"seed {TUNING_SEED} is the calibration seed; use `tune` for it")
    if args.dry_run:
        for cell in cells:
            print(cell.name)
        print(f"{len(cells)} cells")
        return
    if not args.no_build:
        build_bench()
    for seed in seeds:
        seed_cells = [c for c in cells if c.seed == seed]
        workloads = sorted({c.workload for c in seed_cells})
        ratios = sorted({c.ratio for c in seed_cells})
        ensure_logs(seed, workloads, ratios, args.logs_dir)
    raw_dir = args.results_dir / "raw"
    run_cells(cells, raw_dir, args.logs_dir)
    records = [r for r in load_records(raw_dir) if r.cell in set(cells)]
    write_summary(aggregate(records, args.allow_mixed, args.primary_capacity), args.results_dir,
                  args.primary_capacity)


def cmd_aggregate(args) -> None:
    records = load_records(args.results_dir / "raw")
    write_summary(aggregate(records, args.allow_mixed, args.primary_capacity), args.results_dir,
                  args.primary_capacity)


def main(argv=None) -> int:
    args = parse_args(argv)
    try:
        {"run": cmd_run, "aggregate": cmd_aggregate}[args.command](args)
    except HarnessError as err:
        print(f"error: {err}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
