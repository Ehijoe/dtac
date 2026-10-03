"""Script for generating synthetic mixed AI and Human request logs."""

import json
import random
from pathlib import Path

import numpy as np

# --- Configuration & Parameters ---
CATALOG_SIZE = 100_000
ZIPF_ALPHA = 0.8
TOTAL_REQUESTS_PER_DATASET = 200_000
WARMUP_REQUESTS = 50_000
OUTPUT_DIR = Path("./benchmark_logs")

BOT_RATIOS = [0.10, 0.30, 0.50, 0.75]

HUMAN_USER_AGENTS = [
    (
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
        "(KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36"
    ),
    (
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 "
        "(KHTML, like Gecko) Version/17.4 Safari/605.1.15"
    ),
    "Mozilla/5.0 (X11; Linux x86_64; rv:125.0) Gecko/20100101 Firefox/125.0",
]

BOT_USER_AGENTS = [
    "python-requests/2.31.0",
    "aiohttp/3.9.3",
    "Go-http-client/1.1",
    "Mozilla/5.0 (compatible; ScraperBot/1.0; +http://example.com/bot)",
]

# Small fixed pool of source IPs simulating a handful of scraper boxes hammering
# at a mechanical cadence -- this is what the router's per-IP velocity/burst
# heuristic (BURST_THRESHOLD / ALPHA in src/heuristic_router/mod.rs) is actually
# designed to catch. Giving every bot request a fresh IP would simulate a
# distributed botnet that evades that heuristic entirely, defeating the point
# of threading IPs through the benchmark at all.
BOT_IP_POOL = [
    "45.33.12.4", "45.33.12.5", "45.33.12.9",
    "198.51.100.23", "198.51.100.47", "198.51.100.91",
    "203.0.113.14", "203.0.113.55", "203.0.113.102",
    "104.248.33.7", "104.248.33.19", "104.248.33.44",
    "167.172.9.201", "167.172.9.214",
]

# First octets to avoid when generating synthetic human IPs, so they don't
# collide with loopback/link-local/private/reserved ranges.
_RESERVED_FIRST_OCTETS = {0, 10, 127, 169, 172, 192, 224, 255}


def random_public_ipv4() -> str:
    """Generates a plausible non-reserved IPv4 address (stdlib-only)."""
    first = random.choice([o for o in range(1, 224) if o not in _RESERVED_FIRST_OCTETS])
    return f"{first}.{random.randint(0, 255)}.{random.randint(0, 255)}.{random.randint(1, 254)}"


def precompute_zipf_cdf(n: int, alpha: float) -> np.ndarray:
    """Precomputes normalized cumulative distribution for Zipfian sampling."""
    ranks = np.arange(1, n + 1, dtype=np.float64)
    weights = 1.0 / np.power(ranks, alpha)
    cdf = np.cumsum(weights)
    cdf /= cdf[-1]
    return cdf


def sample_zipf(cdf: np.ndarray, size: int) -> np.ndarray:
    """Samples item IDs following the precomputed Zipfian distribution."""
    random_values = np.random.random(size)
    return np.searchsorted(cdf, random_values) + 1


def generate_human_event(item_id: int, current_timestamp: float) -> list[dict]:
    """Generates a human request followed occasionally by a secondary asset."""
    requests = []

    # One simulated browser session gets one IP, shared across its primary
    # request and any follow-up asset request.
    session_ip = random_public_ipv4()

    # Primary Document Request
    primary_req = {
        "timestamp": round(current_timestamp, 4),
        "method": "GET",
        "path": f"/item/{item_id}",
        "headers": {
            "User-Agent": random.choice(HUMAN_USER_AGENTS),
            "Accept-Language": "en-US,en;q=0.9",
            "Sec-CH-UA": '"Chromium";v="124", "Google Chrome";v="124"',
            "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        },
        "traffic_type": "human",
        "ip": session_ip,
    }
    requests.append(primary_req)

    # Secondary Asset Request (e.g., favicon / CSS) 40% of the time
    if random.random() < 0.40:
        asset_delay = random.uniform(0.005, 0.030)  # 5-30ms browser pipelining
        asset_req = {
            "timestamp": round(current_timestamp + asset_delay, 4),
            "method": "GET",
            "path": "/favicon.ico",
            "headers": {
                "User-Agent": primary_req["headers"]["User-Agent"],
                "Accept-Language": "en-US,en;q=0.9",
                "Referer": f"http://localhost/item/{item_id}",
            },
            "traffic_type": "human",
            "ip": session_ip,
        }
        requests.append(asset_req)

    return requests


def generate_bot_event(item_id: int, current_timestamp: float) -> list[dict]:
    """Generates a mechanical scraper request without secondary assets."""
    return [
        {
            "timestamp": round(current_timestamp, 4),
            "method": "GET",
            "path": f"/item/{item_id}",
            "headers": {
                "User-Agent": random.choice(BOT_USER_AGENTS),
                # Omit Accept-Language, Sec-CH-UA to trigger heuristic signals
                "Accept": "*/*",
            },
            "traffic_type": "bot",
            "ip": random.choice(BOT_IP_POOL),
        }
    ]


def generate_dataset(
    zipf_cdf: np.ndarray,
    total_requests: int,
    bot_ratio: float,
    output_file: Path,
):
    """Generates a mixed, chronologically interleaved NDJSON log file."""
    num_bot_requests = int(total_requests * bot_ratio)
    num_human_requests = total_requests - num_bot_requests

    # Sample object spaces
    human_items = sample_zipf(zipf_cdf, num_human_requests)
    bot_items = np.random.randint(1, CATALOG_SIZE + 1, size=num_bot_requests)

    events = []

    # 1. Human Stream Generation (Bursty Pareto / Exponential delays)
    t = 0.0
    for item_id in human_items:
        inter_arrival = np.random.exponential(scale=0.08)  # Average ~80ms gap
        t += inter_arrival
        events.extend(generate_human_event(int(item_id), t))

    # 2. Bot Stream Generation (Highly consistent, mechanical intervals)
    t = 0.0
    for item_id in bot_items:
        inter_arrival = random.gauss(0.02, 0.001)  # Fixed ~20ms cadence with low jitter
        t += max(0.001, inter_arrival)
        events.extend(generate_bot_event(int(item_id), t))

    # 3. Interleave chronologically
    events.sort(key=lambda x: x["timestamp"])

    # 4. Stream to NDJSON file
    with output_file.open("w", encoding="utf-8") as f:
        for event in events:
            f.write(json.dumps(event) + "\n")

    print(
        f"Generated {output_file.name} -> {len(events)} total lines "
        f"(Bot ratio: {bot_ratio * 100:.0f}%)"
    )


def main():
    """Generates the warmup dataset and all mixed bot-ratio benchmark datasets."""
    OUTPUT_DIR.mkdir(parents=True, exist_ok=True)
    print(f"Precomputing Zipfian CDF (N={CATALOG_SIZE}, alpha={ZIPF_ALPHA})...")
    zipf_cdf = precompute_zipf_cdf(CATALOG_SIZE, ZIPF_ALPHA)

    # 1. Warm-Up Dataset (Pure Zipfian Human Traffic)
    generate_dataset(
        zipf_cdf=zipf_cdf,
        total_requests=WARMUP_REQUESTS,
        bot_ratio=0.0,
        output_file=OUTPUT_DIR / "traffic_warmup.ndjson",
    )

    # 2. Mixed Workloads (10%, 30%, 50%, 75% Bot Traffic)
    for ratio in BOT_RATIOS:
        pct = int(ratio * 100)
        filename = OUTPUT_DIR / f"traffic_mixed_{pct}pct_bot.ndjson"
        generate_dataset(
            zipf_cdf=zipf_cdf,
            total_requests=TOTAL_REQUESTS_PER_DATASET,
            bot_ratio=ratio,
            output_file=filename,
        )

    print("\nAll benchmarking datasets successfully written to ./benchmark_logs/")


if __name__ == "__main__":
    main()
