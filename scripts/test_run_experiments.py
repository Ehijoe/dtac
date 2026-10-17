"""Unit tests for run_experiments.py: the grid, resume, refusals and the aggregation."""

import json
import math
import stat
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

from run_experiments import (
    Cell,
    HarnessError,
    aggregate,
    check_consistent,
    check_tuned,
    load_records,
    mean_ci,
    paired_differences,
    pending_cells,
    plan_cells,
    raw_path,
    record_from_doc,
    run_cells,
    summary_csv,
    summary_markdown,
    ALGORITHMS,
    CAPACITIES,
    EVALUATION_SEEDS,
    RATIOS,
    WORKLOADS,
)


def fake_doc(seed, workload, ratio, algorithm, capacity, human_hit_rate, *,
             commit="abc123", router_config=None, auc=1.0):
    """A minimal --json-out document."""
    has_router = algorithm in ("dtac", "dtac-no-score")
    return {
        "run": {
            "file": f"seed_{seed}/x.ndjson",
            "algorithm": algorithm,
            "capacity": capacity,
            "shards": 8,
            "router_config": (router_config or {"prior_bot_rate": 0.1}) if has_router else None,
            "engine_config": {"n_samples": 5} if has_router else None,
            "log_meta": {"seed": seed, "workload": workload, "ratio": ratio},
            "git_commit": commit,
        },
        "results": {
            "human": {"hit_rate": human_hit_rate, "byte_hit_rate": human_hit_rate / 2},
            "overall": {"hit_rate": human_hit_rate / 2, "byte_hit_rate": 0.1},
            "bot": {"hit_rate": 0.01},
            "latency": {"proxy": {"p99_us": 30.0} if algorithm == "dtac" else None,
                        "total": {"p99_us": 100.0}},
            "classifier": {"roc_auc": auc, "mean_gap": 0.5, "fpr_new_visitors": 0.0,
                           "fpr_returning_visitors": 0.01, "recall": 0.99}
            if algorithm == "dtac" else None,
            "pollution": {"cpi_mean": 0.2, "bot_triggered_share_mean": 0.3},
            "memory": {"rss_delta_bytes": 2**20, "logical_resident_bytes": 2**21,
                       "main_entries": capacity, "ghost_entries": None, "shards": 8},
        },
    }


def records(docs):
    return [record_from_doc(d) for d in docs]


def row(rows, **match):
    found = [r for r in rows if all(r[k] == v for k, v in match.items())]
    assert len(found) == 1, (match, found)
    return found[0]


class GridTest(unittest.TestCase):
    def test_thesis_profile_has_350_cells_and_full_has_800(self):
        args = (EVALUATION_SEEDS, WORKLOADS, RATIOS, ALGORITHMS, CAPACITIES)
        thesis = plan_cells(*args, "thesis")
        self.assertEqual(len(thesis), 350)
        self.assertEqual(len(plan_cells(*args, "full")), 800)
        # Only naive at 30% and 75% is swept.
        swept = {(c.workload, c.ratio) for c in thesis if c.capacity != 10_000}
        self.assertEqual(swept, {("naive", 0.30), ("naive", 0.75)})

    def test_cell_names_match_the_raw_layout(self):
        cell = Cell(3, "stealthy", 0.75, "dtac-no-score", 10_000)
        self.assertEqual(cell.name, "3_stealthy_75_dtac-no-score_10000")


class StatsTest(unittest.TestCase):
    def test_mean_and_t_interval(self):
        n, mean, std, ci = mean_ci([1, 2, 3, 4, 5])
        self.assertEqual((n, mean), (5, 3))
        self.assertAlmostEqual(std, math.sqrt(2.5))
        # t(0.975, df=4) = 2.776
        self.assertAlmostEqual(ci, 2.776 * math.sqrt(2.5) / math.sqrt(5))

    def test_a_single_value_has_no_interval(self):
        self.assertEqual(mean_ci([0.4]), (1, 0.4, None, None))
        self.assertEqual(mean_ci([None]), (0, None, None, None))

    def test_aggregation_of_fixed_results(self):
        docs = [fake_doc(s, "naive", 0.3, "dtac", 10_000, v)
                for s, v in [(1, 0.50), (2, 0.60), (3, 0.70)]]
        summary = aggregate(records(docs))
        r = row(summary.rows, kind="cell", algorithm="dtac", metric="human_hit_rate")
        self.assertEqual(r["n"], 3)
        self.assertAlmostEqual(r["mean"], 0.60)
        self.assertAlmostEqual(r["std"], 0.10)
        self.assertAlmostEqual(r["ci95"], 4.303 * 0.10 / math.sqrt(3))
        router = row(summary.rows, kind="router", metric="roc_auc")
        self.assertAlmostEqual(router["mean"], 1.0)
        self.assertEqual(summary.seeds, [1, 2, 3])

    def test_paired_differences_cancel_the_shared_seed_effect(self):
        # The seeds differ a lot, but dtac is 0.10 ahead of lru on every one: the paired
        # CI is tight even though the per-algorithm CIs overlap.
        dtac = [0.30, 0.50, 0.70]
        lru = [0.21, 0.40, 0.59]
        docs = [fake_doc(s, "naive", 0.75, "dtac", 10_000, d) for s, d in zip((1, 2, 3), dtac)]
        docs += [fake_doc(s, "naive", 0.75, "lru", 10_000, v) for s, v in zip((1, 2, 3), lru)]
        rows = paired_differences(records(docs))
        r = row(rows, baseline="lru", metric="human_hit_rate")
        diffs = [0.09, 0.10, 0.11]
        self.assertAlmostEqual(r["mean"], 0.10)
        self.assertAlmostEqual(r["ci95"], 4.303 * 0.01 / math.sqrt(3))
        self.assertEqual(r["n"], len(diffs))
        dtac_ci = mean_ci(dtac)[3]
        self.assertGreater(dtac_ci, 10 * r["ci95"])

    def test_paired_differences_skip_seeds_missing_on_one_side(self):
        docs = [fake_doc(1, "naive", 0.3, "dtac", 10_000, 0.5),
                fake_doc(2, "naive", 0.3, "dtac", 10_000, 0.6),
                fake_doc(1, "naive", 0.3, "sieve", 10_000, 0.4)]
        r = row(paired_differences(records(docs)), baseline="sieve", metric="human_hit_rate")
        self.assertEqual(r["n"], 1)
        self.assertAlmostEqual(r["mean"], 0.1)

    def test_csv_and_markdown_render(self):
        docs = []
        for s in (1, 2):
            for a, v in [("dtac", 0.6), ("lru", 0.5)]:
                for cap in (1_000, 10_000):
                    docs.append(fake_doc(s, "naive", 0.75, a, cap, v + 0.01 * s))
        summary = aggregate(records(docs))
        csv_text = summary_csv(summary)
        self.assertTrue(csv_text.startswith("kind,workload,ratio,capacity,algorithm"))
        md = summary_markdown(summary)
        self.assertIn("## Naive workload, capacity 10,000", md)
        self.assertIn("Capacity sweeps", md)
        self.assertIn("+10.00 [+10.00, +10.00]", md)


class RefusalTest(unittest.TestCase):
    def test_seed_zero_is_never_aggregated(self):
        docs = [fake_doc(0, "naive", 0.3, "dtac", 10_000, 0.5),
                fake_doc(1, "naive", 0.3, "dtac", 10_000, 0.5)]
        with self.assertRaisesRegex(HarnessError, "calibration seed"):
            aggregate(records(docs))

    def test_mixed_commits_are_refused_unless_allowed(self):
        docs = [fake_doc(1, "naive", 0.3, "dtac", 10_000, 0.5, commit="aaa"),
                fake_doc(2, "naive", 0.3, "dtac", 10_000, 0.5, commit="bbb")]
        with self.assertRaisesRegex(HarnessError, "git_commit"):
            aggregate(records(docs))
        summary = aggregate(records(docs), allow_mixed=True)
        self.assertEqual(summary.shared["git_commit"], "mixed")

    def test_mixed_router_configs_are_refused(self):
        docs = [fake_doc(1, "naive", 0.3, "dtac", 10_000, 0.5, router_config={"w_hdr": 1}),
                fake_doc(2, "naive", 0.3, "dtac", 10_000, 0.5, router_config={"w_hdr": 2})]
        with self.assertRaisesRegex(HarnessError, "router_config"):
            check_consistent(records(docs))

    def test_baselines_without_configs_do_not_count_as_mixed(self):
        docs = [fake_doc(1, "naive", 0.3, "dtac", 10_000, 0.5),
                fake_doc(1, "naive", 0.3, "lru", 10_000, 0.4)]
        shared = check_consistent(records(docs))
        self.assertEqual(shared["router_config"], {"prior_bot_rate": 0.1})

    def test_evaluation_seeds_need_tuning_md(self):
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            check_tuned([0], results)  # calibration itself is allowed
            with self.assertRaisesRegex(HarnessError, "tuning.md"):
                check_tuned([1, 2], results)
            (results / "tuning.md").write_text("calibrated\n")
            check_tuned([1, 2], results)


class ResumeTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.raw = self.root / "raw"
        self.calls = self.root / "calls.txt"
        # A stand-in for replay_bench: logs its call and writes a result for its cell.
        self.bench = self.root / "fake_bench.py"
        self.bench.write_text(textwrap.dedent(f"""\
            #!{sys.executable}
            import json, sys
            args = sys.argv[1:]
            value = lambda flag: args[args.index(flag) + 1]
            with open({str(self.calls)!r}, "a") as f:
                f.write(value("--algorithm") + "\\n")
            with open(value("--json-out"), "w") as f:
                json.dump({{"ok": True}}, f)
            """))
        self.bench.chmod(self.bench.stat().st_mode | stat.S_IEXEC)

    def tearDown(self):
        self.tmp.cleanup()

    def test_existing_cells_are_skipped(self):
        cells = [Cell(1, "naive", 0.3, a, 10_000) for a in ("dtac", "lru", "sieve")]
        self.raw.mkdir()
        raw_path(self.raw, cells[1]).write_text("{}")
        self.assertEqual(pending_cells(cells, self.raw), [cells[0], cells[2]])

        run_cells(cells, self.raw, self.root, bench=self.bench)
        self.assertEqual(sorted(self.calls.read_text().split()), ["dtac", "sieve"])
        self.assertEqual(pending_cells(cells, self.raw), [])
        self.assertFalse(list(self.raw.glob("*.partial")))

        # A second run does nothing.
        run_cells(cells, self.raw, self.root, bench=self.bench)
        self.assertEqual(len(self.calls.read_text().split()), 2)

    def test_load_records_reads_the_cell_from_the_document(self):
        self.raw.mkdir()
        doc = fake_doc(4, "stealthy", 0.5, "s3fifo", 5_000, 0.3)
        (self.raw / "anything.json").write_text(json.dumps(doc))
        (rec,) = load_records(self.raw)
        self.assertEqual(rec.cell, Cell(4, "stealthy", 0.5, "s3fifo", 5_000))
        self.assertAlmostEqual(rec.metric("human_hit_rate"), 0.3)


if __name__ == "__main__":
    unittest.main()
