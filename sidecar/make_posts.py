"""Generate a small set of SYNTHETIC social posts spread over the bar window.

These posts are invented. They exist so the text-signal path can be run end
to end offline. They are not real news, and nothing in the backtest should be
read as evidence that text helps or hurts.

Deterministic: a fixed seed and the first/last bar timestamps from the data
file fully determine the output.

    python sidecar/make_posts.py            # writes tests/fixtures/synthetic_posts.jsonl
"""

import json
import random
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BARS = ROOT / "data" / "bars_1m.jsonl"
OUT = ROOT / "tests" / "fixtures" / "synthetic_posts.jsonl"
N_POSTS = 60
SEED = 20261005

TEMPLATES = [
    # (text, label) -- the label is for humans reading the file, never used by the scorer.
    ("Spot bitcoin ETF inflows hit a weekly record, desks report heavy BTC buying", "btc_bull"),
    ("Large BTC holder moves 5,000 coins to an exchange; traders brace for a sell-off", "btc_bear"),
    ("Bitcoin breaks out above resistance as funding stays neutral", "btc_bull"),
    ("Exchange hack: hot wallet drained, withdrawals of BTC paused", "btc_bear"),
    ("Ethereum upgrade date confirmed, ETH staking inflows rising", "eth_bull"),
    ("Major ETH liquidation cascade on a lending protocol after oracle exploit", "eth_bear"),
    ("Regulator signals approval path for ether ETF options, ETH rallies", "eth_bull"),
    ("Lawsuit filed against Ethereum staking provider; ETH outflows accelerate", "eth_bear"),
    ("Crypto market quiet ahead of the US CPI print", "neutral"),
    ("New memecoin launches on a layer-2, community excited", "irrelevant"),
    ("Central bank minutes due tomorrow; risk assets range-bound", "neutral"),
]


def bar_window():
    first = last = None
    with BARS.open() as f:
        for line in f:
            ts = json.loads(line)["ts"]
            first = ts if first is None else min(first, ts)
            last = ts if last is None else max(last, ts)
    return first, last


def main():
    rng = random.Random(SEED)
    start, end = bar_window()
    posts = []
    for i in range(N_POSTS):
        text, label = rng.choice(TEMPLATES)
        if posts and rng.random() < 0.15:
            # A repost of an earlier post: same content, little new information.
            prev = rng.choice(posts)
            text, label = "RT " + prev["text"], prev["label"] + "_repost"
        posts.append({
            "id": f"syn-{i:03d}",
            "published_ts": rng.randint(start, end),
            "source": "synthetic",
            "synthetic": True,
            "label": label,
            "text": text,
        })
    posts.sort(key=lambda p: p["published_ts"])
    with OUT.open("w", newline="\n") as f:
        for p in posts:
            f.write(json.dumps(p) + "\n")
    print(f"wrote {len(posts)} synthetic posts to {OUT}")


if __name__ == "__main__":
    main()
