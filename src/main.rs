//! Polymarket Copytrading Bot
//!
//! Spawns two concurrent Tokio tasks that share a whale list:
//!
//! - **Investigation task** — periodically queries the Polymarket Data API to
//!   rank traders by recent ROI and win rate, then updates the shared whale list
//!   (at most [`Config::max_whales`] traders, capped at 10).
//!
//! - **Trading task** — subscribes to real-time Polymarket CLOB WebSocket
//!   orderbook feeds for the markets where tracked whales are active.  When a
//!   new whale trade is detected via the Data API, it sizes a copy position
//!   using the Kelly criterion and submits a market order through the CLOB.
//!
//! ## Quick start
//!
//! ```sh
//! cp .env.example .env
//! # Fill in POLYMARKET_PRIVATE_KEY, POLYMARKET_API_KEY, etc.
//! cargo run --release
//! ```

mod config;
mod investigation;
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
        max_whales = config.max_whales,
        bankroll_usdc = config.bankroll_usdc,
        kelly_fraction = config.kelly_fraction,
        investigation_interval_secs = config.investigation_interval_secs,
        "Copytrading bot starting"
    );

    // Shared state: the investigation thread writes, the trading thread reads.
    let whale_list = Arc::new(RwLock::new(Vec::new()));

    let investigation_handle =
        investigation::spawn(config.clone(), Arc::clone(&whale_list));

    let trading_handle =
        trading::spawn(config, Arc::clone(&whale_list));

    // Run both tasks concurrently; propagate panics.
    tokio::try_join!(
        async { investigation_handle.await.map_err(anyhow::Error::from) },
        async { trading_handle.await.map_err(anyhow::Error::from) },
    )?;

    Ok(())
}
