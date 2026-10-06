"""Fetch recent Reddit posts through the official OAuth API.

Reddit's anonymous JSON endpoints are blocked for scripts, so this uses an
app-only OAuth token (client credentials grant). It runs only when both
REDDIT_CLIENT_ID and REDDIT_CLIENT_SECRET are set in the environment; the
values are read from the environment, sent only to reddit.com, and never
printed or written. Create a "script" app at https://www.reddit.com/prefs/apps.

Not exercised in this repository's committed results: no Reddit credentials
were available when the results were produced.

    python sidecar/fetch_reddit.py --out data/reddit_posts.jsonl
"""

import argparse
import base64
import json
import os
import sys
import urllib.parse
import urllib.request

SUBREDDITS = ["Bitcoin", "ethereum", "CryptoCurrency"]
USER_AGENT = "mft-engine-research/0.1 (read-only)"


def token(client_id, secret):
    basic = base64.b64encode(f"{client_id}:{secret}".encode()).decode()
    req = urllib.request.Request(
        "https://www.reddit.com/api/v1/access_token",
        data=urllib.parse.urlencode({"grant_type": "client_credentials"}).encode(),
        headers={"Authorization": f"Basic {basic}", "User-Agent": USER_AGENT},
    )
    with urllib.request.urlopen(req, timeout=20) as r:
        return json.load(r)["access_token"]


def new_posts(tok, sub, limit):
    req = urllib.request.Request(
        f"https://oauth.reddit.com/r/{sub}/new?limit={limit}",
        headers={"Authorization": f"Bearer {tok}", "User-Agent": USER_AGENT},
    )
    with urllib.request.urlopen(req, timeout=20) as r:
        return [c["data"] for c in json.load(r)["data"]["children"]]


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--out", default="data/reddit_posts.jsonl")
    ap.add_argument("--limit", type=int, default=100)
    a = ap.parse_args()
    cid, secret = os.environ.get("REDDIT_CLIENT_ID"), os.environ.get("REDDIT_CLIENT_SECRET")
    if not (cid and secret):
        print("REDDIT_CLIENT_ID / REDDIT_CLIENT_SECRET not set: Reddit skipped")
        return 0
    tok = token(cid, secret)
    posts = []
    for sub in SUBREDDITS:
        for d in new_posts(tok, sub, a.limit):
            # Point in time: id, creation time and text only. Score and
            # comment counts accrue later and are deliberately not kept.
            posts.append({
                "id": f"reddit-{d['id']}",
                "published_ts": int(d["created_utc"]) * 1000,
                "source": f"reddit/r/{sub}",
                "kind": "post",
                "synthetic": False,
                "url": f"https://www.reddit.com{d['permalink']}",
                "text": (d.get("title") or "")[:280],
            })
    posts.sort(key=lambda p: (p["published_ts"], p["id"]))
    with open(a.out, "w", newline="\n", encoding="utf-8") as f:
        for p in posts:
            f.write(json.dumps(p, ensure_ascii=False) + "\n")
    print(f"wrote {len(posts)} Reddit posts to {a.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
