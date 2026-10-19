"""Score posts into TextSignal events for the Rust engine.

For each post and each coin it writes one JSONL line in the engine's event
format:

    {"type":"TextSignal","coin":"BTC","ts":...,"published_ts":...,
     "post_id":...,"source":...,"relevance":...,"bullish":...,"bearish":...,
     "novelty":...,"scorer":"classifier"}

Point in time. `ts` is when the engine is allowed to see the score:
publication time, plus how long until our poller would have fetched the post
(`--poll-delay-ms`, 60 s by default, matching the live sidecar), plus the
scoring latency (measured for the classifier, an assumed 1 s for the mock). A backtest
therefore never acts on a post before it could have been fetched and scored.
The scorer sees only the post's text.

Two scorers:
  classifier  A fast text classifier at CLASSIFIER_URL (bearer CLASSIFIER_API_KEY,
        model CLASSIFIER_MODEL), used when all three are set. The key is read
        from the environment only to send it; it is never printed or stored.
  mock  A deterministic keyword scorer, labelled "mock-keyword-v1". Used when
        there is no key, or with --scorer mock.

    python sidecar/score_posts.py --posts data/hn_posts.jsonl --out data/text_signals.jsonl
    python sidecar/score_posts.py --scorer mock --posts sidecar/posts_sample.jsonl --poll-delay-ms 0
"""

import argparse
import asyncio
import json
import math
import os
import statistics
import time
import urllib.request
from pathlib import Path

from secrets_guard import write_lines

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


def classifier_key():
    """CLASSIFIER_API_KEY, else the pre-rename name (fallback removed after one release)."""
    return os.environ.get("CLASSIFIER_API_KEY") or os.environ.get("TYPESAFE_API_KEY") or None


def classifier_configured():
    return bool(os.environ.get("CLASSIFIER_URL") and classifier_key() and os.environ.get("CLASSIFIER_MODEL"))


class ClassifierScorer:
    """One classifier call per post, asking about both coins at once."""

    name = "classifier"

    def __init__(self):
        self.url = os.environ.get("CLASSIFIER_URL")
        self.model = os.environ.get("CLASSIFIER_MODEL")
        if not self.url or not classifier_key() or not self.model:
            # the endpoint has no default model: a request without one is rejected (HTTP 422)
            raise RuntimeError("set CLASSIFIER_URL, CLASSIFIER_API_KEY and CLASSIFIER_MODEL for the classifier scorer")

    @staticmethod
    def questions():
        q = {"novelty": {"type": "noul", "instructions": "Is this post new information, rather than a repost or a rehash of older news?"}}
        for coin, name in COINS.items():
            q[f"about_{coin}"] = {"type": "noul", "instructions": f"Is this post about {name}?"}
            q[f"dir_{coin}"] = {
                "type": "choice",
                "instructions": f"What does the post imply for the {name} price over the next hour?",
                "criteria": {
                    "bullish": "likely to push the price up",
                    "bearish": "likely to push the price down",
                    "neither": "no clear price implication",
                },
            }
        return q

    def _post(self, body):
        req = urllib.request.Request(
            self.url, data=json.dumps(body).encode("utf-8"), method="POST",
            headers={"Authorization": f"Bearer {classifier_key() or ''}",
                     "Content-Type": "application/json"})
        with urllib.request.urlopen(req, timeout=30) as r:
            return json.loads(r.read().decode("utf-8"))

    async def score(self, text):
        """Returns (scores per coin, latency ms, input tokens)."""
        body = {"model": self.model, "state": f"Post: {text}", "questions": self.questions()}
        t0 = time.perf_counter()
        resp = await asyncio.to_thread(self._post, body)
        latency_ms = (time.perf_counter() - t0) * 1000.0
        a = resp["answers"]
        out = {}
        for coin in COINS:
            probs = dict(a[f"dir_{coin}"].get("probabilities", {}))
            out[coin] = {
                "relevance": float(a[f"about_{coin}"]["noul"]),
                "bullish": float(probs.get("bullish", 0.0)),
                "bearish": float(probs.get("bearish", 0.0)),
                "novelty": float(a["novelty"]["noul"]),
            }
        tokens = (resp.get("usage") or {}).get("input_tokens") or 0
        return out, latency_ms, tokens


class MockScorer:
    name = "mock-keyword-v1"

    async def score(self, text):
        return mock_score(text), float(MOCK_DELAY_MS), 0


def make_scorer(choice):
    """classifier, mock, or auto (classifier if CLASSIFIER_URL, CLASSIFIER_API_KEY and CLASSIFIER_MODEL are set)."""
    if choice == "classifier" or (choice == "auto" and classifier_configured()):
        return ClassifierScorer()
    return MockScorer()


def signal_lines(post, scores, scorer_name, ts):
    """One TextSignal event (as a dict) per coin for a scored post."""
    return [{
        "type": "TextSignal",
        "coin": coin,
        "ts": ts,
        "published_ts": post["published_ts"],
        "post_id": post["id"],
        "source": post["source"],
        "relevance": round(s["relevance"], 6),
        "bullish": round(s["bullish"], 6),
        "bearish": round(s["bearish"], 6),
        "novelty": round(s["novelty"], 6),
        "scorer": scorer_name,
    } for coin, s in scores.items()]


def pct(values, p):
    """Nearest-rank percentile, matching the Rust engine's definition."""
    v = sorted(values)
    return v[max(1, math.ceil(p / 100 * len(v))) - 1] if v else None


def summary(name, n_posts, latencies, tokens):
    stats = {"scorer": name, "posts": n_posts}
    if name == ClassifierScorer.name and latencies:
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
    return stats


async def run(args):
    posts = [json.loads(l) for l in Path(args.posts).read_text(encoding="utf-8").splitlines() if l.strip()]
    scorer = make_scorer(args.scorer)
    print(f"scorer: {scorer.name} ({'CLASSIFIER_API_KEY present' if scorer.name == ClassifierScorer.name else 'offline mock, no key used'})")

    lines, latencies, tokens = [], [], []
    for p in posts:
        scores, latency_ms, tok = await scorer.score(p["text"])
        latencies.append(latency_ms)
        tokens.append(tok)
        ts = p["published_ts"] + args.poll_delay_ms + math.ceil(latency_ms)
        lines.extend(signal_lines(p, scores, scorer.name, ts))
    lines.sort(key=lambda l: (l["ts"], l["coin"], l["post_id"]))
    write_lines(args.out, [json.dumps(l) for l in lines])
    print(f"wrote {len(lines)} TextSignal events for {len(posts)} posts to {args.out}")

    stats = summary(scorer.name, len(posts), latencies, tokens)
    print(json.dumps(stats, indent=2))
    if args.stats:
        write_lines(args.stats, [json.dumps(stats, indent=2)])


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--posts", default=str(ROOT / "data" / "hn_posts.jsonl"))
    ap.add_argument("--out", default=str(ROOT / "data" / "text_signals.jsonl"))
    ap.add_argument("--scorer", choices=["auto", "classifier", "mock"], default="auto")
    ap.add_argument("--poll-delay-ms", type=int, default=60_000,
                    help="assumed delay between publication and our poller fetching the post")
    ap.add_argument("--stats", default=None, help="optional path for a JSON summary of latency and tokens")
    asyncio.run(run(ap.parse_args()))


if __name__ == "__main__":
    main()
