//! Polymarket Copytrading Bot
//!
//! Spawns three concurrent Tokio tasks that share state:
//!
//! - **Investigation task** — periodically queries the Polymarket Data API to
//!   rank traders by recent ROI and win rate, then updates the shared whale list
//!   (at most [`Config::max_whales`] traders, capped at 10).
//!
//! - **Trading task** — subscribes to real-time Polymarket CLOB WebSocket
//!   orderbook feeds for the markets where tracked whales are active.  When a
//!   new whale trade is detected via the Data API, it sizes a copy position
//!   using the Kelly criterion and — in live mode — submits a market order
//!   through the CLOB.  In dry-run mode the order is only simulated.
//!
//! - **Dashboard task** — serves a simple web dashboard on `DASHBOARD_PORT`
//!   (default 8080) so you can inspect the bot's state in real time.
//!
//! ## Quick start (dry-run, no credentials needed)
//!
//! ```sh
//! DRY_RUN=true cargo run --release
//! # Then open http://localhost:8080
//! ```
//!
//! ## Live trading
//!
//! ```sh
//! cp .env.example .env
//! # Fill in POLYMARKET_PRIVATE_KEY, POLYMARKET_API_KEY, etc.
//! cargo run --release
//! ```

mod config;
mod dashboard;
mod investigation;
mod state;
mod trading;

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::RwLock;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env before anything else.
    let _ = dotenv::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = config::Config::from_env()?;

    info!(
        dry_run = config.dry_run,
        dry_run_balance_usdc = config.dry_run_balance_usdc,
        max_whales = config.max_whales,
        bankroll_usdc = config.bankroll_usdc,
        kelly_fraction = config.kelly_fraction,
        investigation_interval_secs = config.investigation_interval_secs,
        dashboard_port = config.dashboard_port,
        "Copytrading bot starting"
    );

    if config.dry_run {
        info!(
            balance = config.dry_run_balance_usdc,
            "Running in DRY-RUN mode — no real orders will be placed"
        );
    }

    // ── Shared state ─────────────────────────────────────────────────────────
    // Balance used in dry-run is the dry_run_balance; in live mode it starts
    // at the configured bankroll (real-time balance tracking is not yet wired
    // to on-chain data, so it serves as a reference only).
    let initial_balance = if config.dry_run {
        config.dry_run_balance_usdc
    } else {
        config.bankroll_usdc
    };
    let shared_state = Arc::new(RwLock::new(state::BotState::new(
        config.dry_run,
        initial_balance,
    )));

    let whale_list = Arc::new(RwLock::new(Vec::new()));

    // ── Spawn tasks ───────────────────────────────────────────────────────────
    let investigation_handle = investigation::spawn(
        config.clone(),
        Arc::clone(&whale_list),
        Arc::clone(&shared_state),
    );

    let trading_handle = trading::spawn(
        config.clone(),
        Arc::clone(&whale_list),
        Arc::clone(&shared_state),
    );

    let dashboard_handle = dashboard::spawn(
        config.dashboard_port,
        Arc::clone(&shared_state),
        Arc::clone(&whale_list),
    );

    info!(
        url = format!("http://localhost:{}", config.dashboard_port),
        "Dashboard available"
    );

    // Run all tasks concurrently; propagate panics.
    tokio::try_join!(
        async { investigation_handle.await.map_err(anyhow::Error::from) },
        async { trading_handle.await.map_err(anyhow::Error::from) },
        async { dashboard_handle.await.map_err(anyhow::Error::from) },
    )?;

    Ok(())
}

