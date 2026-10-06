"""Live social feed for `mft-engine paper`: poll Hacker News, score new posts
with Jev (or the mock), append TextSignal lines to a file the engine tails.

The file is the interface between the two processes. The Rust engine reads
new lines as they appear (`paper --text-feed`), so neither side needs to know
how the other is built, and the file is a complete record of what the live
engine was shown.

Point in time: each line's `ts` is the wall-clock time it was written, after
scoring, which is the earliest the engine could have seen it.

    python sidecar/live_social.py --out data/live_text_signals.jsonl --minutes 6
"""

import argparse
import asyncio
import json
import math
import time

from fetch_hn import collect
from score_posts import make_scorer, signal_lines, summary
from secrets_guard import write_lines


async def run(a):
    scorer = make_scorer(a.scorer)
    print(f"[social] scorer: {scorer.name}; polling Hacker News every {a.interval}s", flush=True)
    seen = set()
    latencies, tokens = [], []
    since = int(time.time()) - a.lookback_minutes * 60
    deadline = time.time() + a.minutes * 60
    while time.time() < deadline:
        now_s = int(time.time())
        try:
            posts = collect(since, now_s + 1)
        except Exception as e:  # network trouble: log, wait, try again
            print(f"[social] fetch failed: {e}", flush=True)
            posts = []
        for p in posts:
            if p["id"] in seen:
                continue
            seen.add(p["id"])
            scores, latency_ms, tok = await scorer.score(p["text"])
            latencies.append(latency_ms)
            tokens.append(tok)
            ts = int(time.time() * 1000)  # available now, after scoring
            write_lines(a.out, [json.dumps(l) for l in signal_lines(p, scores, scorer.name, ts)], mode="a")
            print(f"[social] scored {p['id']} in {math.ceil(latency_ms)} ms: {p['text'][:70]!r}", flush=True)
        since = now_s - 120  # small overlap; `seen` drops repeats
        await asyncio.sleep(a.interval)
    stats = summary(scorer.name, len(seen), latencies, tokens)
    print(json.dumps(stats, indent=2), flush=True)
    if a.stats:
        write_lines(a.stats, [json.dumps(stats, indent=2)])


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--out", default="data/live_text_signals.jsonl")
    ap.add_argument("--scorer", choices=["auto", "jev", "mock"], default="auto")
    ap.add_argument("--interval", type=int, default=60)
    ap.add_argument("--minutes", type=float, default=6)
    ap.add_argument("--lookback-minutes", type=int, default=30,
                    help="on start, also score posts from this far back (the text TTL)")
    ap.add_argument("--stats", default=None)
    asyncio.run(run(ap.parse_args()))


if __name__ == "__main__":
    main()
