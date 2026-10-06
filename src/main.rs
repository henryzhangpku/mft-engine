//! Command-line entry point. Parses arguments and hands off to one module per
//! subcommand; no trading logic lives here.

use anyhow::Result;
use clap::{Parser, Subcommand};
use mft_engine::artifacts::{write_json, write_jsonl};
use mft_engine::backtest::{evaluate, load_events, print_table};
use mft_engine::engine::EngineConfig;
use mft_engine::{demo, experiment, fetch, paper, positioning, record, universe};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "mft-engine",
    version,
    about = "Paper-only mid-frequency engine: reasoning signals from social and prediction-market data, gated by code"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// The default replay inputs: bars, Kalshi ladders, Jev-scored HN posts.
const DEFAULT_DATA: [&str; 3] = ["data/bars_1m.jsonl", "data/kalshi_ladders.jsonl", "data/text_signals.jsonl"];

#[derive(Subcommand)]
enum Command {
    /// Record the live Hyperliquid feed (trades + top of book) to JSONL.
    Record {
        #[arg(long, value_delimiter = ',', default_value = "BTC,ETH")]
        coins: Vec<String>,
        #[arg(long, default_value_t = 120)]
        duration_secs: u64,
        #[arg(long, default_value = "data/recorded_feed.jsonl")]
        out: PathBuf,
        /// Also poll Kalshi ladders (0 = off).
        #[arg(long, default_value_t = 60)]
        kalshi_every_secs: u64,
        /// Also snapshot top-wallet positioning (0 = off).
        #[arg(long, default_value_t = 300)]
        positioning_every_secs: u64,
        #[arg(long, default_value_t = 100)]
        positioning_wallets: usize,
        /// Also record the sidecar's live TextSignal file.
        #[arg(long)]
        text_feed: Option<PathBuf>,
    },
    /// Every Hyperliquid perp across all dexes (main + HIP-3 builder dexes).
    Universe {
        #[arg(long, default_value = "data/universe.json")]
        out: PathBuf,
        /// Rows to print, by 24h volume.
        #[arg(long, default_value_t = 40)]
        top: usize,
    },
    /// One snapshot of top leaderboard wallets' positioning, per coin.
    Positioning {
        /// Coins to show (empty = the 25 largest by gross value).
        #[arg(long, value_delimiter = ',')]
        coins: Vec<String>,
        #[arg(long, default_value_t = 100)]
        wallets: usize,
        #[arg(long, default_value_t = 100_000.0)]
        min_account_value: f64,
        #[arg(long, default_value = "results/positioning_snapshot.jsonl")]
        out: PathBuf,
    },
    /// Download historical 1-minute candles to JSONL.
    FetchBars {
        #[arg(long, value_delimiter = ',', default_value = "BTC,ETH")]
        coins: Vec<String>,
        /// How many days back from now. Hyperliquid keeps about 3.5 days of 1m bars.
        #[arg(long, default_value_t = 4.0)]
        days: f64,
        /// Explicit window start, RFC 3339 UTC (with --end), instead of --days.
        #[arg(long)]
        start: Option<String>,
        #[arg(long)]
        end: Option<String>,
        #[arg(long, default_value = "data/bars_1m.jsonl")]
        out: PathBuf,
    },
    /// Backfill Kalshi hourly strike ladders over the bar file's window.
    FetchKalshi {
        #[arg(long, value_delimiter = ',', default_value = "BTC,ETH")]
        coins: Vec<String>,
        #[arg(long, default_value = "data/bars_1m.jsonl")]
        bars: PathBuf,
        /// Strikes to download on each side of spot at each event's open.
        #[arg(long, default_value_t = 10)]
        strikes_each_side: usize,
        #[arg(long, default_value = "data/kalshi_ladders.jsonl")]
        out: PathBuf,
    },
    /// Download the v4 data: hourly candles and funding for a liquid universe.
    FetchCarry {
        /// Window length in days, ending at the last UTC midnight (or --end).
        #[arg(long, default_value_t = 97)]
        days: i64,
        /// Leading days used only to choose the universe and warm the signal.
        #[arg(long, default_value_t = 7)]
        formation_days: i64,
        #[arg(long, default_value_t = 30)]
        top: usize,
        #[arg(long, default_value = "BTC")]
        benchmark: String,
        /// Window end, RFC 3339 UTC, on a midnight.
        #[arg(long)]
        end: Option<String>,
        #[arg(long, default_value = "data/carry_1h_bars.jsonl")]
        bars_out: PathBuf,
        #[arg(long, default_value = "data/carry_funding_1h.jsonl")]
        funding_out: PathBuf,
        #[arg(long, default_value = "data/carry_universe.json")]
        universe_out: PathBuf,
    },
    /// Strategy v4 (funding carry): preregister, in-sample, or the sealed out-of-sample run.
    Carry {
        #[arg(value_enum)]
        phase: mft_engine::carry_research::Phase,
        #[arg(long, default_value = "experiments/carry_v4.toml")]
        file: PathBuf,
        #[arg(long, default_value = "results/ledger.jsonl")]
        ledger: PathBuf,
        /// Why: required for a design choice and for a forced rerun.
        #[arg(long)]
        note: Option<String>,
        /// Rerun the sealed out-of-sample window anyway (recorded as forced).
        #[arg(long)]
        force: bool,
    },
    /// Replay event files through strategies v1 and v2, side by side.
    Backtest {
        #[arg(long, default_values = DEFAULT_DATA)]
        data: Vec<PathBuf>,
        #[arg(long, default_value = "results/backtest.json")]
        out: PathBuf,
    },
    /// Run the variants in a TOML file and append each to the hash-chained ledger.
    Experiment {
        #[arg(long, default_value = "experiments/ideas.toml")]
        file: PathBuf,
        #[arg(long, default_value = "results/ledger.jsonl")]
        ledger: PathBuf,
        /// Only verify the ledger's hash chain; run nothing.
        #[arg(long)]
        verify: bool,
    },
    /// Run live on Hyperliquid + Kalshi (+ the sidecar's social feed) with paper fills.
    Paper {
        #[arg(long, value_delimiter = ',', default_value = "BTC,ETH")]
        coins: Vec<String>,
        #[arg(long, default_value_t = 180)]
        duration_secs: u64,
        /// v1 (momentum only), v2 (Kalshi + social gates) or v3 (positioning gate).
        #[arg(long, default_value = "v2")]
        strategy: String,
        /// TextSignal JSONL written live by sidecar/live_social.py.
        #[arg(long)]
        text_feed: Option<PathBuf>,
        #[arg(long, default_value_t = 60)]
        kalshi_every_secs: u64,
        /// Top-wallet positioning snapshots (0 = off).
        #[arg(long, default_value_t = 300)]
        positioning_every_secs: u64,
        #[arg(long, default_value = "results/paper.json")]
        out: PathBuf,
        #[arg(long, default_value = "results/paper_events.jsonl")]
        events_out: PathBuf,
    },
    /// Write docs/data/demo.json for the static web demo.
    ExportDemo {
        #[arg(long, default_values = DEFAULT_DATA)]
        data: Vec<PathBuf>,
        #[arg(long, default_value = "data/hn_posts.jsonl")]
        posts: PathBuf,
        #[arg(long, default_value = "results/ledger.jsonl")]
        ledger: PathBuf,
        #[arg(long, default_value = "results/paper.json")]
        paper: PathBuf,
        #[arg(long, default_value = "sidecar/scoring_stats_jev.json")]
        jev_stats: PathBuf,
        #[arg(long, default_value = "data/live_session.jsonl")]
        session: PathBuf,
        #[arg(long, default_value = "data/live_session_warmup.jsonl")]
        session_warmup: PathBuf,
        #[arg(long, default_value = "data/universe.json")]
        universe: PathBuf,
        #[arg(long, default_value = "experiments/carry_v4.toml")]
        carry_spec: PathBuf,
        #[arg(long, default_value = "docs/data/demo.json")]
        out: PathBuf,
    },
}

fn strategy_config(name: &str) -> Result<EngineConfig> {
    match name {
        "v1" => Ok(EngineConfig::v1()),
        "v2" => Ok(EngineConfig::v2()),
        "v3" => Ok(EngineConfig::v3()),
        other => anyhow::bail!("unknown strategy {other:?}; use v1, v2 or v3"),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Record { coins, duration_secs, out, kalshi_every_secs, positioning_every_secs, positioning_wallets, text_feed } => {
            let extras = record::Extras {
                kalshi_every: Duration::from_secs(kalshi_every_secs),
                positioning_every: Duration::from_secs(positioning_every_secs),
                positioning_wallets,
                text_feed,
            };
            record::run(coins, &out, Duration::from_secs(duration_secs), extras).await
        }
        Command::Universe { out, top } => tokio::task::spawn_blocking(move || universe::run(&out, top)).await?,
        Command::Positioning { coins, wallets, min_account_value, out } => {
            tokio::task::spawn_blocking(move || positioning::run(&coins, wallets, min_account_value, &out)).await?
        }
        Command::FetchBars { coins, days, start, end, out } => {
            let window = match (start, end) {
                (Some(s), Some(e)) => {
                    let parse = |t: &str| mft_engine::clock::parse_utc(t).ok_or_else(|| anyhow::anyhow!("bad UTC time {t:?}"));
                    Some((parse(&s)?, parse(&e)?))
                }
                (None, None) => None,
                _ => anyhow::bail!("--start and --end go together"),
            };
            // Blocking HTTP; run it off the async workers.
            tokio::task::spawn_blocking(move || fetch::run(&coins, days, window, &out)).await?
        }
        Command::FetchKalshi { coins, bars, strikes_each_side, out } => {
            tokio::task::spawn_blocking(move || fetch::run_kalshi(&coins, &bars, strikes_each_side, &out)).await?
        }
        Command::FetchCarry { days, formation_days, top, benchmark, end, bars_out, funding_out, universe_out } => {
            let end = end
                .map(|t| mft_engine::clock::parse_utc(&t).ok_or_else(|| anyhow::anyhow!("bad UTC time {t:?}")))
                .transpose()?;
            let opts = fetch::CarryFetch { days, formation_days, top, benchmark, end, bars_out, funding_out, universe_out };
            tokio::task::spawn_blocking(move || fetch::run_carry(&opts)).await?
        }
        Command::Carry { phase, file, ledger, note, force } => {
            let opts = mft_engine::carry_research::Options { spec: &file, ledger: &ledger, note, force, results_dir: std::path::Path::new("results") };
            mft_engine::carry_research::run(phase, opts).map(|_| ())
        }
        Command::Backtest { data, out } => backtest(data, out).await,
        Command::Experiment { file, ledger, verify } => {
            if verify {
                let n = mft_engine::ledger::verify(&mft_engine::ledger::read(&ledger)?)?;
                println!("ledger {}: {n} entries, chain verified", ledger.display());
                return Ok(());
            }
            experiment::run(&file, &ledger).await.map(|_| ())
        }
        Command::Paper { coins, duration_secs, strategy, text_feed, kalshi_every_secs, positioning_every_secs, out, events_out } => {
            let report = paper::run(paper::PaperOptions {
                coins,
                duration: Duration::from_secs(duration_secs),
                config: strategy_config(&strategy)?,
                out: out.clone(),
                events_out: events_out.clone(),
                text_feed,
                kalshi_every: Duration::from_secs(kalshi_every_secs),
                positioning_every: Duration::from_secs(positioning_every_secs),
            })
            .await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            println!("wrote {} and {}", out.display(), events_out.display());
            Ok(())
        }
        Command::ExportDemo { data, posts, ledger, paper, jev_stats, session, session_warmup, universe, carry_spec, out } => {
            demo::run(demo::DemoInputs { data, posts, ledger, paper, jev_stats, session, session_warmup, universe, carry_spec, out }).await
        }
    }
}

/// v1 and v2 over the full sample and both halves, then the audit trails.
async fn backtest(data: Vec<PathBuf>, out: PathBuf) -> Result<()> {
    let events = load_events(&data)?;
    let mut evals = Vec::new();
    let strategies = [
        ("v1_momentum", EngineConfig::v1()),
        ("v2_reasoning_gated", EngineConfig::v2()),
        ("v3_positioning_gated", EngineConfig::v3()),
    ];
    for (name, config) in strategies {
        let (eval, run) = evaluate(name, &events, config).await;
        let trail = out.with_file_name(format!("backtest_decisions_{}.jsonl", &name[..2]));
        write_jsonl(&trail, &run.decisions)?;
        evals.push(eval);
    }
    print_table(&evals);
    for e in &evals {
        let r = &e.full;
        println!(
            "{} full: {} to {}, {} bars, {} kalshi snapshots, {} positioning snapshots, {} text signals, {} gaps, blocked {:?}, kalshi vetoes {}, positioning vetoes {}, text reductions {}, fingerprint {}",
            e.name, r.start, r.end, r.bars, r.prediction_snapshots, r.positioning_snapshots, r.text_signals, r.gaps, r.blocked_orders,
            r.pm_vetoes, r.crowd_vetoes, r.text_reductions, r.decisions_fingerprint
        );
    }
    let doc = serde_json::json!({
        "data": data,
        "config": { "v1": EngineConfig::v1(), "v2": EngineConfig::v2(), "v3": EngineConfig::v3() },
        "evaluations": evals,
    });
    write_json(&out, &doc)?;
    println!("wrote {} and the decision trails next to it", out.display());
    Ok(())
}
