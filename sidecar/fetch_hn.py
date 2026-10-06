"""Fetch real Hacker News stories and comments about crypto from the public
Algolia API (no key) into a posts file the scorer reads.

Point-in-time discipline: we keep only what was knowable when the post
appeared: its id, creation time, and text. Points and comment counts are NOT
kept, because they accumulate after the post and would leak the future into a
backtest.

    python sidecar/fetch_hn.py                       # window = the committed bar file
    python sidecar/fetch_hn.py --since 1791250000    # explicit Unix seconds
"""

import argparse
import html
import json
import re
import time
import urllib.parse
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
API = "https://hn.algolia.com/api/v1/search_by_date"
# Plain words, not tickers: "btc" matches thousands of unrelated items in
# Algolia's prefix search.
QUERIES = ["bitcoin", "ethereum", "crypto", "stablecoin", "coinbase"]
MAX_CHARS = 280


def bar_window(path):
    lo = hi = None
    with open(path) as f:
        for line in f:
            ts = json.loads(line)["ts"]
            lo = ts if lo is None else min(lo, ts)
            hi = ts if hi is None else max(hi, ts)
    return lo // 1000, hi // 1000


def clean(text):
    """Strip HTML tags and entities from a comment, collapse whitespace."""
    text = html.unescape(re.sub(r"<[^>]+>", " ", text or ""))
    text = re.sub(r"\s+", " ", text).strip()
    return text[:MAX_CHARS]


def fetch(query, tag, since, until):
    """All hits for one query and tag in [since, until), paging by page."""
    hits, page = [], 0
    while True:
        params = urllib.parse.urlencode({
            "query": query,
            "tags": tag,
            "numericFilters": f"created_at_i>={since},created_at_i<{until}",
            "hitsPerPage": 100,
            "page": page,
        })
        with urllib.request.urlopen(f"{API}?{params}", timeout=20) as r:
            data = json.load(r)
        hits.extend(data["hits"])
        page += 1
        if page >= data.get("nbPages", 0):
            return hits
        time.sleep(0.2)


def to_post(hit):
    kind = "story" if "story" in hit.get("_tags", []) else "comment"
    text = hit.get("title") if kind == "story" else clean(hit.get("comment_text"))
    if not text:
        return None
    return {
        "id": f"hn-{hit['objectID']}",
        "published_ts": int(hit["created_at_i"]) * 1000,
        "source": "hackernews",
        "kind": kind,
        "synthetic": False,
        "url": f"https://news.ycombinator.com/item?id={hit['objectID']}",
        "text": text,
    }


def collect(since, until):
    posts = {}
    for q in QUERIES:
        for tag in ("story", "comment"):
            for hit in fetch(q, tag, since, until):
                p = to_post(hit)
                if p:
                    posts[p["id"]] = p  # dedupe across queries
    return sorted(posts.values(), key=lambda p: (p["published_ts"], p["id"]))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--bars", default=str(ROOT / "data" / "bars_1m.jsonl"))
    ap.add_argument("--since", type=int, help="Unix seconds; default: first bar")
    ap.add_argument("--until", type=int, help="Unix seconds; default: last bar")
    ap.add_argument("--out", default=str(ROOT / "data" / "hn_posts.jsonl"))
    a = ap.parse_args()
    lo, hi = bar_window(a.bars)
    posts = collect(a.since or lo, a.until or hi)
    with open(a.out, "w", newline="\n", encoding="utf-8") as f:
        for p in posts:
            f.write(json.dumps(p, ensure_ascii=False) + "\n")
    kinds = {k: sum(p["kind"] == k for p in posts) for k in ("story", "comment")}
    print(f"wrote {len(posts)} Hacker News posts {kinds} to {a.out}")


if __name__ == "__main__":
    main()
