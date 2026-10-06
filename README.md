# perp-engine

A small Rust engine for crypto perpetuals: live public market data in, one
mid-frequency signal, a risk layer that fails closed, paper execution, and a
backtester that runs the same code path, so research and production cannot
disagree.

**Paper only.** There is no order router, no signing code and no key handling
anywhere in the crate. A test (`there_is_no_order_path_in_the_source`) fails
the build if the source ever mentions Hyperliquid's order endpoint or signing.

**The honest result:** the pre-registered signal loses money after costs on
the committed sample, in both halves of it. That is reported below exactly as
the program printed it.

## The signal, stated before any backtest was run

Volatility-normalised short-horizon momentum on 1-minute bars, per coin (BTC
and ETH perpetuals on Hyperliquid):

| parameter | value |
|---|---|
| return horizon | 5 bars: `r5 = ln(close_t / close_{t-5})` |
| volatility | std of the last 60 one-minute log returns |
| score | `z = r5 / (sigma * sqrt(5))` |
| entry | long if `z >= 2.0`, short if `z <= -2.0` |
| exit | when `z` changes sign against the position, or after 15 bars |
| flip | directly, if `z` crosses the entry threshold the other way |
| size | fixed target of $1,000 notional per coin |
| minimum trade | target minus position under $50 is not traded |

Nothing was tuned. The parameters are the defaults in `src/strategy.rs` and
were fixed before the first run. The data is split at its midpoint in time and
both halves are reported, so a reader can see whether the result is stable.

## Architecture

```
   Hyperliquid websocket                     data/*.jsonl (bars, recorded feed,
   (trades, l2Book, public)                   scored text signals)
            |                                          |
        feed.rs  (reconnect, backoff,           backtest.rs (load, add bar
         gap detection)                          gaps, build bars, sort)
            |                                          |
        bars.rs  BarBuilder (trades -> 1m bars) -------+  (same builder)
            |                                          |
            v                                          v
       mpsc channel of Envelope { Event, received }   mpsc channel
            \                                         /
             \_______  event_loop::run  ____________/      <- one loop
                        |  clock.observe(event)
                        |  engine.on_event(event, clock)
                        v
   engine.rs:  market state -> strategy.rs (target)
                            -> text.rs (may only shrink the target)
                            -> risk.rs (every rule must allow; errors block)
                            -> execution.rs (paper fill, portfolio)
                        |
                        v
                     Decision  (Filled or Blocked with a reason)

   Clock: ReplayClock (event time) in backtest, WallClock in paper.
   Event: Trade | BookTop | Bar | TextSignal | Gap   (event.rs)

   sidecar/ (Python): posts -> Jev or keyword mock -> data/text_signals.jsonl
```

The strategy, text rule, risk rules, fill model and portfolio are identical in
both modes. Only the event source and the clock differ. `Engine::on_event`
does no I/O and never reads the system clock, which is what makes a replay
deterministic.

## Modules

| file | what it does |
|---|---|
| `src/event.rs` | the one `Event` enum and replay ordering |
| `src/clock.rs` | `Clock` trait, replay and wall clocks, a UTC formatter |
| `src/engine.rs` | the shared code path: one event in, at most one decision out |
| `src/strategy.rs` | the momentum signal and its position state machine |
| `src/text.rs` | text caution: a multiplier in [0, 1] and the check that it added no risk |
| `src/risk.rs` | risk rules as trait objects; the fail-closed `RiskEngine` |
| `src/execution.rs` | paper fill model, cash-based PnL, round-trip accounting |
| `src/event_loop.rs` | the single loop both modes run, with latency measurement |
| `src/feed.rs` | live websocket: subscribe, ping, reconnect with backoff, gaps |
| `src/gap.rs` | silence and backwards-time detection per stream |
| `src/bars.rs` | trades to 1m bars, missing-bar detection, JSONL I/O |
| `src/hyperliquid.rs` | wire formats for the public websocket and candle endpoint |
| `src/backtest.rs`, `src/paper.rs`, `src/record.rs`, `src/fetch.rs` | the subcommands |
| `sidecar/` | Python text scorer (Jev or offline mock) |

## Running it

Built and tested with Rust 1.93 on Windows. Everything below works offline except `record`, `paper`
and `fetch-bars`.

```
cargo build --release
cargo test

# Replay the committed bars (and Jev-scored text signals) through the engine.
./target/release/perp-engine backtest
./target/release/perp-engine backtest --no-text
./target/release/perp-engine backtest --text data/text_signals_mock.jsonl
./target/release/perp-engine backtest --data data/sample_recorded_feed.jsonl --no-text   # a recorded live feed

# Live, public data, no key needed.
./target/release/perp-engine record --duration-secs 120          # -> data/recorded_feed.jsonl
./target/release/perp-engine paper  --duration-secs 240          # -> results/paper.json
./target/release/perp-engine fetch-bars --days 4                 # -> data/bars_1m.jsonl
```

`backtest` writes `results/backtest.json` (config and every window) and
`results/backtest_decisions.jsonl` (every fill and every block, in order).

`paper` prints one human-readable line per decision, for example from the run
below:

```
SIGNAL 2026-10-06T03:33:02Z ETH-PERP SHORT target -1000 USD (order -0.37124 ETH) | reason: 5m momentum z=-3.17 | risk: PASSED, paper fill -0.37124 @ 2693.43 fee 0.4500
```

A blocked decision prints `risk: BLOCKED (<rule>: <reason>)`. Anyone acting
on a line does so by hand, outside this program.

## Costs and fill model

* Every order is a taker order and fills in full immediately.
* Reference price: the touch (ask for buys, bid for sells) when the book top
  is at most 12 s old, otherwise the last bar close. The backtest has bars
  only, so it always uses the bar close.
* Slippage: a further **1 bp** against us on top of the reference.
* Fee: **4.5 bps** of filled notional, Hyperliquid's base taker tier.
* A round trip therefore costs about 11 bps of notional, about $1.10 on $1,000.
* PnL is cash-based and includes open positions marked at the last close.

## Backtest result

Data: Hyperliquid 1-minute candles for BTC and ETH, 2026-10-02 14:47 UTC to
2026-10-06 03:17 UTC (10,139 bars, about 3.5 days, which is all the 1m history
the endpoint keeps). No missing bars. Output of `perp-engine backtest`:

```
window                  fills  trips       hit    pnl_net  pnl_gross     costs   turn_x   max_dd
full                      296    148      2.0%    -194.49     -33.23    161.25    293.2   194.71
full_without_text         294    148      2.0%    -197.78     -34.97    162.81    296.0   198.00
first_half                181     90      2.2%    -113.81     -15.76     98.06    178.3   113.81
second_half_holdout       172     86      5.8%    -107.00     -13.00     94.00    170.9   107.23
```

Columns: fills; completed round trips; share of round trips with positive PnL
after costs; PnL after costs (USD); PnL before fees and slippage; fees plus
slippage; traded notional divided by the $1,000 target; maximum drawdown of
the equity curve sampled on every bar.

What this says, plainly:

* **The signal loses money after costs, and loses before costs too.** Gross
  PnL is negative in both halves. At this horizon, 1-minute crypto momentum
  measured this way has no edge in this sample, and 11 bps per round trip
  turns a small negative into a large one: costs are about 80% of the loss.
* The hit rate after costs is 2 to 6%. Positions exit as soon as the
  5-minute move turns against them, so most round trips capture less than the
  11 bps they cost.
* The two halves agree. There is no sign of a good half hiding a bad one.
* **The daily loss limit ($50) tripped on every full UTC day** (Oct 3, 4 and
  5, each around midday), after which only reducing orders were allowed. The
  657 blocks are all `max_daily_loss`. So these numbers are the signal *with*
  the risk layer; without the limit the engine would have kept trading a
  signal whose gross edge here was negative.
* Text signals: 120 Jev-scored signals from 60 synthetic posts reduced or
  vetoed a target 95 times and changed the result by $3.29. The posts are
  invented, so this shows the mechanism works, not that text helps.
* Determinism: the decision fingerprint for the full run is
  `30a14fb49ff230bb` on every run, and the tests check that two replays give
  identical decisions and reports.

## Paper run (live feed, paper fills)

`perp-engine paper --duration-secs 240`, started 2026-10-06 03:31:54 UTC, BTC
and ETH. 1,285 trades, 92 book tops and 8 bars processed; 2 decisions, both
filled on paper (`results/paper.json`, `results/paper_session.log`).

| latency (from websocket frame read to decision) | samples | p50 | p99 |
|---|---|---|---|
| all events | 1,385 | 436 us | 2,061 us |
| bar events (the ones that can trade) | 8 | 263 us | 590 us |
| inside `Engine::on_event` only | 1,385 | 1.4 us | 24.9 us |

The engine itself takes about a microsecond. The rest of the end-to-end time
is two channel hops (feed task, bar-builder task, engine) and queueing: a
single websocket frame can carry dozens of trades, and the last one waits for
the others. Eight bar samples are too few for a meaningful p99; it is printed
because it was measured, not because it is stable.

Exchange-to-receive time (local receive clock minus exchange timestamp) was
360 ms at p50. Its p99 (12.7 s) is the snapshot of recent trades Hyperliquid
sends on subscribe, not network delay. That figure also includes any offset
between this machine's clock and the exchange's.

An earlier 4-minute run (03:27 UTC) gave all-event p50 639 us and p99 5.9 ms,
with one paper fill. Latency varies run to run; these are single runs on a
Windows laptop, not a benchmark.

## Recording

`perp-engine record --duration-secs 120` at 03:27 UTC wrote 406 trades and 46
book tops with no gaps. `data/sample_recorded_feed.jsonl` is that file; it
replays through `backtest`, which builds bars from its trades with the same
`BarBuilder` the paper engine uses.

The first recording found something worth knowing: the public `l2Book`
channel pushed a snapshot only every ~5.4 s (not sub-second), so the original
5 s silence threshold flagged 42 false gaps in two minutes. The threshold is
now 20 s, and a quiet book stream no longer resets the strategy (bars come
from trades; a stale book only stops being used as a fill reference).

## Text signal (sidecar)

`sidecar/score_posts.py` scores each post with TypeSafe's Jev (one
`system_one` call per post: two `Noul` relevance questions, two `Choice`
direction questions, one `Noul` novelty question) when `TYPESAFE_API_KEY` is
set, or with a deterministic keyword mock (`mock-keyword-v1`) otherwise. The
key is read by the SDK from the environment and is never printed or stored.

Measured on the 60 committed posts (`sidecar/scoring_stats_jev.json`):

| scorer | latency p50 | latency p99 | input tokens per post |
|---|---|---|---|
| Jev | 144 ms | 402 ms (the first call) | 506 |
| keyword mock | none (offline; a fixed 1 s delay is assumed) | | 0 |

Each `TextSignal` is stamped with publication time plus scoring latency, so a
backtest cannot act on a score before it existed. A plain-LLM comparison on
the same posts was not run.

The rule: text may make a decision more cautious, never less. The engine
scales the strategy's target by a `Caution` multiplier that can only hold a
value in [0, 1] (the field is private; the constructor clamps; NaN becomes 0).
Only the probability *opposing* the intended direction counts, weighted by
relevance and novelty: a bullish post never adds to a long; a bearish one
shrinks it, and vetoes it at 0.6. Afterwards `check_not_riskier` compares the
target before and after, and blocks the order if it grew or flipped. Text can
never open a position.

## What is not done

* **No real orders.** No exchange order endpoint, no signing, no keys. Paper
  fills only.
* **One venue.** Hyperliquid public data (BTC and ETH perpetuals). No second
  venue.
* **The posts are synthetic**, generated from templates with a fixed seed
  (`sidecar/make_posts.py`). Real news ingestion is not built.
* **Short history.** About 3.5 days of 1-minute bars, because that is all the
  candle endpoint keeps. Long recordings are possible with `record` but none
  is committed.
* Gap detection is heuristic: Hyperliquid's public feed has no sequence
  numbers, so we detect silences, backwards timestamps and reconnects, not
  individual missed messages.
* The fill model ignores queue position, partial fills, funding payments and
  market impact beyond the fixed 1 bp.
* No Jev-versus-plain-LLM latency and cost comparison yet.
* Money is `f64`. Fine for a paper engine; a production engine would use
  integer ticks.

## Design choices

* **One code path.** `Engine::on_event` is the only place decisions are made,
  and both modes call it through the same `event_loop::run`. A test replays
  the same data through the async loop and through a plain `for` loop and
  checks the decisions are identical.
* **Fail closed.** Each risk rule returns allow, block, or an error; an error
  blocks. A risk engine with no rules blocks everything. Non-finite numbers
  are errors. Stale data (no market data newer than 90 s by its *exchange*
  timestamp) blocks, so a lagging feed counts as stale even while messages
  arrive. After the daily loss limit, reducing orders are still allowed.
  Every block is returned as a `Decision::Blocked` with the rule and reason.
* **Deterministic replay.** Event time drives the clock; maps are `BTreeMap`
  so iteration order is fixed; events are sorted by time, coin and kind; the
  decisions of a run are fingerprinted with FNV-1a.
* **Bars stamped when known.** A bar's timestamp is its close (or the trade
  that closed it), never its open, so a replay cannot use a close before it
  happened.
* **Gaps are events.** A detected gap travels through the same stream as the
  data, so a replay of a recorded file reacts to it exactly as the live run
  did. A gap in trades, bars or the connection resets the strategy's price
  history; no return is computed across a hole.
* **Minimal dependencies:** tokio, tokio-tungstenite, futures-util, serde,
  serde_json, ureq, native-tls, anyhow, clap. Dates are formatted by hand.

## License

MIT. See `LICENSE`.
