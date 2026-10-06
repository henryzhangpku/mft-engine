# mft-engine

Reasoning signals from social and prediction-market data, gated by code, at
mid frequency. A small Rust engine takes live crypto prices (Hyperliquid),
prediction-market strike ladders (Kalshi) and social posts scored by a
reasoning classifier (TypeSafe's Jev, on Hacker News), turns them into one
stream of events, and runs one strategy and one risk layer over that stream.
The same code runs in replay and live on paper, so research and production
cannot disagree.

**Demo:** [henryzhangpku.github.io/mft-engine](https://henryzhangpku.github.io/mft-engine/)
(a replay of the recorded data below; paper only).

**Paper only.** There is no order router, no request signing and no exchange
key handling anywhere in the crate. A test fails the build if the source ever
mentions an order endpoint or signing code for Hyperliquid or Kalshi, and
another fails if anything secret-shaped appears in `data/`, `results/`,
`docs/` or `experiments/`.

**The honest result:** both strategies lose money after costs on the
committed sample, in both halves of it. The reasoning-gated version (v2)
loses less than plain momentum (v1), mostly by trading less. The sample is
3.5 days, which is far too short to call that an edge.

## What it does, in one picture

```
 Hyperliquid ws        Kalshi REST (KXBTCD, KXETHD)      Hacker News (Algolia)
 trades, book tops     hourly "above strike K" ladders   stories + comments
       |                         |                              |
   feed.rs                  kalshi.rs                  sidecar/ (Python)
   reconnect, gaps          bid/ask mid per strike      Jev: about BTC? about ETH?
       |                         |                      bullish/bearish/neither?
   bars.rs                  prediction.rs               new or repost?
   trades -> 1m bars        clean ladder -> P(close>K)        |
       |                         |                      TextSignal JSONL
       +------------+------------+------------+---------------+
                    |   one Event enum, one channel, time order
                    v
          event_loop::run  (the same loop in backtest and paper)
                    |
   engine.rs  1. market state
              2. strategy.rs: momentum target
              3. gates: Kalshi agreement, social caution  (can only shrink)
              4. target - position = order intent
              5. risk.rs: every rule must allow; an error blocks
              6. execution.rs: paper fill, fees, slippage, PnL
                    |
              Decision (filled, or blocked with a reason)

   Clock: event time in replay, wall time live. Nothing else differs.
   Research: experiment.rs + ledger.rs (hash-chained results), demo.rs (web export)
```

## The two strategies

**v1, momentum (fixed before the first backtest, never tuned).**
Volatility-normalised 5-minute momentum on 1-minute bars, per coin:

| parameter | value |
|---|---|
| return horizon | 5 bars: `r5 = ln(close_t / close_{t-5})` |
| volatility | std of the last 60 one-minute log returns |
| score | `z = r5 / (sigma * sqrt(5))` |
| entry | long if `z >= 2.0`, short if `z <= -2.0` |
| exit | when `z` changes sign against the position, or after 15 bars |
| size | $1,000 target per coin; differences under $50 are not traded |

**v2, reasoning-gated.** The same v1 targets, then two gates that can only
reduce exposure:

1. **Prediction-market agreement.** Kalshi's hourly KXBTCD and KXETHD events
   are ladders of binary contracts, "price above strike K at the top of the
   hour". The yes mid of each contract is the market's P(close > K). After
   cleaning (quotes with a spread over 20 cents dropped, probabilities forced
   non-increasing in K) the engine reads P(close > current spot) off the
   ladder by linear interpolation. **A new long is taken only if that
   probability is above 0.5; a new short only if it is below 0.5.** A
   missing, stale (over 3 minutes) or out-of-range ladder means no entry: the
   gate fails closed. Held positions are not re-gated (see the note below).
2. **Social caution.** For each Jev-scored post judged relevant to the coin
   (relevance at least 0.5) in the last 30 minutes, the probability that
   *opposes* the position, times relevance and novelty, scales the target by
   `1 - strength`, and vetoes it at 0.6. A bullish post never adds to a long.

Both gates are a `Caution`, a multiplier that can only hold a value in [0, 1],
and a separate check blocks any order whose target the gates made larger or
flipped. Text and prediction markets can make a decision more cautious, never
less.

**How preregistered is v2, exactly.** v2's rules were written in code and in
`experiments/ideas.toml` before its first run. That first run re-checked the
Kalshi gate on every bar while a position was open, and the target flickered
between $1,000 and flat as P(up) wobbled around 0.5, which churned fees (323
fills against v1's 294). I then changed the gate to apply to entries only.
That change was made after seeing a result, so it is disclosed here and the
original version is on the experiment ledger as `v2_gate_every_bar`, with its
numbers. Nothing else was changed after a run. This README was written after
the runs.

## Results (replay of recorded data)

Data, all committed under `data/`:

| source | what | real or not |
|---|---|---|
| `bars_1m.jsonl` | Hyperliquid 1-minute candles, BTC and ETH, 2026-10-02 14:47 to 10-06 03:17 UTC, 10,139 bars | real (all the 1m history the endpoint keeps) |
| `kalshi_ladders.jsonl` | 9,222 one-minute ladder snapshots from 80 hourly BTC events and 80 hourly ETH events over the same window, backfilled from Kalshi's public 1-minute candlesticks (bid/ask at each minute's close, 10 strikes either side of spot) | real |
| `hn_posts.jsonl` | 259 Hacker News items in the window (20 stories, 239 comments) matching bitcoin, ethereum, crypto, stablecoin or coinbase | real |
| `text_signals.jsonl` | those 259 posts scored by Jev (518 signals, one per coin per post) | real model output |
| `tests/fixtures/synthetic_*` | 60 invented posts, mock-scored | synthetic, tests only |

Point in time: a bar is stamped with its close; a Kalshi snapshot with the end
of its minute; a post becomes usable at publication plus a 60 s poll delay
plus the measured Jev latency. Only the post's text is scored; HN points and
comment counts, which accrue later, are not stored.

Output of `mft-engine backtest` (costs: 4.5 bp taker fee plus 1 bp slippage
per fill, about 11 bp per round trip):

```
strategy                   window        fills     hit    pnl_net  pnl_gross     costs   turn_x   max_dd  vetoes
v1_momentum                full            294    2.0%    -197.78     -34.97    162.81    296.0   198.00       0
v1_momentum                first_half      179    2.2%    -115.89     -16.89     99.00    180.0   115.89       0
v1_momentum                second_half     175    5.7%    -110.95     -14.14     96.81    176.0   111.18       0
v2_reasoning_gated         full            267    5.2%    -152.97      -5.80    147.17    267.6   153.20     823
v2_reasoning_gated         first_half      133    1.5%     -76.52      -4.07     72.45    131.7    76.52     413
v2_reasoning_gated         second_half     127    9.4%     -71.23      -0.90     70.32    127.9    71.45     393
```

Columns: fills; share of round trips with positive PnL after costs; PnL after
costs (USD); PnL before fees and slippage; fees plus slippage; traded notional
over the $1,000 target; max drawdown on the per-bar equity curve; bars where
the Kalshi gate vetoed an entry.

What this says, plainly:

* **Neither strategy makes money.** v1 loses before costs too. v2's gross PnL
  is close to zero (-$5.80) and costs then make it clearly negative.
* **v2 loses $44.81 less than v1**, in both halves. Most of the difference is
  fewer trades and therefore lower costs ($147 against $163), plus a gross
  loss that is $29 smaller. With 267 fills over 3.5 days, that difference is
  well within what noise could produce. It is a reason to collect more data,
  not a finding.
* **The social feature barely matters here.** Of 259 real posts, Jev judged 11
  relevant to BTC (one with bearish probability over 0.5) and 1 relevant to
  ETH. The social gate changed a target 51 times and moved PnL by about 30
  cents either way: $0.34 (v2 against `v2_kalshi_only`) and $0.29
  (`v2_social_only` against v1), on the ledger.
  Hacker News is a thin source for crypto. Reddit (below) would be thicker.
* v1 hit its $50 daily loss limit on every full UTC day (695 blocks, all
  `max_daily_loss`). v2 never did.
* A caveat on the Kalshi feature: P(up) compares Kalshi's settlement index
  with Hyperliquid's perpetual price, so the basis between the two tilts it.
  On this sample it leaned slightly above 0.5 more often than below.
* Determinism: decision fingerprints `afb924fee4ed2223` (v1) and
  `86f02dbcc8fa2493` (v2) repeat on every run; the tests check that two
  replays give identical decisions, reports and equity curves.

## Experiment ledger ("idea to live experiment fast")

`mft-engine experiment` runs every variant in `experiments/ideas.toml` (a base
strategy, a hypothesis written first, optional config overrides by dotted
path) through the same backtester and appends each result to
`results/ledger.jsonl`. Each entry stores the SHA-256 of the entry before it
and of its own contents, plus the SHA-256 of the experiment file and of the
input data. Editing a past verdict, or quietly dropping a bad idea, breaks the
chain, and `mft-engine experiment --verify` says where. The verdict rule is
fixed in code: kept only if the second-half (holdout) PnL after costs is
positive and beats the baseline's.

| # | variant | verdict | PnL | holdout PnL | fills |
|---|---|---|---|---|---|
| 0 | v1_momentum | baseline | -197.78 | -110.95 | 294 |
| 1 | v2_reasoning_gated | killed | -152.97 | -71.23 | 267 |
| 2 | v2_gate_every_bar | killed | -175.63 | -91.07 | 323 |
| 3 | v2_kalshi_only | killed | -153.31 | -71.34 | 266 |
| 4 | v2_social_only | killed | -197.49 | -110.73 | 294 |
| 5 | v2_kalshi_strict_0p6 | killed | -38.85 | -7.24 | 77 |
| 6 | v1_entry_z_3 | killed | -66.37 | -30.65 | 94 |

All seven were killed: nothing has a positive holdout. The two that lose least
(strict Kalshi agreement, and a higher momentum threshold) do so by trading a
quarter to a third as often; neither is positive before costs in its holdout.

## Live paper run

`mft-engine paper --duration-secs 600 --text-feed results/live_text_signals.jsonl`
(strategy v2), started 2026-10-06 04:22:20 UTC, with `sidecar/live_social.py`
running alongside it against real Jev. All three live sources ran:

* Hyperliquid websocket: 1,421 trades, 226 book tops, 22 one-minute bars.
* Kalshi, polled every minute: 20 ladder snapshots (BTC and ETH). A typical
  line: `KXBTCD-26OCT0601 7 strikes, implied median 85598, P(close > spot 85595.0) = 0.509`.
* Hacker News plus Jev: 2 new posts in the window, scored live (p50 111 ms,
  530 input tokens per post), 4 `TextSignal` events tailed by the engine.
  Neither post was about crypto (relevance under 0.5), so neither mattered.

**No decisions were made.** The momentum signal never reached |z| >= 2 in
those ten minutes. A v1 control run started at the same moment also made no
decisions, and the v2 run logged 0 Kalshi vetoes, so the quiet run was the
market, not the gate. Two earlier 5- and 10-minute v2 runs (04:05 and 04:11
UTC) were also quiet. The SIGNAL line format, from an earlier v1 session of
this engine:

```
SIGNAL 2026-10-06T03:33:02Z ETH-PERP SHORT target -1000 USD (order -0.37124 ETH) | reason: 5m momentum z=-3.17 | risk: PASSED, paper fill -0.37124 @ 2693.43 fee 0.4500
```

v2 lines add the gate inputs to `reason`; the first v2 decision in the backtest trail (an ETH exit) reads
`5m momentum z=-0.80; kalshi P(up)=0.50 median=2694 gate x1; social x1.00`.

Latency in the 04:22 run (`results/paper.json`):

| from websocket frame read to decision | samples | p50 | p99 |
|---|---|---|---|
| all events | 1,693 | 418 us | 1,775 us |
| bar events (the ones that can trade) | 22 | 353 us | 1,713 us |
| inside `Engine::on_event` only | 1,693 | 1.8 us | 47.3 us |

The engine itself takes a couple of microseconds; the rest is channel hops and
queueing behind other trades in the same websocket frame. 22 bar samples are
too few for a stable p99. Exchange-to-receive was 312 ms at p50; its p99
(17.8 s) is the batch of recent trades Hyperliquid replays on subscribe.

Every event the live engine saw is in `results/paper_events.jsonl`, and the
live Jev output in `results/live_text_signals.jsonl`, so the session can be
replayed through `backtest --data results/paper_events.jsonl`. The 90 bars of
REST history used to warm the strategy up are not in that file, so a replay
starts cold.

## Running it

Built and tested with Rust 1.93 on Windows. `backtest`, `experiment`,
`export-demo` and `cargo test` run offline from the committed data.

```
cargo build --release
cargo test

./target/release/mft-engine backtest                 # v1 and v2 side by side -> results/backtest.json
./target/release/mft-engine experiment               # experiments/ideas.toml -> results/ledger.jsonl
./target/release/mft-engine experiment --verify      # check the hash chain
./target/release/mft-engine export-demo              # -> docs/data/demo.json

# Live, public data, no keys needed except Jev's.
./target/release/mft-engine record --duration-secs 120
./target/release/mft-engine fetch-bars --days 4
./target/release/mft-engine fetch-kalshi             # backfill ladders over the bar window
./target/release/mft-engine paper --duration-secs 600 --text-feed results/live_text_signals.jsonl
```

The demo is static: `cd docs && python -m http.server`, then open
`http://localhost:8000`. GitHub Pages serves the same folder.

### Social data and Jev

The sidecar is Python, because that is where the Jev SDK is. Use a virtual
environment (the build machine's global Python hit a TLS `RecursionError`
inside `truststore` with `typesafe-sdk`):

```
python -m venv .venv
.venv/Scripts/pip install -r sidecar/requirements.txt     # .venv/bin/pip off Windows
export TYPESAFE_API_KEY=...                                 # in your shell only

python sidecar/fetch_hn.py                                  # real HN posts over the bar window
.venv/Scripts/python sidecar/score_posts.py                 # Jev scores -> data/text_signals.jsonl
.venv/Scripts/python sidecar/live_social.py --minutes 11    # live: poll, score, append; paper tails it
```

The SDK reads `TYPESAFE_API_KEY` from the environment; nothing in this
repository reads, prints or stores it, and every file the sidecar and engine
write is checked for it (and for token-shaped strings) before it is written.
Without a key, scoring falls back to a deterministic keyword mock labelled
`mock-keyword-v1`.

One Jev `system_one` call per post asks five typed questions: is it about BTC
(`Noul`), is it about ETH (`Noul`), the implied direction for each over the
next hour (`Choice`: bullish, bearish, neither), and is it new information
(`Noul`). Measured on the 259 backfilled posts
(`sidecar/scoring_stats_jev.json`): **p50 118 ms, p99 230 ms (first call
397 ms), 546 input tokens per post.**

**Reddit** is supported only through Reddit's official OAuth API
(`sidecar/fetch_reddit.py`), because the anonymous JSON endpoints are blocked
for scripts. It runs when `REDDIT_CLIENT_ID` and `REDDIT_CLIENT_SECRET` are
set (a "script" app at reddit.com/prefs/apps). No Reddit credentials were
available, so it has not been run and no Reddit data is committed.

## Risk, in code, failing closed

Every order passes every rule; a rule returns allow, block, or an error, and
an error blocks. An engine with no rules blocks everything.

| rule | default |
|---|---|
| sane inputs | non-finite or non-positive price or quantity is an error |
| stale data | no market data newer than 90 s by its exchange timestamp blocks |
| max order | $2,500 notional |
| max position | $1,500 notional per coin (reductions always allowed) |
| max daily loss | $50 per UTC day, fees included; then only reducing orders |
| gate check | any order whose target the reasoning gates enlarged or flipped |

Every block becomes a `Decision::Blocked` with the rule and reason, in the
decision trail (`results/backtest_decisions_v1.jsonl`, `_v2.jsonl`) and the demo.

## Modules

| file | what it does |
|---|---|
| `src/event.rs` | the one `Event` enum: Trade, BookTop, Bar, TextSignal, PredictionMarket, Gap |
| `src/engine.rs` | the shared decision path: one event in, at most one decision out |
| `src/strategy.rs` | v1 momentum and its position state machine |
| `src/prediction.rs` | ladder cleaning, P(close > x), implied quantiles, the v2 gate |
| `src/text.rs` | the social gate: a [0, 1] multiplier and the "never riskier" check |
| `src/risk.rs` | risk rules as trait objects; the fail-closed `RiskEngine` |
| `src/execution.rs` | paper fill model, cash-based PnL, round trips |
| `src/event_loop.rs` | the single loop both modes run, with latency measurement |
| `src/kalshi.rs` | Kalshi public markets and candlesticks; live ladder and backfill |
| `src/feed.rs`, `src/gap.rs`, `src/hyperliquid.rs`, `src/bars.rs` | live feed, reconnects, gaps, bars |
| `src/experiment.rs`, `src/ledger.rs` | TOML variants, hash-chained ledger |
| `src/artifacts.rs` | every file write; refuses anything secret-shaped |
| `src/demo.rs` | `export-demo` |
| `src/backtest.rs`, `src/paper.rs`, `src/record.rs`, `src/fetch.rs` | the subcommands |
| `sidecar/` | HN and Reddit fetchers, Jev scorer, live social feed, secret guard |
| `docs/` | the static demo (index.html, app.js, style.css) |

## What is not done

* **No real orders.** No order endpoint, no signing, no exchange keys. Paper
  fills only. Anyone acting on a paper SIGNAL line does so by hand.
* **Venues:** Hyperliquid (BTC and ETH perpetuals) for prices; Kalshi hourly
  KXBTCD and KXETHD ladders for prediction markets. Kalshi's KXBTC range
  series and Polymarket are not used: the threshold ladder already gives the
  distribution directly.
* **Social data is thin.** 259 Hacker News items, 12 judged relevant. Reddit
  is implemented but unrun (no credentials). No X/Twitter.
* **Short history.** 3.5 days of 1-minute bars, the most Hyperliquid keeps.
  Kalshi history goes back further; a longer test needs a longer price
  history (recorded with `record`, or another source).
* **No Jev-versus-plain-LLM comparison** on cost and latency yet.
* Gap detection on Hyperliquid is heuristic (no sequence numbers).
* The fill model ignores queue position, partial fills, funding and impact
  beyond 1 bp. Money is `f64`; production would use integer ticks.
* The backfilled Kalshi ladder uses the bid/ask at each minute's close from
  1-minute candles, not the full order book.

## Design choices

* **One code path.** `Engine::on_event` is the only place decisions are made;
  both modes reach it through `event_loop::run`. A test replays the same data
  through the async loop and through a plain loop and requires identical
  decisions, for v1 and v2.
* **Reasoning features gate, code decides.** Jev and the prediction market
  supply probabilities; deterministic code turns them into a multiplier that
  can only shrink a position, and a check enforces that.
* **Fail closed.** Missing or stale prediction-market data means no entry; a
  risk rule that errors blocks; non-finite numbers are errors.
* **Deterministic replay.** Event time drives the clock, maps are `BTreeMap`,
  events are sorted by time, coin and kind, and decisions are fingerprinted.
* **Point in time everywhere.** Bars stamped at close, Kalshi snapshots at
  minute end, posts at publication plus poll delay plus scoring latency.
* **Research is recorded, not remembered.** The ledger keeps killed ideas next
  to kept ones and cannot be edited quietly.
* **Minimal dependencies:** tokio, tokio-tungstenite, futures-util, serde,
  serde_json, ureq, native-tls, anyhow, clap, sha2, toml.

## License

MIT. See `LICENSE`.
