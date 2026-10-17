"""Script for generating synthetic mixed AI and Human request logs.

The default traffic model (see `generate_workloads`):

- One shared human stream per seed, reused in every ratio file and both workloads, made of
  multi-page sessions (one IP each) with exponential think times and a per-visitor favicon.
- Bots spread evenly over the same measurement window, so the bot share is about the target
  ratio in every slice of the timeline. Naive bots come from a small pool of datacenter /
  TEST-NET IPs at a fixed per-IP rate; stealthy bots imitate human sessions.
- Every record carries a deterministic per-item `size_bytes`.

`--legacy` reproduces the original single-request-per-IP, fixed-cadence generator, byte for
byte for a given `--seed`, so old results can be regenerated for comparison.
"""

import argparse
import ipaddress
import json
import math
import random
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

import numpy as np

# --- Configuration & Parameters ---
CATALOG_SIZE = 100_000
ZIPF_ALPHA = 0.8
TOTAL_REQUESTS_PER_DATASET = 200_000
# Number of pure-human warm-up requests prepended to EACH generated dataset
# (not a single shared file), so the cache reaches a representative steady
# state before the measurement traffic begins.
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
# of threading IPs through the benchmark at all. Used by `--legacy` only; the
# default model sizes its pool with `bot_ip_pool`.
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
_PUBLIC_FIRST_OCTETS = tuple(o for o in range(1, 224) if o not in _RESERVED_FIRST_OCTETS)

# Datacenter ranges covered by data/asn/ip2asn-fixture.tsv (Linode, DigitalOcean).
DATACENTER_NETWORKS = tuple(
    ipaddress.ip_network(n) for n in ("45.33.0.0/17", "104.248.0.0/16", "167.172.0.0/16")
)
# Documentation ranges (RFC 5737): routable-looking but matched by no ASN.
TEST_NET_NETWORKS = tuple(
    ipaddress.ip_network(n) for n in ("192.0.2.0/24", "198.51.100.0/24", "203.0.113.0/24")
)
# Bots draw from the datacenter ranges and the two TEST-NETs that BOT_IP_POOL uses.
BOT_TEST_NET_NETWORKS = TEST_NET_NETWORKS[1:]
# Share of naive-bot IPs placed in datacenter ranges, matching BOT_IP_POOL (9 of 14).
BOT_DATACENTER_SHARE = 9 / 14

# Naive bot IPs run in on/off cycles of this length (seconds), so a pool that would be
# idle part of the time still sends at the full per-IP rate while it is active.
BOT_DUTY_PERIOD_S = 30.0
# Relative jitter on a naive bot's inter-request gap.
BOT_JITTER = 0.05

FAVICON_PATH = "/favicon.ico"

WORKLOAD_IDS = {"naive": 1, "stealthy": 2}


def random_public_ipv4() -> str:
    """Generates a plausible non-reserved IPv4 address (stdlib-only)."""
    first = random.choice([o for o in range(1, 224) if o not in _RESERVED_FIRST_OCTETS])
    return f"{first}.{random.randint(0, 255)}.{random.randint(0, 255)}.{random.randint(1, 254)}"


def is_reserved_for_bots(ip: str) -> bool:
    """Whether `ip` is in a fixture datacenter range or a TEST-NET range."""
    addr = ipaddress.ip_address(ip)
    return any(addr in net for net in DATACENTER_NETWORKS + TEST_NET_NETWORKS)


def random_residential_ipv4(rng: random.Random) -> str:
    """A public IPv4 address outside the fixture's datacenter ranges and TEST-NET.

    Used for humans and stealthy bots, so the per-IP and ASN signals can't collide with
    the naive bot pool by chance.
    """
    while True:
        first = rng.choice(_PUBLIC_FIRST_OCTETS)
        ip = f"{first}.{rng.randint(0, 255)}.{rng.randint(0, 255)}.{rng.randint(1, 254)}"
        if not is_reserved_for_bots(ip):
            return ip


def random_ip_in(network: ipaddress.IPv4Network, rng: random.Random) -> str:
    """A random host address inside `network`."""
    return str(network.network_address + rng.randint(1, network.num_addresses - 2))


def bot_ip_pool(size: int, rng: random.Random) -> list[str]:
    """`size` distinct naive-bot IPs, about 64% in datacenter ranges and the rest in TEST-NET."""
    n_datacenter = round(size * BOT_DATACENTER_SHARE)
    ips: list[str] = []
    seen: set[str] = set()
    while len(ips) < size:
        networks = DATACENTER_NETWORKS if len(ips) < n_datacenter else BOT_TEST_NET_NETWORKS
        ip = random_ip_in(rng.choice(networks), rng)
        if ip not in seen:
            seen.add(ip)
            ips.append(ip)
    return ips


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


# --- Legacy model (`--legacy`): kept verbatim so it reproduces the old files ---


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
    warmup_requests: int = WARMUP_REQUESTS,
):
    """Generates a phase-tagged, chronologically interleaved NDJSON log file (legacy model).

    The file opens with a pure-human Zipfian warm-up prefix (phase="warmup",
    no bots) drawn from the same catalog distribution as the measurement
    traffic, so the items that will be popular during measurement are
    already cache-resident by the time measurement begins. The warm-up and
    measurement streams share one continuous timeline (measurement's clock
    continues from where warm-up left off), so a replay tool pacing requests
    by timestamp flows naturally from one phase into the other.

    Known flaws, fixed by the default model: the 20 ms bot stream and the 80 ms human
    stream end at different times, so the bot share is not stationary, and every human
    request comes from a fresh IP.
    """
    events = []

    # 0. Warm-Up Phase: pure Zipfian human traffic, no bots.
    warmup_items = sample_zipf(zipf_cdf, warmup_requests)
    t = 0.0
    for item_id in warmup_items:
        inter_arrival = np.random.exponential(scale=0.08)  # Average ~80ms gap
        t += inter_arrival
        for event in generate_human_event(int(item_id), t):
            event["phase"] = "warmup"
            events.append(event)
    warmup_end_t = t

    num_bot_requests = int(total_requests * bot_ratio)
    num_human_requests = total_requests - num_bot_requests

    # Sample object spaces
    human_items = sample_zipf(zipf_cdf, num_human_requests)
    bot_items = np.random.randint(1, CATALOG_SIZE + 1, size=num_bot_requests)

    # 1. Human Stream Generation (Bursty Pareto / Exponential delays),
    #    continuing the warm-up clock.
    t = warmup_end_t
    for item_id in human_items:
        inter_arrival = np.random.exponential(scale=0.08)  # Average ~80ms gap
        t += inter_arrival
        for event in generate_human_event(int(item_id), t):
            event["phase"] = "measurement"
            events.append(event)

    # 2. Bot Stream Generation (Highly consistent, mechanical intervals),
    #    also continuing the warm-up clock.
    t = warmup_end_t
    for item_id in bot_items:
        inter_arrival = random.gauss(0.02, 0.001)  # Fixed ~20ms cadence with low jitter
        t += max(0.001, inter_arrival)
        for event in generate_bot_event(int(item_id), t):
            event["phase"] = "measurement"
            events.append(event)

    # 3. Interleave chronologically
    events.sort(key=lambda x: x["timestamp"])

    # 4. Stream to NDJSON file
    with output_file.open("w", encoding="utf-8") as f:
        for event in events:
            f.write(json.dumps(event) + "\n")

    warmup_count = sum(1 for e in events if e["phase"] == "warmup")
    measurement_count = len(events) - warmup_count
    print(
        f"Generated {output_file.name} -> {len(events)} total lines "
        f"({warmup_count} warm-up + {measurement_count} measurement, "
        f"bot ratio: {bot_ratio * 100:.0f}%)"
    )


# --- Default model ---


class SizeModel:
    """Deterministic per-item payload sizes.

    An item's size is a lognormal draw from `np.random.default_rng(item_id)`, so every
    request for an item, in every file and for every seed, gets the same size.
    """

    def __init__(self, median: int, sigma: float, min_bytes: int, max_bytes: int, favicon: int):
        self.mu = math.log(median)
        self.sigma = sigma
        self.min_bytes = min_bytes
        self.max_bytes = max_bytes
        self.favicon = favicon
        self._cache: dict[int, int] = {}

    def item(self, item_id: int) -> int:
        """The size in bytes of `/item/{item_id}`."""
        size = self._cache.get(item_id)
        if size is None:
            draw = np.random.default_rng(item_id).lognormal(mean=self.mu, sigma=self.sigma)
            size = int(min(max(round(draw), self.min_bytes), self.max_bytes))
            self._cache[item_id] = size
        return size


@dataclass(frozen=True)
class Window:
    """The simulated timeline: warm-up is `[0, warmup_end)`, measurement `[warmup_end, end)`."""

    warmup_end: float
    end: float

    @property
    def measurement_s(self) -> float:
        """Length of the measurement window in seconds."""
        return self.end - self.warmup_end


def page_headers(user_agent: str) -> dict:
    """Full browser headers for a page request."""
    return {
        "User-Agent": user_agent,
        "Accept-Language": "en-US,en;q=0.9",
        "Sec-CH-UA": '"Chromium";v="124", "Google Chrome";v="124"',
        "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
    }


def browser_session(
    *,
    start: float,
    ip: str,
    pages: int,
    think_time_mean: float,
    fetch_favicon: bool,
    next_item,
    traffic_type: str,
    label_key: str,
    label: str,
    sizes: SizeModel,
    rng: random.Random,
    nprng: np.random.Generator,
) -> list[dict]:
    """One browser-like session: `pages` page views from one IP with exponential think times,
    plus a favicon right after the first page if `fetch_favicon`.

    `next_item()` picks each page's item id. `label_key`/`label` tag every record
    (`visitor` for humans, `bot_kind` for stealthy bots).
    """
    user_agent = rng.choice(HUMAN_USER_AGENTS)
    records = []
    t = start
    for page in range(pages):
        if page > 0:
            t += float(nprng.exponential(think_time_mean))
        item_id = next_item()
        records.append(
            {
                "timestamp": round(t, 4),
                "method": "GET",
                "path": f"/item/{item_id}",
                "headers": page_headers(user_agent),
                "traffic_type": traffic_type,
                "ip": ip,
                "phase": "measurement",
                "size_bytes": sizes.item(item_id),
                label_key: label,
            }
        )
        if page == 0 and fetch_favicon:
            records.append(
                {
                    "timestamp": round(t + rng.uniform(0.005, 0.030), 4),
                    "method": "GET",
                    "path": FAVICON_PATH,
                    "headers": {
                        "User-Agent": user_agent,
                        "Accept-Language": "en-US,en;q=0.9",
                        "Referer": f"http://localhost/item/{item_id}",
                    },
                    "traffic_type": traffic_type,
                    "ip": ip,
                    "phase": "measurement",
                    "size_bytes": sizes.favicon,
                    label_key: label,
                }
            )
    return records


def generate_human_stream(
    args: argparse.Namespace, zipf_cdf: np.ndarray, sizes: SizeModel
) -> tuple[list[dict], Window]:
    """The shared human stream: Poisson session arrivals over warm-up plus measurement.

    The session arrival rate is chosen so the aggregate request rate is `--human-rate`.
    Requests before `warmup_end` are tagged `phase="warmup"`; requests at or after `end`
    are dropped, truncating the sessions still open then.
    """
    rng = random.Random(f"{args.seed}:human")
    nprng = np.random.default_rng([args.seed, 0])
    window = Window(
        warmup_end=args.warmup_requests / args.human_rate,
        end=(args.warmup_requests + args.human_requests) / args.human_rate,
    )
    requests_per_session = args.pages_per_session_mean + (1.0 - args.returning_frac)
    session_rate = args.human_rate / requests_per_session
    nat_ips = [random_residential_ipv4(rng) for _ in range(args.nat_ips)] if args.nat_frac else []

    def next_item() -> int:
        return int(np.searchsorted(zipf_cdf, nprng.random())) + 1

    records: list[dict] = []
    t = 0.0
    while True:
        t += rng.expovariate(session_rate)
        if t >= window.end:
            break
        returning = rng.random() < args.returning_frac
        behind_nat = bool(nat_ips) and rng.random() < args.nat_frac
        records.extend(
            browser_session(
                start=t,
                ip=rng.choice(nat_ips) if behind_nat else random_residential_ipv4(rng),
                pages=int(nprng.geometric(1.0 / args.pages_per_session_mean)),
                think_time_mean=args.think_time_mean,
                # Returning visitors already have the favicon cached.
                fetch_favicon=not returning,
                next_item=next_item,
                traffic_type="human",
                label_key="visitor",
                label="returning" if returning else "new",
                sizes=sizes,
                rng=rng,
                nprng=nprng,
            )
        )

    records = [r for r in records if r["timestamp"] < window.end]
    for record in records:
        if record["timestamp"] < window.warmup_end:
            record["phase"] = "warmup"
    return records, window


def target_bot_requests(human_measurement_requests: int, ratio: float) -> int:
    """`B = H * r / (1 - r)`: the bot count that makes bots a share `r` of measurement."""
    return round(human_measurement_requests * ratio / (1.0 - ratio))


def generate_naive_bots(
    args: argparse.Namespace, n_bots: int, window: Window, sizes: SizeModel, rng, nprng
) -> tuple[list[dict], dict]:
    """Mechanical scrapers spread evenly over the measurement window.

    The pool grows with the ratio, `ceil((B / T) / bot_ip_rate)` IPs, so each IP sends at
    `--bot-ip-rate` whatever the ratio. Pool IPs run in on/off cycles of
    `BOT_DUTY_PERIOD_S` with a shared duty cycle that makes the aggregate rate exactly
    `B / T`, and with random phases so their on-periods don't line up.
    """
    if n_bots <= 0:
        return [], {"bot_ip_pool_size": 0, "bot_duty_cycle": 0.0}
    aggregate_rate = n_bots / window.measurement_s
    pool_size = math.ceil(aggregate_rate / args.bot_ip_rate)
    duty = aggregate_rate / (pool_size * args.bot_ip_rate)
    on_s = duty * BOT_DUTY_PERIOD_S
    gap = 1.0 / args.bot_ip_rate

    records = []
    for ip in bot_ip_pool(pool_size, rng):
        user_agent = rng.choice(BOT_USER_AGENTS)
        # Start of the current on-period. Tracked explicitly rather than with `%`, which
        # can round to just under a period and then never advance.
        cycle_start = window.warmup_end - rng.uniform(0.0, BOT_DUTY_PERIOD_S)
        t = window.warmup_end + rng.uniform(0.0, gap)
        while t < window.end:
            while t >= cycle_start + BOT_DUTY_PERIOD_S:
                cycle_start += BOT_DUTY_PERIOD_S
            if t >= cycle_start + on_s:
                # Off: sleep until the next on-period starts.
                cycle_start += BOT_DUTY_PERIOD_S
                t = cycle_start
                continue
            item_id = int(nprng.integers(1, args.catalog_size + 1))
            records.append(
                {
                    "timestamp": round(t, 4),
                    "method": "GET",
                    "path": f"/item/{item_id}",
                    "headers": {
                        "User-Agent": user_agent,
                        # Omit Accept-Language, Sec-CH-UA to trigger heuristic signals
                        "Accept": "*/*",
                    },
                    "traffic_type": "bot",
                    "ip": ip,
                    "phase": "measurement",
                    "size_bytes": sizes.item(item_id),
                    "bot_kind": "naive",
                }
            )
            t += max(0.001, rng.gauss(gap, BOT_JITTER * gap))
    records = [r for r in records if r["timestamp"] < window.end]
    return records, {"bot_ip_pool_size": pool_size, "bot_duty_cycle": duty}


def generate_stealthy_bots(
    args: argparse.Namespace, n_bots: int, window: Window, sizes: SizeModel, rng, nprng
) -> tuple[list[dict], dict]:
    """Bots that imitate human sessions but still scan the catalog uniformly.

    Each IP is a fresh residential-looking address that sends about
    `--stealthy-requests-per-ip` requests: full browser headers, exponential think times
    and one favicon after its first page. Session starts are spread evenly over the
    measurement window (one per slot of `T / sessions`, at a random point in the slot)
    rather than drawn as a Poisson process, so the bot share stays near the target ratio
    in every slice of the timeline, like the naive pool's.
    """
    if n_bots <= 0:
        return [], {"stealthy_sessions": 0}
    pages_mean = args.stealthy_requests_per_ip - 1.0  # one request per IP is the favicon
    sessions = max(1, round(n_bots / args.stealthy_requests_per_ip))
    slot_s = window.measurement_s / sessions

    def next_item() -> int:
        return int(nprng.integers(1, args.catalog_size + 1))

    records: list[dict] = []
    for slot in range(sessions):
        records.extend(
            browser_session(
                start=window.warmup_end + (slot + rng.random()) * slot_s,
                ip=random_residential_ipv4(rng),
                pages=int(nprng.geometric(1.0 / pages_mean)),
                think_time_mean=args.think_time_mean,
                fetch_favicon=True,
                next_item=next_item,
                traffic_type="bot",
                label_key="bot_kind",
                label="stealthy",
                sizes=sizes,
                rng=rng,
                nprng=nprng,
            )
        )
    records = [r for r in records if r["timestamp"] < window.end]
    return records, {"stealthy_sessions": sessions}


BOT_GENERATORS = {"naive": generate_naive_bots, "stealthy": generate_stealthy_bots}
FILE_PREFIXES = {"naive": "traffic_mixed", "stealthy": "traffic_stealthy"}


def ratio_pct(ratio: float) -> int:
    """The ratio as a whole percentage, for file names."""
    return round(ratio * 100)


def write_ndjson(path: Path, events: list[dict]) -> None:
    """Writes one JSON object per line."""
    with path.open("w", encoding="utf-8") as f:
        for event in events:
            f.write(json.dumps(event) + "\n")


def git_commit() -> str | None:
    """The generator's git commit (with `-dirty` if the tree has changes), or None."""
    here = Path(__file__).resolve().parent
    try:
        commit = subprocess.run(
            ["git", "rev-parse", "HEAD"], cwd=here, capture_output=True, text=True, check=True
        ).stdout.strip()
        dirty = subprocess.run(
            ["git", "status", "--porcelain", "--", "."], cwd=here, capture_output=True,
            text=True, check=True,
        ).stdout.strip()
    except (OSError, subprocess.CalledProcessError):
        return None
    return f"{commit}-dirty" if dirty else commit


def meta_path(ndjson_path: Path) -> Path:
    """`traffic_x.ndjson` -> `traffic_x.meta.json`."""
    return ndjson_path.with_suffix(".meta.json")


def write_meta(ndjson_path: Path, args: argparse.Namespace, workload: str, ratio: float,
               derived: dict) -> None:
    """Writes the `.meta.json` sidecar that replay_bench copies into `--json-out`."""
    meta = {
        "generator": "scripts/generate_logs.py",
        "git_commit": git_commit(),
        "seed": args.seed,
        "workload": workload,
        "ratio": ratio,
        "legacy": args.legacy,
        "args": args_to_json(args),
        "derived": derived,
    }
    meta_path(ndjson_path).write_text(json.dumps(meta, indent=2) + "\n", encoding="utf-8")


def args_to_json(args: argparse.Namespace) -> dict:
    """The parsed CLI arguments as JSON-compatible values."""
    return {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()}


def generate_workloads(args: argparse.Namespace) -> list[Path]:
    """Writes one file per (workload, ratio) for the default model; returns their paths."""
    sizes = SizeModel(
        args.item_size_median, args.item_size_sigma, args.item_size_min, args.item_size_max,
        args.favicon_size,
    )
    zipf_cdf = precompute_zipf_cdf(args.catalog_size, args.zipf_alpha)
    humans, window = generate_human_stream(args, zipf_cdf, sizes)
    human_measurement = sum(1 for r in humans if r["phase"] == "measurement")
    human_warmup = len(humans) - human_measurement

    workloads = ["naive", "stealthy"] if args.workload == "both" else [args.workload]
    written = []
    for workload in workloads:
        for ratio in args.ratios:
            pct = ratio_pct(ratio)
            rng = random.Random(f"{args.seed}:{workload}:{pct}")
            nprng = np.random.default_rng([args.seed, WORKLOAD_IDS[workload], pct])
            n_bots = target_bot_requests(human_measurement, ratio)
            bots, bot_stats = BOT_GENERATORS[workload](args, n_bots, window, sizes, rng, nprng)

            # Stable sort: ties keep humans before bots, so output is deterministic.
            events = sorted(humans + bots, key=lambda e: e["timestamp"])
            path = args.out_dir / f"{FILE_PREFIXES[workload]}_{pct}pct_bot.ndjson"
            write_ndjson(path, events)
            derived = {
                "warmup_end_s": window.warmup_end,
                "end_s": window.end,
                "human_warmup_requests": human_warmup,
                "human_measurement_requests": human_measurement,
                "target_bot_requests": n_bots,
                "bot_requests": len(bots),
                "bot_duty_period_s": BOT_DUTY_PERIOD_S,
                **bot_stats,
            }
            write_meta(path, args, workload, ratio, derived)
            share = len(bots) / max(1, len(bots) + human_measurement)
            print(
                f"Generated {path.name} -> {len(events)} lines ({human_warmup} warm-up + "
                f"{human_measurement} human + {len(bots)} bot measurement, "
                f"bot share {share * 100:.1f}%)"
            )
            written.append(path)
    return written


def generate_legacy(args: argparse.Namespace) -> list[Path]:
    """Reproduces the original generator's output for the given seed."""
    zipf_cdf = precompute_zipf_cdf(args.catalog_size, args.zipf_alpha)
    written = []
    for ratio in args.ratios:
        # The original used int(), e.g. int(0.29 * 100) == 28; keep its names.
        path = args.out_dir / f"traffic_mixed_{int(ratio * 100)}pct_bot.ndjson"
        generate_dataset(
            zipf_cdf=zipf_cdf,
            total_requests=args.total_requests,
            bot_ratio=ratio,
            output_file=path,
            warmup_requests=args.warmup_requests,
        )
        write_meta(path, args, "naive", ratio, {})
        written.append(path)
    return written


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    """Parses and validates the command line."""
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("--seed", type=int, default=42, help="seeds random and numpy (default 42)")
    p.add_argument("--out-dir", type=Path, default=OUTPUT_DIR)
    p.add_argument("--ratios", type=float, nargs="+", default=BOT_RATIOS,
                   help="bot shares of measurement traffic, each in [0, 1)")
    p.add_argument("--workload", choices=["naive", "stealthy", "both"], default="both")
    p.add_argument("--legacy", action="store_true",
                   help="reproduce the original generator (naive workload only)")

    catalog = p.add_argument_group("catalog")
    catalog.add_argument("--catalog-size", type=int, default=CATALOG_SIZE)
    catalog.add_argument("--zipf-alpha", type=float, default=ZIPF_ALPHA)
    catalog.add_argument("--item-size-median", type=int, default=16 * 1024)
    catalog.add_argument("--item-size-sigma", type=float, default=1.0)
    catalog.add_argument("--item-size-min", type=int, default=512)
    catalog.add_argument("--item-size-max", type=int, default=2 * 1024 * 1024)
    catalog.add_argument("--favicon-size", type=int, default=1024)

    humans = p.add_argument_group("human sessions")
    humans.add_argument("--warmup-requests", type=int, default=WARMUP_REQUESTS,
                        help="human-only warm-up requests (approximate)")
    humans.add_argument("--human-requests", type=int, default=100_000,
                        help="human measurement requests (approximate)")
    humans.add_argument("--human-rate", type=float, default=12.5,
                        help="aggregate human requests per second")
    humans.add_argument("--pages-per-session-mean", type=float, default=5.0)
    humans.add_argument("--think-time-mean", type=float, default=8.0, help="seconds")
    humans.add_argument("--returning-frac", type=float, default=0.3,
                        help="share of sessions that already have the favicon cached")
    humans.add_argument("--nat-frac", type=float, default=0.0,
                        help="share of sessions behind a small set of shared NAT IPs")
    humans.add_argument("--nat-ips", type=int, default=16)

    bots = p.add_argument_group("bots")
    bots.add_argument("--bot-ip-rate", type=float, default=3.6,
                      help="requests per second per naive-bot IP")
    bots.add_argument("--stealthy-requests-per-ip", type=float, default=5.0)

    legacy = p.add_argument_group("legacy")
    legacy.add_argument("--total-requests", type=int, default=TOTAL_REQUESTS_PER_DATASET,
                        help="measurement requests per file (--legacy only)")

    args = p.parse_args(argv)
    if any(not 0.0 <= r < 1.0 for r in args.ratios):
        p.error("--ratios must each be in [0, 1)")
    if args.pages_per_session_mean < 1.0:
        p.error("--pages-per-session-mean must be at least 1")
    if args.stealthy_requests_per_ip < 2.0:
        p.error("--stealthy-requests-per-ip must be at least 2 (a page plus the favicon)")
    if not 0.0 <= args.returning_frac <= 1.0 or not 0.0 <= args.nat_frac <= 1.0:
        p.error("--returning-frac and --nat-frac must be in [0, 1]")
    if args.nat_frac and args.nat_ips < 1:
        p.error("--nat-ips must be at least 1 when --nat-frac is set")
    if min(args.human_rate, args.bot_ip_rate, args.think_time_mean) <= 0:
        p.error("rates and --think-time-mean must be positive")
    if args.legacy:
        args.workload = "naive"
    return args


def main(argv: list[str] | None = None) -> list[Path]:
    """Generates every requested benchmark dataset and its `.meta.json` sidecar."""
    args = parse_args(argv)
    random.seed(args.seed)
    np.random.seed(args.seed)
    args.out_dir.mkdir(parents=True, exist_ok=True)
    written = generate_legacy(args) if args.legacy else generate_workloads(args)
    print(f"\nAll benchmarking datasets successfully written to {args.out_dir}/")
    return written


if __name__ == "__main__":
    main(sys.argv[1:])
