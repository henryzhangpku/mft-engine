//! Command-line entry point. Parses arguments and hands off to one module per
//! subcommand; no trading logic lives here.

use anyhow::Result;
use clap::{Parser, Subcommand};
use perp_engine::backtest::{load_events, run_backtest, split_halves, BacktestReport};
use perp_engine::engine::EngineConfig;
use perp_engine::event::Event;
use perp_engine::{fetch, paper, record};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "perp-engine", version, about = "Paper-only mid-frequency engine for crypto perpetuals")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

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
    },
    /// Download historical 1-minute candles to JSONL.
    FetchBars {
        #[arg(long, value_delimiter = ',', default_value = "BTC,ETH")]
        coins: Vec<String>,
        /// How many days back from now. Hyperliquid keeps about 3.5 days of 1m bars.
        #[arg(long, default_value_t = 4.0)]
        days: f64,
        #[arg(long, default_value = "data/bars_1m.jsonl")]
        out: PathBuf,
    },
    /// Replay event files through the engine and report PnL after costs.
    Backtest {
        /// Event files to merge (bars, recorded feed, text signals).
        #[arg(long, default_values = ["data/bars_1m.jsonl"])]
        data: Vec<PathBuf>,
        /// Scored text signals from the sidecar.
        #[arg(long, default_value = "data/text_signals.jsonl")]
        text: PathBuf,
        /// Ignore the text signal file entirely.
        #[arg(long)]
        no_text: bool,
        #[arg(long, default_value = "results/backtest.json")]
        out: PathBuf,
    },
    /// Run the engine live on the websocket feed with paper fills.
    Paper {
        #[arg(long, value_delimiter = ',', default_value = "BTC,ETH")]
        coins: Vec<String>,
        #[arg(long, default_value_t = 180)]
        duration_secs: u64,
        #[arg(long, default_value = "results/paper.json")]
        out: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Record { coins, duration_secs, out } => {
            record::run(coins, &out, Duration::from_secs(duration_secs)).await
        }
        Command::FetchBars { coins, days, out } => {
            // Blocking HTTP; run it off the async workers.
            tokio::task::spawn_blocking(move || fetch::run(&coins, days, &out)).await?
        }
        Command::Backtest { data, text, no_text, out } => backtest(data, (!no_text).then_some(text), out).await,
        Command::Paper { coins, duration_secs, out } => {
            let report = paper::run(coins, Duration::from_secs(duration_secs), EngineConfig::default(), &out).await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            println!("wrote {}", out.display());
            Ok(())
        }
    }
}

async fn backtest(data: Vec<PathBuf>, text: Option<PathBuf>, out: PathBuf) -> Result<()> {
    let config = EngineConfig::default();
    let mut with_text = data.clone();
    if let Some(text) = text {
        with_text.push(text);
    }
    let events = load_events(&with_text)?;
    let no_text: Vec<Event> = events.iter().filter(|e| !matches!(e, Event::TextSignal(_))).cloned().collect();
    let (early, late) = split_halves(&events);

    let mut reports: Vec<BacktestReport> = Vec::new();
    let (full, decisions) = run_backtest("full", events, config).await;
    reports.push(full);
    reports.push(run_backtest("full_without_text", no_text, config).await.0);
    reports.push(run_backtest("first_half", early, config).await.0);
    reports.push(run_backtest("second_half_holdout", late, config).await.0);

    println!(
        "{:<22} {:>6} {:>6} {:>9} {:>10} {:>10} {:>9} {:>8} {:>8}",
        "window", "fills", "trips", "hit", "pnl_net", "pnl_gross", "costs", "turn_x", "max_dd"
    );
    for r in &reports {
        println!(
            "{:<22} {:>6} {:>6} {:>9} {:>10.2} {:>10.2} {:>9.2} {:>8.1} {:>8.2}",
            r.window,
            r.fills,
            r.round_trips,
            r.hit_rate.map_or("n/a".into(), |h| format!("{:.1}%", h * 100.0)),
            r.pnl_after_costs,
            r.pnl_before_costs,
            r.fees + r.slippage,
            r.turnover_multiple,
            r.max_drawdown
        );
    }
    for r in &reports {
        println!(
            "{}: {} to {}, {} bars, {} text signals, {} gaps, blocked {:?}, text reductions {}, fingerprint {}",
            r.window, r.start, r.end, r.bars, r.text_signals, r.gaps, r.blocked_orders, r.text_reductions, r.decisions_fingerprint
        );
    }

    let doc = serde_json::json!({ "config": config, "windows": reports });
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&out, serde_json::to_string_pretty(&doc)?)?;
    // Every decision of the full run, one per line: the audit trail.
    let trail = out.with_file_name("backtest_decisions.jsonl");
    let lines: Vec<String> = decisions.iter().map(serde_json::to_string).collect::<Result<_, _>>()?;
    std::fs::write(&trail, lines.join("
") + "
")?;
    println!("wrote {} and {}", out.display(), trail.display());
    Ok(())
}
