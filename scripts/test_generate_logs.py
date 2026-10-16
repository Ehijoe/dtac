"""Unit tests for generate_logs.py: the default traffic model, the CLI and `--legacy`."""

import collections
import contextlib
import hashlib
import io
import ipaddress
import json
import random
import statistics
import tempfile
import unittest
from pathlib import Path

from generate_logs import (
    BOT_IP_POOL,
    SizeModel,
    generate_bot_event,
    generate_dataset,
    generate_human_event,
    main,
    parse_args,
    precompute_zipf_cdf,
    random_public_ipv4,
    random_residential_ipv4,
)

FIXTURE = Path(__file__).resolve().parent.parent / "data" / "asn" / "ip2asn-fixture.tsv"
# Datacenter ASNs in the fixture (Linode, DigitalOcean).
FIXTURE_DATACENTER_ASNS = {"63949", "14061"}
TEST_NETS = [
    ipaddress.ip_network(n) for n in ("192.0.2.0/24", "198.51.100.0/24", "203.0.113.0/24")
]

RATIOS = [0.1, 0.5, 0.75]
BOT_IP_RATE = 3.6
PAGES_MEAN = 5.0
# Small enough to run in a few seconds, large enough that each tenth of the measurement
# window holds a few thousand requests.
SMALL_RUN = [
    "--seed", "7",
    "--catalog-size", "5000",
    "--human-requests", "50000",
    "--warmup-requests", "2000",
    "--ratios", *map(str, RATIOS),
    "--bot-ip-rate", str(BOT_IP_RATE),
    "--pages-per-session-mean", str(PAGES_MEAN),
    "--workload", "both",
]


def run_quietly(argv: list[str]) -> list[Path]:
    """Runs the generator CLI with its progress output suppressed."""
    with contextlib.redirect_stdout(io.StringIO()):
        return main(argv)


def read_events(path: Path) -> list[dict]:
    return [json.loads(line) for line in path.read_text().splitlines()]


def fixture_datacenter_ranges() -> list[tuple[ipaddress.IPv4Address, ipaddress.IPv4Address]]:
    ranges = []
    for line in FIXTURE.read_text().splitlines():
        if line.startswith("#") or not line.strip():
            continue
        start, end, asn, *_ = line.split("\t")
        if asn in FIXTURE_DATACENTER_ASNS and ":" not in start:
            ranges.append((ipaddress.ip_address(start), ipaddress.ip_address(end)))
    return ranges


class TestRandomPublicIpv4(unittest.TestCase):
    def test_format(self):
        for _ in range(200):
            ip = random_public_ipv4()
            octets = ip.split(".")
            self.assertEqual(len(octets), 4)
            for octet in octets:
                self.assertTrue(octet.isdigit())
                self.assertGreaterEqual(int(octet), 0)
                self.assertLessEqual(int(octet), 255)

    def test_avoids_reserved_first_octets(self):
        reserved = {0, 10, 127, 169, 172, 192, 224, 255}
        for _ in range(500):
            first_octet = int(random_public_ipv4().split(".")[0])
            self.assertNotIn(first_octet, reserved)

    def test_residential_avoids_datacenter_and_test_net(self):
        rng = random.Random(3)
        datacenter = fixture_datacenter_ranges()
        for _ in range(20_000):
            addr = ipaddress.ip_address(random_residential_ipv4(rng))
            self.assertFalse(any(lo <= addr <= hi for lo, hi in datacenter))
            self.assertFalse(any(addr in net for net in TEST_NETS))


class TestLegacyEvents(unittest.TestCase):
    def test_primary_and_favicon_share_ip(self):
        # Run enough times to reliably get at least one 2-request result
        # (40% favicon probability per generate_human_event call).
        for _ in range(200):
            requests = generate_human_event(item_id=1, current_timestamp=0.0)
            if len(requests) == 2:
                self.assertEqual(requests[0]["ip"], requests[1]["ip"])
                return
        self.fail("never observed a 2-request human event across 200 trials")

    def test_bot_ip_drawn_from_fixed_pool(self):
        for _ in range(200):
            ip = generate_bot_event(item_id=1, current_timestamp=0.0)[0]["ip"]
            self.assertIn(ip, BOT_IP_POOL)

    def test_warmup_prefix_tagged_and_precedes_measurement(self):
        cdf = precompute_zipf_cdf(100, 0.8)
        with tempfile.TemporaryDirectory() as tmp:
            output_file = Path(tmp) / "test.ndjson"
            with contextlib.redirect_stdout(io.StringIO()):
                generate_dataset(
                    zipf_cdf=cdf,
                    total_requests=20,
                    bot_ratio=0.5,
                    output_file=output_file,
                    warmup_requests=5,
                )
            events = read_events(output_file)

        for event in events:
            if event["phase"] == "warmup":
                self.assertEqual(event["traffic_type"], "human")
        timestamps = [e["timestamp"] for e in events]
        self.assertEqual(timestamps, sorted(timestamps))
        self.assertEqual(events[0]["phase"], "warmup")


class TestLegacyReproducesOriginal(unittest.TestCase):
    # SHA-256 of the files the original generator (before --legacy existed) wrote after
    # `random.seed(1); np.random.seed(1)` with a 100-item catalog, 200 measurement and 50
    # warm-up requests: for ratio 0.5 alone, and for 0.1 then 0.75 in one run.
    GOLDEN = {
        ("0.5",): {
            "traffic_mixed_50pct_bot.ndjson":
                "17fd77a59390c93f549fd4b24f123833a26ec2fdec38a7b73bc3e58621744b0b",
        },
        ("0.1", "0.75"): {
            "traffic_mixed_10pct_bot.ndjson":
                "fea4fe23552010d1fffd4c7520d47cd51d23f23c7e1542f0ed1be5f3848ca016",
            "traffic_mixed_75pct_bot.ndjson":
                "4fd1e01df83d0b6600007c10ce3072f8d9a0431405e5ad007623ead8c539ccda",
        },
    }

    def test_byte_for_byte(self):
        for ratios, expected in self.GOLDEN.items():
            with tempfile.TemporaryDirectory() as tmp:
                run_quietly([
                    "--legacy", "--seed", "1", "--catalog-size", "100",
                    "--total-requests", "200", "--warmup-requests", "50",
                    "--ratios", *ratios, "--out-dir", tmp,
                ])
                for name, digest in expected.items():
                    data = (Path(tmp) / name).read_bytes()
                    self.assertEqual(hashlib.sha256(data).hexdigest(), digest, name)


class TestSizeModel(unittest.TestCase):
    def test_stable_per_item_and_within_bounds(self):
        a = SizeModel(16 * 1024, 1.0, 512, 2 * 1024 * 1024, 1024)
        b = SizeModel(16 * 1024, 1.0, 512, 2 * 1024 * 1024, 1024)
        sizes = [a.item(i) for i in range(1, 5001)]
        self.assertEqual(sizes, [b.item(i) for i in range(1, 5001)])
        self.assertTrue(all(512 <= s <= 2 * 1024 * 1024 for s in sizes))
        # Median about 16 KB.
        self.assertLess(abs(statistics.median(sizes) / (16 * 1024) - 1.0), 0.1)


class TestDefaultModel(unittest.TestCase):
    """Checks one small generated run of every (workload, ratio) file."""

    @classmethod
    def setUpClass(cls):
        cls._tmp = tempfile.TemporaryDirectory()
        cls.out = Path(cls._tmp.name)
        cls.argv = [*SMALL_RUN, "--out-dir", str(cls.out)]
        run_quietly(cls.argv)
        cls.files = {}
        for workload, prefix in (("naive", "traffic_mixed"), ("stealthy", "traffic_stealthy")):
            for ratio in RATIOS:
                path = cls.out / f"{prefix}_{round(ratio * 100)}pct_bot.ndjson"
                cls.files[(workload, ratio)] = (path, read_events(path))

    @classmethod
    def tearDownClass(cls):
        cls._tmp.cleanup()

    def meta(self, workload, ratio):
        path, _ = self.files[(workload, ratio)]
        return json.loads(path.with_suffix(".meta.json").read_text())

    def test_same_seed_gives_identical_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            run_quietly([*SMALL_RUN, "--out-dir", tmp])
            for path, _ in self.files.values():
                self.assertEqual(path.read_bytes(), (Path(tmp) / path.name).read_bytes())

    def test_every_record_has_a_stable_size(self):
        sizes = {}
        for _, events in self.files.values():
            for e in events:
                self.assertGreaterEqual(e["size_bytes"], 512 if e["path"] != "/favicon.ico" else 1)
                self.assertLessEqual(e["size_bytes"], 2 * 1024 * 1024)
                self.assertEqual(sizes.setdefault(e["path"], e["size_bytes"]), e["size_bytes"])
        self.assertEqual(sizes["/favicon.ico"], 1024)

    def test_human_records_identical_across_files(self):
        streams = [
            [e for e in events if e["traffic_type"] == "human"]
            for _, events in self.files.values()
        ]
        for stream in streams[1:]:
            self.assertEqual(stream, streams[0])

    def test_bot_share_is_stationary(self):
        for (workload, ratio), (_, events) in self.files.items():
            derived = self.meta(workload, ratio)["derived"]
            start, end = derived["warmup_end_s"], derived["end_s"]
            humans, bots = [0] * 10, [0] * 10
            for e in events:
                if e["phase"] != "measurement":
                    continue
                tenth = min(9, int((e["timestamp"] - start) / (end - start) * 10))
                (bots if e["traffic_type"] == "bot" else humans)[tenth] += 1
            for tenth in range(10):
                share = bots[tenth] / (bots[tenth] + humans[tenth])
                self.assertLess(
                    abs(share - ratio), 0.03, f"{workload} {ratio} tenth {tenth}: {share:.3f}"
                )

    def test_naive_bot_ip_rate_is_constant_across_ratios(self):
        pool_sizes = []
        for ratio in RATIOS:
            _, events = self.files[("naive", ratio)]
            times = collections.defaultdict(list)
            for e in events:
                if e["traffic_type"] == "bot":
                    times[e["ip"]].append(e["timestamp"])
            pool_sizes.append(len(times))
            for ip, ts in times.items():
                gap = statistics.median(b - a for a, b in zip(ts, ts[1:]))
                self.assertLess(abs(gap * BOT_IP_RATE - 1.0), 0.05, f"{ratio} {ip}")
            self.assertEqual(len(times), self.meta("naive", ratio)["derived"]["bot_ip_pool_size"])
        # More bot traffic means more bot IPs, not faster ones.
        self.assertEqual(pool_sizes, sorted(pool_sizes))
        self.assertLess(pool_sizes[0], pool_sizes[-1])

    def test_naive_bot_ips_are_datacenter_or_test_net(self):
        datacenter = fixture_datacenter_ranges()
        _, events = self.files[("naive", 0.75)]
        ips = {ipaddress.ip_address(e["ip"]) for e in events if e["traffic_type"] == "bot"}
        in_dc = {ip for ip in ips if any(lo <= ip <= hi for lo, hi in datacenter)}
        in_test_net = {ip for ip in ips if any(ip in net for net in TEST_NETS)}
        self.assertEqual(in_dc | in_test_net, ips)
        self.assertLess(abs(len(in_dc) / len(ips) - 9 / 14), 0.15)

    def test_stealthy_bots_look_like_browsers(self):
        for ratio in RATIOS:
            _, events = self.files[("stealthy", ratio)]
            bots = [e for e in events if e["traffic_type"] == "bot"]
            self.assertTrue(all(e["bot_kind"] == "stealthy" for e in bots))
            self.assertTrue(all("Accept-Language" in e["headers"] for e in bots))
            favicons = collections.Counter(e["ip"] for e in bots if e["path"] == "/favicon.ico")
            self.assertLessEqual(max(favicons.values()), 1)
            # About 5 requests per IP, so many distinct IPs.
            ips = {e["ip"] for e in bots}
            self.assertGreater(len(ips), len(bots) / 10)

    def test_session_model(self):
        _, events = self.files[("naive", 0.1)]
        end = self.meta("naive", 0.1)["derived"]["end_s"]
        sessions = collections.defaultdict(list)
        for e in events:
            if e["traffic_type"] == "human":
                sessions[e["ip"]].append(e)

        complete = [s for s in sessions.values() if s[0]["timestamp"] < end - 600]
        pages = [sum(1 for e in s if e["path"] != "/favicon.ico") for s in complete]
        self.assertLess(abs(statistics.mean(pages) / PAGES_MEAN - 1.0), 0.05)
        for session in complete:
            favicons = sum(1 for e in session if e["path"] == "/favicon.ico")
            self.assertEqual(favicons, 1 if session[0]["visitor"] == "new" else 0)
        returning = sum(1 for s in complete if s[0]["visitor"] == "returning")
        self.assertLess(abs(returning / len(complete) - 0.3), 0.03)

    def test_human_ips_avoid_bot_ranges(self):
        datacenter = fixture_datacenter_ranges()
        _, events = self.files[("naive", 0.1)]
        for ip in {e["ip"] for e in events if e["traffic_type"] == "human"}:
            addr = ipaddress.ip_address(ip)
            self.assertFalse(any(lo <= addr <= hi for lo, hi in datacenter), ip)
            self.assertFalse(any(addr in net for net in TEST_NETS), ip)

    def test_meta_round_trips_the_cli_arguments(self):
        expected = parse_args(self.argv)
        for workload, ratio in self.files:
            meta = self.meta(workload, ratio)
            self.assertEqual(meta["seed"], 7)
            self.assertEqual((meta["workload"], meta["ratio"]), (workload, ratio))
            args = meta["args"]
            self.assertEqual(Path(args.pop("out_dir")), expected.out_dir)
            for key, value in args.items():
                self.assertEqual(value, getattr(expected, key), key)
            self.assertEqual(parse_args(self.argv_from(meta["args"])).seed, expected.seed)

    @staticmethod
    def argv_from(args: dict) -> list[str]:
        argv = []
        for key, value in args.items():
            flag = "--" + key.replace("_", "-")
            if isinstance(value, bool):
                argv += [flag] if value else []
            elif isinstance(value, list):
                argv += [flag, *map(str, value)]
            else:
                argv += [flag, str(value)]
        return argv


class TestNat(unittest.TestCase):
    def test_nat_sessions_share_a_small_ip_set(self):
        with tempfile.TemporaryDirectory() as tmp:
            run_quietly([
                "--seed", "2", "--catalog-size", "500", "--human-requests", "5000",
                "--warmup-requests", "500", "--ratios", "0.1", "--workload", "naive",
                "--nat-frac", "0.5", "--nat-ips", "4", "--out-dir", tmp,
            ])
            events = read_events(Path(tmp) / "traffic_mixed_10pct_bot.ndjson")
        per_ip = collections.Counter(e["ip"] for e in events if e["traffic_type"] == "human")
        busiest = per_ip.most_common(4)
        # Half of all sessions land on four IPs.
        self.assertGreater(sum(n for _, n in busiest), 0.4 * sum(per_ip.values()))


if __name__ == "__main__":
    unittest.main()
