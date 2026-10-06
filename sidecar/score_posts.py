"""Score posts into TextSignal events for the Rust engine.

For each post and each coin it writes one JSONL line in the engine's event
format:

    {"type":"TextSignal","coin":"BTC","ts":...,"published_ts":...,
     "source":...,"relevance":...,"bullish":...,"bearish":...,
     "novelty":...,"scorer":"jev"}

`ts` is when the score became available: publication time plus the scoring
latency (measured for Jev, an assumed fixed delay for the mock). Using the
publication time instead would let the backtest react to a post before it
could have been scored.

Two scorers:
  jev   TypeSafe's Jev via typesafe-sdk. Used when TYPESAFE_API_KEY is set.
        The key is read by the SDK from the environment; this script never
        reads, prints or stores it.
  mock  A deterministic keyword scorer, clearly labelled "mock-keyword-v1".
        Used when there is no key, or with --scorer mock.

    python sidecar/score_posts.py                     # jev if a key is set, else mock
    python sidecar/score_posts.py --scorer mock --out data/text_signals_mock.jsonl
"""

import argparse
import asyncio
import json
import math
import os
import statistics
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
COINS = {"BTC": "bitcoin (BTC)", "ETH": "ether / Ethereum (ETH)"}
MOCK_DELAY_MS = 1_000  # assumed scoring delay for the mock

BULL_WORDS = ["inflow", "record", "breaks out", "breakout", "approval", "rall", "buying", "upgrade", "confirmed"]
BEAR_WORDS = ["sell-off", "hack", "drained", "paused", "liquidation", "exploit", "lawsuit", "outflow", "brace"]
COIN_WORDS = {"BTC": ["bitcoin", "btc"], "ETH": ["ethereum", "ether", "eth"]}


def mock_score(text):
    """Keyword counts turned into probabilities. Deterministic, no network."""
    low = text.lower()
    words = set(low.replace(",", " ").replace(";", " ").replace(":", " ").split())
    nb = sum(w in low for w in BULL_WORDS)
    ns = sum(w in low for w in BEAR_WORDS)
    if nb + ns == 0:
        bull, bear = 0.1, 0.1
    else:
        bull, bear = 0.9 * nb / (nb + ns), 0.9 * ns / (nb + ns)
    novelty = 0.1 if low.startswith("rt ") else 0.9
    out = {}
    for coin, kws in COIN_WORDS.items():
        relevant = any(k in words for k in kws)
        out[coin] = {"relevance": 0.9 if relevant else 0.05, "bullish": bull, "bearish": bear, "novelty": novelty}
    return out


class JevScorer:
    """One Jev system_one call per post, asking about both coins at once."""

    name = "jev"

    def __init__(self):
        from typesafe_sdk import AsyncTypeSafeClient
        self.client = AsyncTypeSafeClient()  # reads TYPESAFE_API_KEY itself

    def questions(self):
        from typesafe_sdk import Choice, Noul
        q = {"novelty": Noul(instructions="Is this post new information, rather than a repost or a rehash of older news?")}
        for coin, name in COINS.items():
            q[f"about_{coin}"] = Noul(instructions=f"Is this post about {name}?")
            q[f"dir_{coin}"] = Choice(
                instructions=f"What does the post imply for the {name} price over the next hour?",
                criteria={
                    "bullish": "likely to push the price up",
                    "bearish": "likely to push the price down",
                    "neither": "no clear price implication",
                },
            )
        return q

    async def score(self, text):
        t0 = time.perf_counter()
        resp = await self.client.system_one(state=f"Post: {text}", questions=self.questions())
        latency_ms = (time.perf_counter() - t0) * 1000.0
        a = resp.answers
        out = {}
        for coin in COINS:
            probs = dict(a[f"dir_{coin}"].probabilities)
            out[coin] = {
                "relevance": float(a[f"about_{coin}"].noul),
                "bullish": float(probs.get("bullish", 0.0)),
                "bearish": float(probs.get("bearish", 0.0)),
                "novelty": float(a["novelty"].noul),
            }
        tokens = getattr(resp.usage, "input_tokens", None) or 0
        return out, latency_ms, tokens


def pct(values, p):
    """Nearest-rank percentile, matching the Rust engine's definition."""
    v = sorted(values)
    return v[max(1, math.ceil(p / 100 * len(v))) - 1] if v else None


async def run(args):
    posts = [json.loads(l) for l in Path(args.posts).read_text().splitlines() if l.strip()]
    use_jev = args.scorer == "jev" or (args.scorer == "auto" and os.environ.get("TYPESAFE_API_KEY"))
    scorer = JevScorer() if use_jev else None
    name = "jev" if use_jev else "mock-keyword-v1"
    print(f"scorer: {name} ({'TYPESAFE_API_KEY present' if use_jev else 'offline mock, no key used'})")

    lines, latencies, tokens = [], [], []
    for p in posts:
        if scorer:
            scores, latency_ms, tok = await scorer.score(p["text"])
            latencies.append(latency_ms)
            tokens.append(tok)
            delay = math.ceil(latency_ms)
        else:
            scores, delay = mock_score(p["text"]), MOCK_DELAY_MS
        for coin, s in scores.items():
            lines.append({
                "type": "TextSignal",
                "coin": coin,
                "ts": p["published_ts"] + delay,
                "published_ts": p["published_ts"],
                "source": p["source"],
                "relevance": round(s["relevance"], 6),
                "bullish": round(s["bullish"], 6),
                "bearish": round(s["bearish"], 6),
                "novelty": round(s["novelty"], 6),
                "scorer": name,
            })
    lines.sort(key=lambda l: (l["ts"], l["coin"]))
    out = Path(args.out)
    with out.open("w", newline="\n") as f:
        for l in lines:
            f.write(json.dumps(l) + "\n")
    print(f"wrote {len(lines)} TextSignal events for {len(posts)} posts to {out}")

    stats = {"scorer": name, "posts": len(posts)}
    if latencies:
        stats.update({
            "latency_ms_p50": round(pct(latencies, 50), 1),
            "latency_ms_p99": round(pct(latencies, 99), 1),
            "latency_ms_first_call": round(latencies[0], 1),
            "latency_ms_mean": round(statistics.mean(latencies), 1),
            "input_tokens_per_post_mean": round(statistics.mean(tokens), 1),
            "input_tokens_total": sum(tokens),
        })
    else:
        stats["note"] = f"mock: no network, assumed scoring delay {MOCK_DELAY_MS} ms"
    print(json.dumps(stats, indent=2))
    if args.stats:
        Path(args.stats).write_text(json.dumps(stats, indent=2) + "\n", newline="\n")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--posts", default=str(ROOT / "sidecar" / "posts_sample.jsonl"))
    ap.add_argument("--out", default=str(ROOT / "data" / "text_signals.jsonl"))
    ap.add_argument("--scorer", choices=["auto", "jev", "mock"], default="auto")
    ap.add_argument("--stats", default=None, help="optional path for a JSON summary of latency and tokens")
    asyncio.run(run(ap.parse_args()))


if __name__ == "__main__":
    main()
