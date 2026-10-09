//! `paper-hourly`: the live paper mode for an hourly strategy (v5).
//!
//! The same `Engine` as `backtest` and `paper`, fed one closed 1-hour candle
//! per coin per hour from Hyperliquid's public `candleSnapshot` endpoint and
//! clocked by each bar's arrival stamp (wall-clock time when it was fetched),
//! exactly as `paper` clocks its minute bars. Fills are simulated; nothing is
//! sent anywhere and no key exists.
//!
//! It is meant to run for weeks, so everything it consumes is appended to a
//! session log as it happens (`session.jsonl`, warm-up first, each line with
//! the clock it was handled at), and every decision to `decisions.jsonl`. On
//! a restart the session log is replayed through a fresh engine on those
//! same clocks, which rebuilds the exact state (positions, cash, the signal
//! window, the day's loss), and the run carries on from the last bar seen,
//! fetching any hours it missed. Catch-up bars arrive late, so the stale-data
//! rule blocks any order they would cause: the risk layer fails closed.
//!
//! Each hour it also rewrites `status.json` (what it holds, its equity, when
//! it last saw a bar) and `daily_summary.csv` (one row per UTC day), small
//! files meant to be read, and committed, by a person.

use crate::artifacts::{append_line, write_json, write_text};
use crate::bars::{read_recorded, HOUR_MS};
use crate::clock::{format_utc, wall_now_ms, Clock, ReplayClock, TimeKey};
use crate::engine::{Decision, Engine, EngineConfig};
use crate::event::{Bar, Event, Recorded, WarmupBar};
use crate::hyperliquid::fetch_candles;
use crate::paper::{signal_line, DecisionRecord};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

const DAY_MS: i64 = 86_400_000;

pub struct ForwardOptions {
    pub coins: Vec<String>,
    pub config: EngineConfig,
    pub strategy: String,
    pub dir: PathBuf,
    /// How long after each hour boundary to fetch the closed candle.
    pub poll_after_close: Duration,
    /// Stop after this many polls (tests and smoke runs); `None` runs forever.
    pub max_polls: Option<u64>,
}

/// Paths inside the run directory.
pub struct RunFiles {
    pub session: PathBuf,
    pub decisions: PathBuf,
    pub signals: PathBuf,
    pub status: PathBuf,
    pub daily: PathBuf,
}

impl RunFiles {
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            session: dir.join("session.jsonl"),
            decisions: dir.join("decisions.jsonl"),
            signals: dir.join("signals.log"),
            status: dir.join("status.json"),
            daily: dir.join("daily_summary.csv"),
        }
    }
}

/// One UTC day of the run, for the summary file.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DayRow {
    pub bars: u64,
    pub fills: u64,
    pub blocked: u64,
    /// Equity (USD, after costs, positions marked at the last close) at the
    /// day's last bar.
    pub equity: f64,
    pub last_bar: i64,
}

/// The live state: the engine, its clock, and what the files need.
pub struct ForwardRun {
    pub engine: Engine,
    clock: ReplayClock,
    pub last_open: BTreeMap<String, i64>,
    pub days: BTreeMap<i64, DayRow>,
    pub events: u64,
    pub decisions: u64,
    pub started: Option<i64>,
}

impl ForwardRun {
    pub fn new(config: EngineConfig) -> Self {
        Self {
            engine: Engine::new(config),
            clock: ReplayClock::keyed(0, TimeKey::Arrival),
            last_open: BTreeMap::new(),
            days: BTreeMap::new(),
            events: 0,
            decisions: 0,
            started: None,
        }
    }

    /// Consume one event at `arrival` (made non-decreasing, as `paper`
    /// does). Returns the engine's decision, if any, and the arrival used.
    pub fn consume(&mut self, event: &Event, arrival: i64) -> (Option<Decision>, i64) {
        let at = arrival.max(self.clock.now_ms());
        self.clock.observe_at(event, Some(at));
        let decision = self.engine.on_event(event, &self.clock);
        self.events += 1;
        self.started.get_or_insert(at);
        let bar = match event {
            Event::Bar(b) => Some(b),
            Event::Warmup(w) => Some(&w.bar),
            _ => None,
        };
        if let Some(b) = bar {
            let prev = self.last_open.entry(b.coin.clone()).or_insert(b.open_ts);
            *prev = (*prev).max(b.open_ts);
        }
        if let Event::Bar(b) = event {
            let day = self.days.entry((b.ts - 1).div_euclid(DAY_MS)).or_default();
            day.bars += 1;
            match &decision {
                Some(Decision::Filled { .. }) => day.fills += 1,
                Some(Decision::Blocked { .. }) => day.blocked += 1,
                None => {}
            }
            day.equity = self.engine.equity();
            day.last_bar = b.ts;
        }
        if decision.is_some() {
            self.decisions += 1;
        }
        (decision, self.clock.now_ms())
    }

    /// The summary file: one CSV row per UTC day.
    pub fn daily_csv(&self) -> String {
        let mut out = String::from("date_utc,bars,fills,blocked,equity_usd,day_pnl_usd,last_bar_utc\n");
        let mut prev = 0.0;
        for (day, row) in &self.days {
            out += &format!(
                "{},{},{},{},{:.2},{:.2},{}\n",
                &format_utc(day * DAY_MS)[..10],
                row.bars,
                row.fills,
                row.blocked,
                row.equity,
                row.equity - prev,
                format_utc(row.last_bar)
            );
            prev = row.equity;
        }
        out
    }
}

/// Rebuild a run from its session log, replaying every line on the clock it
/// was handled at. No decision is re-logged: the decision log already holds
/// them.
pub fn restore(config: EngineConfig, session: &Path) -> Result<ForwardRun> {
    let mut run = ForwardRun::new(config);
    if session.exists() {
        for r in read_recorded(session)? {
            let at = r.arrival();
            let _ = run.consume(&r.event, at);
        }
    }
    Ok(run)
}

/// The closed hourly bars for `coin` that open after `after_open`, oldest
/// first. `now` decides what is closed: a bar is closed once its close
/// stamp is at or before now.
pub fn closed_bars_after(bars: Vec<Bar>, after_open: Option<i64>, now: i64) -> Vec<Bar> {
    let mut out: Vec<Bar> = bars
        .into_iter()
        .filter(|b| b.ts <= now && after_open.map_or(true, |a| b.open_ts > a))
        .collect();
    out.sort_by_key(|b| b.open_ts);
    out.dedup_by_key(|b| b.open_ts);
    out
}

fn fetch_hourly(coin: &str, start: i64, end: i64) -> Result<Vec<Bar>> {
    let mut wait = Duration::from_secs(2);
    for attempt in 1..=5 {
        match fetch_candles(coin, "1h", start, end) {
            Ok(b) => return Ok(b),
            Err(e) if attempt < 5 => {
                eprintln!("[paper-hourly] {coin} candles, attempt {attempt} failed ({e:#}); retrying in {wait:?}");
                std::thread::sleep(wait);
                wait *= 2;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

#[derive(Serialize)]
struct Status<'a> {
    strategy: &'a str,
    mode: &'static str,
    pid: u32,
    started: String,
    updated: String,
    last_bar: BTreeMap<String, String>,
    positions_usd: BTreeMap<String, f64>,
    equity_usd: f64,
    fills: u64,
    fees_usd: f64,
    slippage_usd: f64,
    round_trips: usize,
    events: u64,
    decisions: u64,
    next_poll: String,
    stop: &'static str,
}

fn write_status(run: &ForwardRun, strategy: &str, files: &RunFiles, next_poll: i64) -> Result<()> {
    let marks = run.engine.marks();
    let positions = marks
        .iter()
        .map(|(c, px)| (c.clone(), (run.engine.portfolio.position(c) * px * 100.0).round() / 100.0))
        .collect();
    let p = &run.engine.portfolio;
    let status = Status {
        strategy,
        mode: "paper (simulated fills; no orders, no keys)",
        pid: std::process::id(),
        started: run.started.map(format_utc).unwrap_or_default(),
        updated: format_utc(wall_now_ms()),
        last_bar: run.last_open.iter().map(|(c, o)| (c.clone(), format_utc(o + HOUR_MS))).collect(),
        positions_usd: positions,
        equity_usd: (run.engine.equity() * 100.0).round() / 100.0,
        fills: p.fills,
        fees_usd: p.fees_paid,
        slippage_usd: p.slippage_paid,
        round_trips: p.round_trips.len(),
        events: run.events,
        decisions: run.decisions,
        next_poll: format_utc(next_poll),
        stop: "Stop-ScheduledTask -TaskName mft-engine-v5-forward (see README, v5)",
    };
    write_json(&files.status, &status)?;
    write_text(&files.daily, &run.daily_csv())
}

/// Log one consumed event and its decision to the run's files.
fn log_event(files: &RunFiles, event: &Event, at: i64, decision: Option<&Decision>, event_index: u64) -> Result<()> {
    let rec = Recorded { event: event.clone(), arrival_ts: Some(at) };
    append_line(&files.session, &serde_json::to_string(&rec)?)?;
    if let Some(d) = decision {
        let dr = DecisionRecord { event_index: event_index as usize, decision: d.clone() };
        append_line(&files.decisions, &serde_json::to_string(&dr)?)?;
        let line = signal_line(d);
        println!("{line}");
        append_line(&files.signals, &line)?;
    }
    Ok(())
}

/// Warm-up history: enough closed hourly bars to fill the signal window.
fn warm_up(run: &mut ForwardRun, coins: &[String], files: &RunFiles) -> Result<()> {
    let need = run.engine.config().strategy.vol_window_bars as i64 + 1;
    let now = wall_now_ms();
    for coin in coins {
        let bars = fetch_hourly(coin, now - (need + 24) * HOUR_MS, now)?;
        let bars = closed_bars_after(bars, None, now);
        let recv_ts = wall_now_ms();
        println!("[warm-up] {} closed hourly bars for {coin}", bars.len());
        for bar in bars {
            let event = Event::Warmup(WarmupBar { recv_ts, bar });
            let (_, at) = run.consume(&event, recv_ts);
            log_event(files, &event, at, None, run.events - 1)?;
        }
    }
    Ok(())
}

/// Run until stopped (or `max_polls`).
pub fn run(opts: ForwardOptions) -> Result<()> {
    let ForwardOptions { coins, config, strategy, dir, poll_after_close, max_polls } = opts;
    if config.strategy.bar_ms != HOUR_MS {
        bail!("paper-hourly needs an hourly strategy (v5); {strategy} uses {} ms bars", config.strategy.bar_ms);
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let files = RunFiles::in_dir(&dir);
    let mut run = restore(config, &files.session)?;
    if run.events == 0 {
        warm_up(&mut run, &coins, &files)?;
    } else {
        println!("[paper-hourly] restored {} events and {} decisions from {}", run.events, run.decisions, files.session.display());
    }
    println!("[paper-hourly] {strategy} on {coins:?}, hourly bars; paper fills only, no orders are sent anywhere");
    let mut polls = 0u64;
    loop {
        let now = wall_now_ms();
        for coin in &coins {
            let after = run.last_open.get(coin).copied();
            let start = after.map_or(now - 3 * HOUR_MS, |a| a + HOUR_MS);
            let bars = match fetch_hourly(coin, start, now) {
                Ok(b) => closed_bars_after(b, after, now),
                Err(e) => {
                    eprintln!("[paper-hourly] {coin}: no candles this hour ({e:#}); trying again next hour");
                    continue;
                }
            };
            for bar in bars {
                let event = Event::Bar(bar);
                let (decision, at) = run.consume(&event, wall_now_ms());
                log_event(&files, &event, at, decision.as_ref(), run.events - 1)?;
            }
        }
        let next = (wall_now_ms().div_euclid(HOUR_MS) + 1) * HOUR_MS + poll_after_close.as_millis() as i64;
        write_status(&run, &strategy, &files, next)?;
        polls += 1;
        if max_polls.is_some_and(|m| polls >= m) {
            return Ok(());
        }
        let wait = (next - wall_now_ms()).max(1_000) as u64;
        std::thread::sleep(Duration::from_millis(wait));
    }
}
