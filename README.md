# mft-engine

A deterministic mid-frequency research and paper-trading engine in Rust, with
one event loop for backtest and live: point-in-time data → features → signals
→ overlays → portfolio and risk → paper execution → a research ledger (the
pipeline is laid out below). Concretely, it takes live crypto prices (Hyperliquid),
prediction-market strike ladders (Kalshi, and Polymarket), social posts scored by a
reasoning classifier (TypeSafe's Jev, on Hacker News) and the positioning of
the top wallets on Hyperliquid's public leaderboard, turns them into one
stream of events, and runs one strategy and one risk layer over that stream.
The same code runs in replay and live on paper, so research and production
cannot disagree.

**Demo:** [henryzhangpku.github.io/mft-engine](https://henryzhangpku.github.io/mft-engine/)
(a replay of the recorded data below; paper only).

**Paper only.** There is no order router, no request signing and no exchange
key handling anywhere in the crate. A test fails the build if the source ever
mentions an order endpoint or signing code for Hyperliquid, Kalshi or
Polymarket (Polymarket is read from public GET endpoints only), and
another fails if anything secret-shaped appears in `data/`, `results/`,
`docs/` or `experiments/`.

**What it proves, measured on live data:** the engine decides in about
2 microseconds (p99 47), a live feed frame reaches a decision in about
0.4 ms, Jev reasons over a post in about 120 ms, and replay reproduces every
decision to the same fingerprint. Kalshi ladders (9,222 minute snapshots),
Polymarket ladders (9,490) and 259 real posts flow through the same event loop as prices.

**Results, as they came out:** both pre-registered strategies lose after
costs on 3.5 days of data; v2 loses less than v1, mostly by trading less.
v3, gated by what the top Hyperliquid wallets hold, can only be tested on
data recorded live (positioning has no history); on one 85-minute session it
took 4 fills and lost $3.06, against v1's 12 fills and $9.79. All of that is
far too short a sample to call an edge, and the numbers are below in full.
v2b adds Polymarket: a new entry needs Kalshi's hourly and Polymarket's
daily ladder to agree. Pre-registered, then run once: it halves the trades
(134 fills against v2's 267) and the costs, and loses $82.76 against v2's
$152.97, still a loss, and killed by the ledger's rule. The two markets put
P(up) on the same side of 0.5 at only 54% of the bars where both read.
v4, a cross-sectional funding-carry strategy across 30 Hyperliquid perps, was
pre-registered on the ledger and tested on 90 days of hourly data, with a
sealed out-of-sample window run once: **it was killed**, losing $478.69 on
$10,000 gross over the sealed 36 days (Sharpe -1.91). It collected the
funding it was built to collect and lost far more on the price leg.
v5 takes the momentum family to a 24-hour horizon on hourly bars, to cut
turnover: pre-registered, then run once on 208 days of BTC and ETH. It
traded about 1/47th as much per day as v1 and made +$269.51 after costs
(Sharpe +0.74), but lost $54.66 in the first half, so **it was killed** by
its own pre-registered rule (positive on the full window and both halves).
A forward paper run of v5 on live hourly candles started 2026-10-09 16:11 UTC.

## The pipeline: from data to a trade

Point-in-time data becomes features, features become two alpha signals,
overlays can only cut them, the portfolio layer sizes and risk-checks the
target, and execution is paper. The same code replays history, and every
idea goes through the ledger before it counts.

```
 1. DATA             Hyperliquid trades & book · Kalshi ladders · Polymarket      feed.rs · kalshi.rs
    (point-in-time)  ladders · HN posts · top-trader positions · funding rates    polymarket.rs · event.rs
                     every record stamped with when it was KNOWABLE, merged
                     into one time-ordered stream
        │
 2. FEATURES         1-min bars, 60-min volatility · implied P(close > spot)      bars.rs · prediction.rs
                     from each ladder · post direction & relevance (Jev)          text.rs · positioning.rs
                     · crowd long share · funding-rate ranks
        │
 3. SIGNALS (alpha)  momentum z-score, 5-min, vol-normalised    (v1, v2, v3)      strategy.rs
                     momentum z-score, 24-hour, hourly bars     (v5)              strategy.rs
                     funding-carry rank across 30 perps         (v4)              carry.rs
                     the only two things that can open a position
        │
 4. OVERLAYS         prediction-market agreement · news caution · crowd          engine.rs (gates)
                     positioning; each a multiplier in [0, 1]:
                     can shrink or veto, never add or flip
        │
 5. PORTFOLIO        target $ per coin → target − position = order intent        engine.rs · risk.rs
    & RISK           every risk rule must approve; missing or stale
                     input, or an error → no trade (fails closed)
        │
 6. EXECUTION        paper fill: fees + slippage, P&L; every decision logged     execution.rs · paper.rs
                     with its inputs; no order code anywhere in the repo
        │
 7. FEEDBACK         replay = same code on history → results → hash-chained      experiment.rs · ledger.rs
    (research loop)  ledger; ideas written down before the test, judged once     verify.rs
                     on sealed data, kept or killed
```

| strategy | signal (layer 3) | overlays (layer 4) |
|---|---|---|
| v1 | momentum | none |
| v2 | momentum | prediction-market agreement (Kalshi) + news caution |
| v2b | momentum | Kalshi AND Polymarket agreement + news caution |
| v3 | momentum | crowd positioning |
| v4 | funding carry | none |
| v5 | momentum, 24-hour horizon on 1-hour bars | none |

**No combination layer, by design:** one alpha source per strategy keeps each
test clean. Combining signals into a weighted composite under risk
constraints would be the next layer.

## Event flow in the code

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

   Also from Hyperliquid's public info API (pollers.rs, every few minutes):
   positioning.rs: top-100 leaderboard wallets -> clearinghouseState -> per-coin
   long/short totals -> Positioning events -> the v3 gate (can only shrink)
   universe.rs: every perp on every dex (main + HIP-3 builder dexes)

   Clock: event time in replay; live, the arrival time of each event, recorded
   with it, so a replay by arrival runs on the live clock. Nothing else differs.
   Research: experiment.rs + ledger.rs (hash-chained results), demo.rs (web export)
```

## The strategies

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

**v2b, two prediction markets must agree.** v2, plus one condition on new
entries: Polymarket's daily ladder must lean the same way as Kalshi's hourly
one. Polymarket lists, for BTC and ETH, a daily event "above ___ on
<date>?": 11 binary markets, one per strike ($2,000 apart for BTC, $100 for
ETH), each resolving on the Binance 1-minute candle at 12:00 ET (16:00 UTC
in October). The Yes price is P(close > K), so the event is the same kind of
ladder as Kalshi's and is read by the same code (`clean_ladder`, the same
97%/3% trim, linear interpolation, no extrapolation), from the event
resolving next. **A new long needs P(close > spot) above 0.5 on both
ladders; a new short, below 0.5 on both.** Either one missing, older than 3
minutes, past its resolution, or not spanning spot means no entry. Held
positions are not re-gated, as in v2. It is the same `Caution`, so it can
only remove exposure. The mapping is honest but not like for like: Kalshi's
horizon is the top of the next hour, Polymarket's is noon ET, anywhere from
a minute to 24 hours away, and the two settle on different prices (Kalshi's
index, Binance spot), both compared with Hyperliquid's perpetual. With
strikes $2,000 apart, a Polymarket ladder that is near-certain on both sides
of spot has fewer than three informative strikes and gives no reading; v2b
then cannot enter. v2b was written in `experiments/polymarket_v2b.toml` and
on the ledger (entries 18 to 20, `experiment --preregister`) and committed
before its first run; `experiment` refuses to run that file without them.

**v3, positioning-gated.** The same v1 targets, entered only when the crowd
of top Hyperliquid traders leans the same way: in the latest positioning
snapshot (under 15 minutes old), the top 100 leaderboard wallets by 30-day
PnL (account value at least $100k) must hold **more than 60% of their gross
position value in this coin on our side**, and at least 5 of them must hold
it. Missing, stale or thin positioning means no entry. Held positions are not
re-gated. The 60% line is the old Python analyser's "bullish/bearish" rule,
kept as it was. v3 was written down (in code and in
`experiments/positioning.toml`) before any run.

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
| `polymarket_ladders.jsonl` | 9,490 one-minute ladder snapshots (BTC 4,584, ETH 4,906) from 10 daily Polymarket events (Oct 2 to 6, BTC and ETH, 11 strikes each), built from the public CLOB price history at 1-minute fidelity; `polymarket_markets.jsonl` lists the 110 markets (question, strike, resolution time, Yes/No token ids); `polymarket_raw/` holds every raw Gamma and CLOB response | real |
| `kalshi_ladders.jsonl` | 9,222 one-minute ladder snapshots from 80 hourly BTC events and 80 hourly ETH events over the same window, backfilled from Kalshi's public 1-minute candlesticks (bid/ask at each minute's close, 10 strikes either side of spot) | real |
| `hn_posts.jsonl` | 259 Hacker News items in the window (20 stories, 239 comments) matching bitcoin, ethereum, crypto, stablecoin or coinbase | real |
| `text_signals.jsonl` | those 259 posts scored by Jev (518 signals, one per coin per post) | real model output |
| `tests/fixtures/synthetic_*` | 60 invented posts, mock-scored | synthetic, tests only |

Point in time: a bar is stamped with its close; a Kalshi snapshot with the end
of its minute; a Polymarket snapshot with the end of the minute its prices
were printed in (each strike's last price at or before that instant, dropped
if older than 5 minutes); a post becomes usable at publication plus a 60 s poll delay
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
* Determinism: decision fingerprints `5ccac06aa73491d6` (v1) and
  `ce040be98f47646d` (v2) repeat on every run; the tests check that two
  replays give identical decisions, reports and equity curves.

### v2b: Kalshi and Polymarket must both agree (ledger 18 to 23)

Pre-registered on the ledger (entries 18 to 20) and committed before any
run, then run once with `mft-engine experiment --file
experiments/polymarket_v2b.toml`, same window, same costs, same verdict rule.
The file reruns v1 and v2 with the Polymarket ladders in the stream as
controls: both repeat entries 0 and 1 to the decision fingerprint
(`5ccac06aa73491d6`, `ce040be98f47646d`), so adding the second venue changed
nothing for the strategies that do not read it.

```
strategy                   window        fills     hit    pnl_net  pnl_gross     costs   turn_x   max_dd  vetoes
v1_momentum                full            294    2.0%    -197.78     -34.97    162.81    296.0   198.00       0
v2_reasoning_gated         full            267    5.2%    -152.97      -5.80    147.17    267.6   153.20     823
v2b_kalshi_and_polymarket  full            134    4.5%     -82.76      -9.21     73.55    133.7    82.76    1236
v2b_kalshi_and_polymarket  first_half       65    0.0%     -40.58      -5.53     35.05     63.7    40.58     619
v2b_kalshi_and_polymarket  second_half      65    9.1%     -40.13      -3.83     36.30     66.0    40.13     588
```

(v1 and v2 halves as in the table above. "vetoes" counts bars where either
market vetoed an entry.)

What this says:

* **Fewer trades, as predicted, and no edge.** v2b takes half of v2's fills
  and pays half its costs, so it loses $70 less. Before costs it is slightly
  *worse* than v2 (-$9.21 against -$5.80): the second market removed trades,
  not bad trades. Both halves lose about the same. Killed by the verdict rule
  (holdout PnL negative), as the hypothesis expected.
* **Too short a sample to call anything.** 3.5 days, 65 fills a half.
* **The two markets often disagree.** At the 8,815 bars where both ladders
  gave a reading (the gate's own view: fresh, unexpired, spanning spot),
  Kalshi's and Polymarket's P(close > spot) were on the same side of 0.5 at
  54.2% of them: 62.8% for BTC (4,263 bars), 46.0% for ETH (4,552 bars),
  barely better than a coin for BTC and worse than one for ETH. That is less
  a verdict on either market than on the comparison: different horizons (the
  next hour against noon ET), different settlement prices (Kalshi's index,
  Binance spot), both read against Hyperliquid's perpetual, so a few dollars
  of basis decide the side when P is near 0.5. A gate that needs both is
  mostly a gate that trades less.
* **Coverage.** Polymarket gave a reading at 8,815 of the 10,139 bars. With
  strikes $2,000 apart, BTC had two stretches (about 3 and 4 hours before a
  noon resolution) where the ladder was near-certain on both sides of spot
  and had fewer than three informative strikes: no reading, so v2b could not
  enter there. That is the fail-closed rule working, not missing data.

## v5: lower turnover, pre-registered (ledger 24 and 25)

The baselines lost because 5-minute momentum's turnover (about 300x the book
in 3.5 days) cannot clear 5.5 bp per fill, whatever the signal. v5 tests the
same family at a 24-hour horizon with about 1/50th of the trading. The spec
was written in `experiments/hourly_trend_v5.toml` and put on the ledger
(entry 24, `experiment --preregister`, commit 62ba778) before any result
existed; the file sets `require_preregistration`, so it cannot be run from
an edited copy.

| item | value |
|---|---|
| coins | BTC and ETH, Hyperliquid perpetuals |
| bars | 1-hour candles from the public `candleSnapshot` endpoint, stamped at their close |
| signal | `r24 = ln(close_t / close_{t-24})`, `sigma` = std of the last 168 hourly log returns, `z = r24 / (sigma * sqrt(24))` |
| entry | long if `z >= 1.0`, short if `z <= -1.0` |
| exit | when `z` crosses 0 against the position, or after 72 bars |
| size | $1,000 target per coin; differences under $50 not traded |
| costs | 4.5 bp taker + 1 bp slippage per fill |
| overlays | none; the same risk layer as v1 (fails closed) |
| data | all the hourly history the endpoint keeps: 5,002 bars per coin, 2026-03-15 06:00 to 10-09 16:00 UTC (208 days), `data/bars_1h.jsonl` |
| verdict rule | kept only if net PnL after costs > 0 on the full window AND in both halves; otherwise killed |

It is `EngineConfig::v5()`: v1's code and state machine with
`MomentumParams::v5_hourly()`, on the same engine, risk layer and fills.

### Historical result, run once (ledger 25)

`mft-engine experiment --file experiments/hourly_trend_v5.toml`:

```
strategy                   window        fills     hit    pnl_net  pnl_gross     costs   turn_x   max_dd  vetoes
v5_hourly_trend            full            391   34.6%     269.51     477.31    207.80    377.8   559.85       0
v5_hourly_trend            first_half      203   33.0%     -54.66      52.17    106.83    194.2   559.85       0
v5_hourly_trend            second_half     183   34.8%     208.15     306.85     98.70    179.5   228.58       0
```

| window | net PnL | Sharpe (daily, annualised) | hit rate (round trips) | turnover | max drawdown |
|---|---|---|---|---|---|
| full, 208 days | +$269.51 | +0.74 | 34.6% of 188 | 378x ($378k traded) | $559.85 |
| first half | **-$54.66** | -0.34 | 33.0% of 97 | 194x | $559.85 |
| second half | +$208.15 | +1.07 | 34.8% of 89 | 180x | $228.58 |

**Killed.** The first half loses after costs, so the pre-registered rule
says killed, whatever the full window says.

What this says:

* **Turnover came down as designed.** 1.8x the book per day against v1's
  85x: about 1/47th. v1 paid $162.81 in costs on a $34.97 gross loss in
  3.5 days; v5 paid $207.80 on a $477.31 gross gain in 208 days.
* **The edge, if there is one, is not stable.** A 35% hit rate with a
  positive full window is the trend-following shape (small losses, a few
  large wins). It made +$52 before costs in the first half and +$307 in the
  second: one half is nearly flat before costs, and costs then take it below
  zero. Two halves of about 100 days, BTC and ETH only, are not enough to
  tell a horizon effect from a few big moves.
* **Deflated:** `ledger dsr --entry 25` gives PSR against zero 0.72 over 209
  days (skew +2.4, kurtosis 15), DSR 0.0000 against all 20 trials on the
  ledger, 0.28 against the 4 daily ones. Not evidence of an edge.
* **The risk layer bit.** The $50 daily-loss stop blocked 38 risk-adding
  orders over the 208 days (28 in the first half), as designed.

### Forward paper run (live since 2026-10-09 16:11 UTC)

A pre-registered strategy that is killed on history can still be watched
forward at no cost, on paper. `mft-engine paper-hourly` is the live paper
mode for an hourly strategy: the same engine, fed each closed 1-hour candle
from the public endpoint 15 seconds after the hour, clocked by its arrival
time, paper fills only (no orders, no keys; the paper-only guard tests pass).
It appends every event it consumes to `results/forward_v5/session.jsonl`
(warm-up first) and every decision to `decisions.jsonl` and `signals.log`; on
a restart it replays its own session log on the recorded clocks to rebuild
its exact state, then fetches any hours it missed (late bars are blocked by
the stale-data rule: fails closed). Each hour it rewrites `status.json` and
`daily_summary.csv` (committed; the logs are not).

On this machine it runs as a Windows scheduled task, `mft-engine-v5-forward`
(at logon, restart on failure, no admin), started 2026-10-09 16:11:16 UTC:

```
powershell -File scriptsorward_v5.ps1 status   # task state, status.json, last signals
powershell -File scriptsorward_v5.ps1 stop     # stop (state kept); "start" resumes
powershell -File scriptsorward_v5.ps1 remove   # stop and unregister the task
powershell -File scriptsorward_v5.ps1 install  # build, copy the binary, register, start
```

The task runs a copy of the binary in `results/forward_v5/bin`, so
rebuilding the repo never collides with the running process.

## Hyperliquid universe and crowd positioning

Ported, read-only, from the owner's older Python tools (a perp universe
scanner, a leaderboard analyser, wallet and contract sentiment). Only the
public market-data side was ported; nothing that signs, holds a key or
places an order came across, and no address from the old code is used. All
wallet addresses are fetched from the public leaderboard at runtime and never
written to disk.

**Universe** (`mft-engine universe`, `src/universe.rs`). `perpDexs` lists the
dexes and `metaAndAssetCtxs` (with a `dex` field) returns each one's contracts
and live context. At 2026-10-06 05:17 UTC, 11 dexes were listed and 5 had live contracts:
331 live perps (main 178, xyz 110, para 29, io 9, mkts 5) plus 203 delisted. Builder (HIP-3)
dexes carry equities, indices and commodities: `xyz:SP500`, `xyz:NVDA`,
`xyz:CL`, `xyz:SILVER` and so on. Saved to `data/universe.json` with size
decimals, max leverage, mark, open interest, volume and funding.

**Positioning** (`mft-engine positioning`, `src/positioning.rs`). The wallet
set is fixed once per run: the top 100 leaderboard accounts by 30-day PnL with
at least $100k account value. Each snapshot reads every wallet's public
`clearinghouseState` (about 25 s for 100 wallets) and sums long and short
position value per coin; a snapshot is dropped if more than a fifth of the
reads fail. `Positioning` events carry only the totals (holders, long and
short counts and value, long share, the change since the last snapshot,
average leverage). One snapshot from this run:

```
$ mft-engine positioning --coins BTC,ETH,SOL,HYPE,XRP      (2026-10-06 05:18 UTC)
coin           holders  longs shorts      long $M     short $M  long %   lev L   lev S  crowd
ETH                  9      7      2        349.7          4.1   98.8%    13.7    17.5  bullish
BTC                 10      6      4        278.5         29.8   90.3%    18.8    17.8  bullish
HYPE                 9      7      2        192.1         19.1   91.0%     8.6     6.5  bullish
SOL                  7      4      3         89.8         14.6   86.0%    12.5    16.7  bullish
XRP                  3      1      2          3.0          9.7   24.0%    20.0    15.0  bearish
```

Two bugs in the old Python, found while porting and fixed here:

* **Funding was understated eightfold.** The old scanner annualised funding as
  `rate * 3 * 365`, as if Hyperliquid paid every 8 hours. It pays hourly; the
  baseline 0.00125% per hour is 10.95% a year, not 1.4%. `universe` uses
  `rate * 24 * 365`.
* **The leaderboard's daily, weekly and monthly figures were always empty.**
  The old parser looked up windows named `daily`, `weekly` and `monthly`; the
  API names them `day`, `week` and `month`.

**There is no positioning history.** Hyperliquid returns current wallet
state only, so positioning cannot be backfilled over the 3.5-day bar window,
and v3 makes no trades there (its gate fails closed on every bar; ledger
entries 8 and 9 record exactly that). It can only be evaluated on data
recorded live, which is what the next part is.

### The recorded session: v1, v2 and v3 on the same live data

`mft-engine record` ran for 85 minutes, 2026-10-06 05:12 to 06:38 UTC, with
every live source on: Hyperliquid trades and book tops (16,991 and 1,896),
Kalshi ladders every minute (170), top-wallet positioning every 3 minutes (58
snapshots, BTC and ETH), and the Jev-scored Hacker News feed tailed from the
sidecar (6 signals from 3 posts, none about crypto). It is committed as
`data/live_session.jsonl` (2.3 MB), with the 90 one-minute REST bars before
it (`data/live_session_warmup.jsonl`) so momentum is warm at the start.

The crowd did not move. All 29 snapshots read the same: BTC held by 10 of
the 100 wallets, 90.3% long by value; ETH by 9, 98.8% long. So for this hour
and a half v3 meant "longs only".

Output of `mft-engine experiment --file experiments/recorded_session.toml`
(full window; the halves are not meaningful at this length):

| strategy | fills | PnL after costs | before costs | gate vetoes |
|---|---|---|---|---|
| v1 momentum | 12 | -$9.79 | -$3.19 | 0 |
| v2 Kalshi + social | 8 | -$6.86 | -$2.45 | 17 |
| v3 positioning | 4 | -$3.06 | -$0.86 | 18 |
| v2 + positioning | 4 | -$2.88 | -$0.68 | 35 |

v1's 12 fills include 2 at 05:13 that closed positions it had opened during
the warm-up history (the first live minute repeated the last REST minute,
which correctly reset the signal). In the session itself momentum fired in
one burst between 06:15 and 06:24 UTC: a short, then a long, then flat. v3
skipped the short (the crowd was long) and took only the long round trip,
which lost 86 cents before costs and $3.06 after. Every variant lost; the
gates lost less by trading less. **This is 85 minutes and one momentum
burst: it shows the positioning gate working end to end on live data, and
says nothing about whether it helps.**

## Strategy v4: funding carry across perps (pre-registered, sealed out-of-sample)

**What it does.** Hyperliquid perpetuals settle funding **every hour**: when
the rate is positive, longs pay shorts that share of position value for the
hour (the old Python scanner's 8-hour assumption understated this eightfold;
see above). v4 ranks a fixed universe of 30 liquid perps on a schedule by
their trailing funding, shorts the six whose longs pay the most, buys the six
whose longs pay the least (or are paid), and stays dollar-neutral. It accrues
funding hourly on every position and pays the taker fee and slippage on
every rebalance trade.

**Why it might work.** Persistently high funding marks a crowded, leveraged
long side that pays to stay in; whoever supplies the other side is paid for
it, and funding persists from hour to hour, so the trailing rate forecasts
the next payment. **Why it might not:** crowded longs are often in coins that
keep rising, so the short side can lose on price far more than it earns in
funding. Dollar neutrality removes the market's direction, not coin-specific
moves.

`src/carry.rs` is the strategy and backtest: a pure, deterministic function
of an hourly panel, fingerprinted like the other strategies. It does not go
through `Engine::on_event`, which handles one coin and one minute bar at a
time; v4 is a cross-sectional portfolio rebalanced on a schedule. Point in
time: a rebalance at hour `h` sees only closes stamped at or before `h` and
funding settled at or before `h`; funding settled at `h` pays positions held
over the hour before it, so a position opened at `h` first earns the payment
at `h + 1h`. The in-sample run is handed a panel physically cut at the split.

### The protocol (`src/carry_research.rs`, `mft-engine carry`)

1. `fetch-carry` downloaded 97 days (2026-07-01 to 10-06 UTC) of hourly
   candles (`candleSnapshot`) and settled hourly funding (`fundingHistory`)
   from the public info API, paced at 1.5 s per request.
2. `carry preregister` wrote the whole specification to the ledger (entry
   14) **before any v4 run on real data**, and it was committed on its own.
   The engine refuses an in-sample run without it, refuses data that differ
   from the pre-registered hashes, and refuses a second pre-registration.
3. `carry in-sample` runs the first 60% of the trading window. A config that
   differs from the pre-registered one is a design choice: it needs a note,
   is logged as such, and at most two are allowed.
4. `carry oos` runs the sealed last 40% once. It refuses a config that was
   not the last one run in-sample, and refuses a second run unless given
   `--force` and a `--note`, in which case the rerun is recorded as forced
   next to the first result. The kill rule is applied by code.

### The specification as pre-registered (ledger entry 14, `experiments/carry_v4.toml`)

| item | value |
|---|---|
| universe | every main-dex perp in the metadata at fetch time, listed or delisted (234), ranked by dollar volume (hourly volume x close) over the formation week 2026-07-01 to 07-08; the top 30 with at least 90% of formation hours, fixed for the whole window |
| the 30 | BTC, ETH, HYPE, SOL, ZEC, LIT, XRP, NEAR, WLD, PUMP, FARTCOIN, XPL, SUI, VVV, AAVE, kBONK, ADA, kPEPE, DOGE, ENA, BNB, TAO, LINK, MORPHO, JTO, BCH, DYDX, AVAX, UNI, XMR |
| signal | mean of the last 72 settled hourly funding rates (at least 90% present) |
| rebalance | every 24 h at 00:00 UTC (changed in-sample to every 168 h, see below) |
| buckets | short the top 6, long the bottom 6; ties broken by coin name |
| weights | equal inside each side |
| gross | $10,000: $5,000 long, $5,000 short; returns are on $10,000, as if the full notional were posted (1x) |
| costs | 4.5 bp taker fee + 3 bp slippage per fill (these alts are thinner than BTC and ETH, which use 1 bp elsewhere here); trades under $50 skipped unless closing; every window starts flat and closes everything at its end, with costs |
| split | trading window 2026-07-08 to 10-06 (90 days): in-sample 07-08 to 08-31 (54 days), sealed out-of-sample 08-31 to 10-06 (36 days) |
| kill rule | killed if sealed out-of-sample net Sharpe <= 0 or net P&L <= 0 after fees, slippage and funding |
| statistics | daily P&L (00:00 UTC marks), Sharpe annualised with sqrt(365); 95% circular moving-block bootstrap (5-day blocks, 10,000 resamples, fixed seed) on mean daily P&L |
| benchmark | buy and hold $10,000 of the BTC perp over the same window, paying funding and costs; and zero |

### Results, as they came out

| run | ledger | net P&L | funding | fees + slippage | price | Sharpe | annualised | max DD | turnover / day | mean / day (95% CI) |
|---|---|---|---|---|---|---|---|---|---|---|
| in-sample, pre-registered (daily) | 15 | -$820.98 | +$136.00 | -$209.51 | -$747.47 | -2.44 | -55.0% | $1,272 | 0.52x | -$15.06 (-$41.30 to +$9.40) |
| in-sample, design choice 1 (weekly) | 16 | -$475.42 | +$103.23 | -$73.48 | -$505.18 | -1.48 | -31.6% | $915 | 0.18x | -$8.67 (-$35.65 to +$17.72) |
| **sealed out-of-sample (weekly), run once** | **17** | **-$478.69** | **+$54.75** | **-$70.70** | **-$462.73** | **-1.91** | **-47.8%** | **$1,080** | **0.26x** | **-$13.09 (-$52.90 to +$22.65)** |
| hold BTC perp, in-sample | | +$2,103.74 | -$137.32 | -$16.69 | +$2,257.75 | 3.39 | 142.7% | $709 | | +$39.10 (-$19.64 to +$121.68) |
| hold BTC perp, out-of-sample | | +$931.98 | -$92.56 | -$15.78 | +$1,040.32 | 2.37 | 95.3% | $831 | | +$26.10 (-$25.60 to +$90.36) |

Zero, the other benchmark, beats v4 in every window. The sealed run made 112
trades over 7 rebalances, was up on 17 of 36 days, and 74.8% of bootstrap
means were at or below zero. Its fingerprint is `e762463bfde66cbe`; a test
replays every logged v4 run and requires the fingerprint and P&L on the
ledger. Both logged in-sample results and the sealed result were reproduced
to the cent by a separate re-implementation written from the specification.

**The one design choice.** At daily rebalancing, fees and slippage ($209.51)
were larger than the funding earned ($136.00): the ranking churned half the
book every day. Weekly rebalancing was run in-sample under a rule written
before the run (adopt only if in-sample net P&L improves), improved it, and
was adopted. For this the schedule was anchored at the window start (no
effect on the daily config: ledger 15 replays unchanged). The second allowed
design choice was not used: the remaining loss is the price leg, and tuning
against it would be fitting the in-sample path.

**What it says, plainly.** v4 is killed by its own pre-registered rule. The
carry is real and was collected (+$54.75 in the sealed window, about 5.5% a
year on gross), and weekly rebalancing kept costs close to the funding
earned. But the short side, coins with crowded longs, kept outperforming the
long side, and that price leg lost about eight times the funding. Over the
same 36 days, simply holding the BTC perp made $931.98. The bootstrap
interval for the sealed mean daily P&L includes zero, so the size of the loss
is not precisely estimated either: this is a clear failure to show an edge,
not proof of a reliably negative one.

Caveats: 90 days and one regime (BTC rose strongly through both windows);
paper fills at the hourly close plus 3 bp; no margin interest, liquidation or
funding caps modelled; prices from 1-hour candle closes, not the oracle price
Hyperliquid uses for funding. Residual survivorship: the candidate list is
Hyperliquid's metadata at fetch time; delisted perps stay in it, flagged, but
`candleSnapshot` returned no formation-week history for 55 of the 56 (most
were delisted long before the window), so a coin delisted during the
formation week itself could be missed. No universe coin was delisted inside
the window. Builder (HIP-3) dexes are excluded.

Data added: `data/carry_1h_bars.jsonl` (69,870 hourly bars, 11.3 MB),
`data/carry_funding_1h.jsonl` (69,870 funding settlements, 5.3 MB) and
`data/carry_universe.json` (the rule, the formation-week volume of all 234
candidates, the chosen 30): 16.6 MB in all.

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
| 7 | v1_momentum_baseline | baseline | -197.78 | -110.95 | 294 |
| 8 | v3_positioning_gated_backfill | killed | 0.00 | 0.00 | 0 |
| 9 | v2_plus_positioning_backfill | killed | 0.00 | 0.00 | 0 |
| 10 | session_v1_momentum | baseline | -9.79 | -7.41 | 12 |
| 11 | session_v2_reasoning_gated | killed | -6.86 | -6.86 | 8 |
| 12 | session_v3_positioning_gated | killed | -3.06 | -3.06 | 4 |
| 13 | session_v2_plus_positioning | killed | -2.88 | -2.88 | 4 |
| 14 | v4_funding_carry | preregistered | | | |
| 15 | v4_funding_carry | in_sample | -820.98 | | 397 trades |
| 16 | v4_funding_carry | design_choice | -475.42 | | 127 trades |
| 17 | v4_funding_carry | killed | | -478.69 | 112 trades |
| 18 | v1_momentum_with_polymarket_data | preregistered | | | |
| 19 | v2_reasoning_gated_with_polymarket_data | preregistered | | | |
| 20 | v2b_kalshi_and_polymarket | preregistered | | | |
| 21 | v1_momentum_with_polymarket_data | baseline | -197.78 | -110.95 | 294 |
| 22 | v2_reasoning_gated_with_polymarket_data | killed | -152.97 | -71.23 | 267 |
| 23 | v2b_kalshi_and_polymarket | killed | -82.76 | -40.13 | 134 |
| 24 | v5_hourly_trend | preregistered | | | |
| 25 | v5_hourly_trend | killed | 269.51 | 208.15 | 391 |

Entries 7 to 9 (`experiments/positioning.toml`) are v3 on the backfilled
window, where no positioning exists: zero trades, as predicted, recorded
anyway. Entries 10 to 13 (`experiments/recorded_session.toml`) are the live
session; each file's first variant is its own baseline.

Entries 14 to 17 are v4 (above), recorded by `mft-engine carry` on the same
chain, each carrying v4's kill rule as its verdict rule. For v4, "PnL" is the
in-sample result and "holdout" the sealed out-of-sample one.

Entries 18 to 23 are v2b (`experiments/polymarket_v2b.toml`): the
specification first (`experiment --preregister`, verdict "preregistered",
with the fully resolved config), then the one run. The file sets
`require_preregistration`, so `experiment` refuses to run it unless those
entries exist for its exact SHA-256.

Entries 24 and 25 are v5 (`experiments/hourly_trend_v5.toml`), the same way.
Its file sets `verdict = "positive_all_windows"`: one strategy judged on its
own (net PnL after costs positive on the full window and in both halves),
with no baseline to beat; the rule is copied into both entries.

A note on fingerprints: entries 0 to 6 logged decision fingerprints that no
committed build gives back (the builds of c77ede1, which added the ledger, of
4c53179 and of today all give `5ccac06aa73491d6` for entry 0, not the logged
`afb924fee4ed2223`), while every number in their reports reproduces exactly,
and entry 7, the same configuration rerun an hour later, logged the
fingerprint the code gives.
Why is not known; an uncommitted difference in the decisions' text at the
time would explain it. `ledger dsr` accepts such an entry only if every field
of its logged report matches the replay, and marks it.

### Deflated Sharpe ratio (`mft-engine ledger dsr`)

The best of many tried strategies has a positive Sharpe ratio by luck alone.
`ledger dsr --entry <seq>` (or `--all`) computes the deflated Sharpe ratio of
Bailey and López de Prado (2014): the probabilistic Sharpe ratio

`PSR(SR0) = Phi( (SR - SR0) sqrt(T - 1) / sqrt(1 - skew SR + (kurt - 1)/4 SR^2) )`

against `SR0 = sqrt(V) ((1 - g) PhiInv(1 - 1/N) + g PhiInv(1 - 1/(N e)))`, the
expected maximum Sharpe of N trials with no edge (g the Euler-Mascheroni
constant, V the variance of the trial Sharpe ratios). SR uses the standard
deviation with T - 1; skewness and kurtosis (not excess) are the sample
moments; with N = 1, SR0 = 0 and the DSR is the PSR against zero
(`src/dsr.rs`, tested against hand-computed values).

* **N** counts every ledger entry that evaluated a strategy, once per
  distinct (config, data, window, time key): entries 0 to 13 and the three v4
  runs 15 to 17, killed ones and the two that never traded included. Not
  counted: the pre-registration (14), which has no result, and entry 7, an
  exact rerun of entry 0. **N = 16.**
* **Returns** are replayed, not read: the ledger keeps summaries only, so
  each trial is rerun from what its entry recorded and must reproduce it
  (above). v1 to v3 use P&L per minute over the full window, v4 its daily P&L.
  Sharpe ratios are compared across trials annualised (sqrt(525,600) per
  minute, sqrt(365) per day), and SR0 is converted back to the entry's own
  period.

| # | variant | T | SR, annualised | skew | kurt | PSR vs 0 | DSR | DSR, same-period trials only |
|---|---|---|---|---|---|---|---|---|
| 0 | v1_momentum | 5,071 min | -98.7 | -2.36 | 73.1 | 0.0000 | 0.0000 | 0.0000 |
| 1 | v2_reasoning_gated | 5,071 min | -96.5 | -2.92 | 45.3 | 0.0000 | 0.0000 | 0.0000 |
| 5 | v2_kalshi_strict_0p6 | 5,071 min | -58.9 | -5.84 | 86.5 | 0.0000 | 0.0000 | 0.0000 |
| 6 | v1_entry_z_3 | 5,071 min | -49.6 | -2.00 | 266.8 | 0.0000 | 0.0000 | 0.0000 |
| 12 | session_v3_positioning_gated | 175 min | -104.9 | -7.51 | 60.2 | 0.0000 | 0.0000 | 0.0000 |
| 15 | v4 in-sample, daily | 54 days | -2.44 | 0.74 | 6.1 | 0.190 | 0.0000 | 0.152 |
| 16 | v4 in-sample, weekly | 54 days | -1.48 | 1.29 | 6.6 | 0.296 | 0.0000 | 0.247 |
| 17 | v4 sealed out-of-sample | 36 days | -1.91 | -0.66 | 3.2 | 0.271 | 0.0000 | 0.230 |

(All rows: `ledger dsr --all`. Entries 2 to 4, 7, 10, 11 and 13 look like
their neighbours; 8 and 9 never traded, so their Sharpe is undefined and they
enter V as 0.) With N = 16, V = 3,242 and SR0 = +102.5 annualised.

The table was computed before v2b. Entries 21 to 23 raise N to 19 (21 and 22
repeat the decisions of 0 and 1, but ran on a different input set, so they
count; that only makes the deflation stricter): V = 2,763, SR0 = +98.7, and
every DSR above is still 0.0000. v2b itself (entry 23): SR -76.8 annualised
over 5,071 minutes, PSR against zero 0.0000, DSR 0.0000.

What it says, plainly: **nothing on the ledger needed deflating to be
rejected.** Every entry has a negative Sharpe ratio, so the PSR against zero
is already below one half: about 0 for the 1-minute strategies, 0.19 to 0.30
for v4. The DSR, which can only be lower, is 0.0000 for all of them. The
headline SR0 is large because the 1-minute strategies' annualised Sharpe
ratios are large and spread out (annualising minute P&L by sqrt(525,600)
magnifies them); deflating v4 only by the three daily v4 trials gives SR0 =
+0.41 and DSRs of 0.15 to 0.25, still well below one half. The 1-minute
series are far from normal (kurtosis 23 to 267: mostly flat minutes, a few
fills), which the PSR's denominator accounts for. The 175-minute session
windows include the 90 warm-up minutes, in which v1 can already trade.

Every non-baseline entry was killed. Before v5 nothing had a positive
holdout; v5 (entry 25) has one (+$208.15) and a positive full window, and
was killed by its own rule because its first half lost. On the
backfilled window the two that lose least (strict Kalshi agreement, and a
higher momentum threshold) do so by trading a quarter to a third as often;
neither is positive before costs in its holdout.

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
live Jev output in `results/live_text_signals.jsonl`. That run predates the
replay verification below: its log has no warm-up, no arrival stamps on the
Kalshi and social lines, and no decision log, so a replay of it starts cold.
Replay it with `--time-key arrival`: by exchange time `backtest` rebuilds
bars from its trades on top of the bars it already holds, and feeds each bar
twice.

## Replay verification

The claim "the same code runs in replay and live" is now checked against the
live run itself, decision by decision.

* **The live clock is recorded.** The live engine used to read the wall clock
  as it processed each event, which no log kept. It now runs on each event's
  arrival time (the wall-clock read time of its websocket frame, poll or file
  line), and every line of the session log carries it as `arrival_ts`, raised
  where needed so it never decreases in the order the engine consumed events.
  The file order is then the arrival order, and the clock is an input.
* **The warm-up is recorded.** `paper` (and `record`) write the 90 REST bars
  per coin that warm the strategy at the head of the session log, as `Warmup`
  events stamped with their fetch time. They reach the strategy's window and
  nothing else, as before, through the same `Engine::on_event` call in both
  modes.
* **Live decisions are recorded.** `paper` writes
  `results/paper_decisions.jsonl`: every decision with the session-log index
  of the event behind it. `paper.json` gives their fingerprint.
* **`--time-key arrival|exchange`.** By arrival, a replay orders events by
  arrival (a stable sort, so a session log keeps its exact order), clocks by
  arrival, builds bars from trades the way the live bar builder does (unless
  the log already holds the live bars) and adds no bar-gap events (the live
  engine only sees the gaps its feed reports, and those are in the log). By
  exchange, nothing changed. `backtest` picks arrival when its files carry
  arrival stamps, exchange otherwise; an experiment file replays by exchange
  unless it says `time_key = "arrival"`, and new ledger entries record their
  key. Every entry already on the ledger carries none and is pinned to
  exchange; a test replays all of them (see the DSR below), and the existing
  replay-fingerprint tests pass unchanged.

```
mft-engine verify-replay --session results/paper_events.jsonl --decisions results/paper_decisions.jsonl --strategy v2
```

replays the log through the same engine and loop and prints either
`identical` with both fingerprints, or the first divergence: the decision
index, the session event behind it, and the live and replayed decisions
(exit code 1). The tests run a synthetic session through the live path
(`paper::run_session`: 61 warm-up bars, trades through the live bar builder,
two posts from the recorded fixture log, virtual arrival stamps, no model
call), write both logs, and require `verify-replay` to say identical; the
same log without its warm-up diverges at decision 0, and so does a replay by
exchange time (each decision's time is off by the 120 ms feed lag). A log
that lost one post the live engine saw is reported at decision 1, at the bar
where the live engine closed its long on that post and the replay did not.

**On the session already committed** (`results/paper_events.jsonl`, the
04:22 run, with an empty decision log since that run made no decisions):
`identical`, 0 decisions on each side, fingerprint `09612b07b5ecb5a5` (the
empty list). That match is vacuous. Without the warm-up the 60-bar window
never fills in 22 live bars, so the replay cannot decide at all; the live run,
warm, made no decisions because |z| never reached 2. It shows nothing about
state. A new four-minute v2 run on 2026-10-07 at 16:55 UTC (not committed)
wrote 180 warm-up bars and 1,097 live events with their arrival times;
`verify-replay` on it: identical, 0 decisions on each side, because momentum
again never reached |z| >= 2. That checks the recording and the warm replay
path end to end, not a decision. The decision-level evidence so far is the
synthetic session in the tests; a live session that trades is still to come.

## Running it

Built and tested with Rust 1.93 on Windows. `backtest`, `experiment`,
`export-demo` and `cargo test` run offline from the committed data.

```
cargo build --release
cargo test

./target/release/mft-engine backtest                 # v1, v2, v3 side by side -> results/backtest.json
./target/release/mft-engine experiment               # experiments/ideas.toml -> results/ledger.jsonl
./target/release/mft-engine experiment --verify      # check the hash chain
./target/release/mft-engine experiment --file experiments/polymarket_v2b.toml   # v2b; refuses unless pre-registered (it is: ledger 18-20)
./target/release/mft-engine ledger dsr --all         # deflated Sharpe of every ledger trial
./target/release/mft-engine ledger dsr --entry 17
./target/release/mft-engine verify-replay            # replay results/paper_events.jsonl, diff its decisions
./target/release/mft-engine export-demo              # -> docs/data/demo.json

# Strategy v4 (funding carry), offline from the committed data. The ledger
# already holds the pre-registration and the sealed run, so `oos` refuses.
./target/release/mft-engine carry in-sample          # a changed config needs --note
./target/release/mft-engine carry oos                # refuses: already run (ledger 17)
./target/release/mft-engine fetch-carry --end 2026-10-06T00:00:00Z   # refetch the data (~15 min)

# Live, public data, no keys needed except Jev's.
./target/release/mft-engine universe                 # every perp on every dex -> data/universe.json
./target/release/mft-engine positioning --coins BTC,ETH,SOL
./target/release/mft-engine record --duration-secs 120   # feed + Kalshi + positioning (+ --text-feed)
./target/release/mft-engine fetch-bars --days 4
./target/release/mft-engine fetch-kalshi             # backfill ladders over the bar window
./target/release/mft-engine fetch-polymarket         # Polymarket daily ladders over the bar window (cached raw in data/polymarket_raw/)
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
| `src/event.rs` | the one `Event` enum: Trade, BookTop, Bar, TextSignal, PredictionMarket, Positioning, Gap |
| `src/engine.rs` | the shared decision path: one event in, at most one decision out |
| `src/strategy.rs` | v1 momentum and its position state machine |
| `src/prediction.rs` | ladder cleaning, P(close > x), implied quantiles, ladders kept per venue, the v2 and v2b gates |
| `src/text.rs` | the social gate: a [0, 1] multiplier and the "never riskier" check |
| `src/risk.rs` | risk rules as trait objects; the fail-closed `RiskEngine` |
| `src/execution.rs` | paper fill model, cash-based PnL, round trips |
| `src/event_loop.rs` | the single loop both modes run, with latency measurement |
| `src/kalshi.rs` | Kalshi public markets and candlesticks; live ladder and backfill |
| `src/polymarket.rs` | Polymarket public daily ladders (Gamma events, CLOB price history), raw-response cache, point-in-time minute ladders |
| `src/positioning.rs` | leaderboard wallet set, public wallet reads, per-coin aggregates, the v3 gate |
| `src/universe.rs` | every Hyperliquid perp on every dex, with funding, OI and mark |
| `src/carry.rs` | v4: hourly panel, funding ranking, dollar-neutral rebalancing, hourly funding accrual, bootstrap |
| `src/carry_research.rs` | v4's protocol: preregister, in-sample with capped design choices, the one-shot sealed out-of-sample run |
| `src/pollers.rs` | the slow live sources (Kalshi, positioning, social file) for `record` and `paper` |
| `src/feed.rs`, `src/gap.rs`, `src/hyperliquid.rs`, `src/bars.rs` | live feed, reconnects, gaps, bars |
| `src/experiment.rs`, `src/ledger.rs` | TOML variants, hash-chained ledger |
| `src/artifacts.rs` | every file write; refuses anything secret-shaped |
| `src/demo.rs` | `export-demo` |
| `src/backtest.rs`, `src/paper.rs`, `src/record.rs`, `src/fetch.rs` | the subcommands (`fetch.rs` also fetches the v4 data); `backtest.rs` also loads a replay by exchange or arrival time |
| `src/verify.rs` | `verify-replay`: replay a live session log, diff against its decision log |
| `src/dsr.rs`, `src/trials.rs` | the deflated Sharpe ratio; the ledger's trials, replayed for their returns |
| `sidecar/` | HN and Reddit fetchers, Jev scorer, live social feed, secret guard |
| `docs/` | the static demo (index.html, app.js, style.css) |

## What is not done

* **No real orders.** No order endpoint, no signing, no exchange keys. Paper
  fills only. Anyone acting on a paper SIGNAL line does so by hand.
* **Venues:** Hyperliquid (BTC and ETH perpetuals) for prices; Kalshi hourly
  KXBTCD and KXETHD ladders and, for v2b only, Polymarket's daily BTC and ETH
  "above" ladders for prediction markets. Kalshi's KXBTC range series is not
  used: the threshold ladder already gives the distribution directly.
* **Polymarket is backfill only.** `fetch-polymarket` reads it after the fact
  (public Gamma and CLOB price-history endpoints); there is no live
  Polymarket poller, so `paper` and `record` do not offer v2b, and the
  recorded live session has no Polymarket data. Polymarket's hourly "up or
  down" and 5/15-minute markets exist but ask a different question (close
  above the period's open, not above a strike) and are not used. Its daily
  ladder has 11 strikes ($2,000 apart for BTC), so near a resolution it is
  often too coarse to read; the price history is Polymarket's own minute
  series, not the order book.
* **Positioning has no history** and only what was recorded live (one
  session here) can test v3. It reads the main dex only, not builder-dex
  positions. Choosing wallets by 30-day PnL favours whoever was on the right
  side of the last month, so "the crowd" may simply be last month's trend.
* **One universe-wide strategy, and it was killed.** v4 trades 30 main-dex
  perps; v1 to v3 trade BTC and ETH only. HIP-3 equities are listed by
  `universe` but not traded.
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
  Live, the clock is each event's recorded arrival time, so a live session
  replays by arrival to the live decisions (`verify-replay`).
* **Point in time everywhere.** Bars stamped at close, Kalshi snapshots at
  minute end, posts at publication plus poll delay plus scoring latency.
* **Research is recorded, not remembered.** The ledger keeps killed ideas next
  to kept ones and cannot be edited quietly.
* **Minimal dependencies:** tokio, tokio-tungstenite, futures-util, serde,
  serde_json, ureq, native-tls, anyhow, clap, sha2, toml.

## License

MIT. See `LICENSE`.
