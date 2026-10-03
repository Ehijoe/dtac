"""Unit tests for generate_logs.py's synthetic IP assignment and warm-up tagging."""

import json
import tempfile
import unittest
from pathlib import Path

from generate_logs import (
    BOT_IP_POOL,
    generate_bot_event,
    generate_dataset,
    generate_human_event,
    precompute_zipf_cdf,
    random_public_ipv4,
)


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


class TestHumanEventIps(unittest.TestCase):
    def test_primary_and_favicon_share_ip(self):
        # Run enough times to reliably get at least one 2-request result
        # (40% favicon probability per generate_human_event call).
        for _ in range(200):
            requests = generate_human_event(item_id=1, current_timestamp=0.0)
            if len(requests) == 2:
                self.assertEqual(requests[0]["ip"], requests[1]["ip"])
                return
        self.fail("never observed a 2-request human event across 200 trials")

    def test_high_ip_cardinality_across_sessions(self):
        n = 500
        ips = {generate_human_event(item_id=i, current_timestamp=0.0)[0]["ip"] for i in range(n)}
        self.assertGreaterEqual(len(ips), 0.9 * n)


class TestBotEventIps(unittest.TestCase):
    def test_ip_drawn_from_fixed_pool(self):
        for _ in range(200):
            ip = generate_bot_event(item_id=1, current_timestamp=0.0)[0]["ip"]
            self.assertIn(ip, BOT_IP_POOL)

    def test_pool_is_reused_not_one_ip_per_request(self):
        n = 200
        ips = {generate_bot_event(item_id=i, current_timestamp=0.0)[0]["ip"] for i in range(n)}
        self.assertLess(len(ips), n)
        self.assertLessEqual(len(ips), len(BOT_IP_POOL))


class TestGenerateDatasetWarmup(unittest.TestCase):
    def test_warmup_prefix_tagged_and_precedes_measurement(self):
        cdf = precompute_zipf_cdf(100, 0.8)
        with tempfile.TemporaryDirectory() as tmp:
            output_file = Path(tmp) / "test.ndjson"
            generate_dataset(
                zipf_cdf=cdf,
                total_requests=20,
                bot_ratio=0.5,
                output_file=output_file,
                warmup_requests=5,
            )
            events = [json.loads(line) for line in output_file.read_text().splitlines()]

        self.assertTrue(any(e["phase"] == "warmup" for e in events))
        self.assertTrue(any(e["phase"] == "measurement" for e in events))

        # No bots during warm-up.
        for event in events:
            if event["phase"] == "warmup":
                self.assertEqual(event["traffic_type"], "human")

        # Still one continuous, chronologically sorted timeline.
        timestamps = [e["timestamp"] for e in events]
        self.assertEqual(timestamps, sorted(timestamps))

        # Warm-up opens the file (precedes measurement on the timeline).
        self.assertEqual(events[0]["phase"], "warmup")


if __name__ == "__main__":
    unittest.main()
