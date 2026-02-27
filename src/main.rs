//! Polymarket Copytrading Bot
//!
//! Spawns concurrent Tokio tasks that share state:
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
//! - **Alerts task** — optionally pushes skip-pressure spike alerts to a
//!   webhook when `SKIP_ALERT_WEBHOOK_URL` is configured.
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

mod alerts;
mod config;
mod dashboard;
mod investigation;
mod persistence;
mod state;
mod trading;

use std::sync::Arc;

use anyhow::Result;
use persistence::PersistenceStore;
use tokio::sync::RwLock;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env before anything else.
    let _ = dotenv::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = config::Config::from_env()?;

    info!(
        dry_run = config.dry_run,
        dry_run_balance_usdc = config.dry_run_balance_usdc,
        max_whales = config.max_whales,
        bankroll_usdc = config.bankroll_usdc,
        kelly_fraction = config.kelly_fraction,
        bayes_prior_alpha = config.bayes_prior_alpha,
        bayes_prior_beta = config.bayes_prior_beta,
        bayes_confidence_z = config.bayes_confidence_z,
        estimated_fee_bps = config.estimated_fee_bps,
        estimated_slippage_bps = config.estimated_slippage_bps,
        adverse_selection_bps = config.adverse_selection_bps,
        min_net_edge_bps = config.min_net_edge_bps,
        max_copy_price_drift_bps = config.max_copy_price_drift_bps,
        max_order_slippage_bps = config.max_order_slippage_bps,
        investigation_interval_secs = config.investigation_interval_secs,
        dashboard_port = config.dashboard_port,
        skip_alert_webhook_enabled = config.skip_alert_webhook_url.is_some(),
        skip_alert_1h_threshold = config.skip_alert_1h_threshold,
        skip_alert_cooldown_secs = config.skip_alert_cooldown_secs,
        skip_alert_eval_interval_secs = config.skip_alert_eval_interval_secs,
        skip_alert_max_retries = config.skip_alert_max_retries,
        skip_alert_retry_backoff_ms = config.skip_alert_retry_backoff_ms,
        risk_daily_loss_limit_usdc = config.daily_loss_limit_usdc,
        risk_max_consecutive_losses = config.max_consecutive_losses,
        risk_max_notional_per_market_usdc = config.max_notional_per_market_usdc,
        state_dir = %config.state_dir,
        state_flush_interval_secs = config.state_flush_interval_secs,
        account_sync_interval_secs = config.account_sync_interval_secs,
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
    let persistence = Arc::new(PersistenceStore::new(config.state_dir.clone()));
    persistence.ensure_dir()?;

    if let Some(snapshot) = persistence.load_bot_state()? {
        shared_state.write().await.restore_from_snapshot(snapshot);
        info!("Loaded persisted bot state snapshot");
    }

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
        Arc::clone(&persistence),
    );

    let dashboard_handle = dashboard::spawn(
        config.dashboard_port,
        Arc::clone(&shared_state),
        Arc::clone(&whale_list),
        config.skip_alert_1h_threshold,
        config.daily_loss_limit_usdc,
        config.max_consecutive_losses,
        config.max_notional_per_market_usdc,
    );

    let alerts_handle = alerts::spawn(
        config.clone(),
        Arc::clone(&shared_state),
        Arc::clone(&persistence),
    );
    if alerts_handle.is_none() {
        info!("Skip alert webhook disabled (set SKIP_ALERT_WEBHOOK_URL to enable)");
    }

    let state_flush_handle = {
        let shared_state = Arc::clone(&shared_state);
        let persistence = Arc::clone(&persistence);
        let flush_secs = config.state_flush_interval_secs;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(tokio::time::Duration::from_secs(flush_secs));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                let snapshot = shared_state.read().await.snapshot();
                if let Err(e) = persistence.save_bot_state(&snapshot) {
                    tracing::warn!("Failed persisting bot state snapshot: {e:#}");
                }
            }
        })
    };

    info!(
        url = format!("http://localhost:{}", config.dashboard_port),
        "Dashboard available"
    );

    // Run all tasks concurrently; propagate panics. On Ctrl+C, force a final
    // flush so runtime state survives process restarts.
    if let Some(alerts_handle) = alerts_handle {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("Shutdown signal received; flushing state snapshot");
                let snapshot = shared_state.read().await.snapshot();
                persistence.save_bot_state(&snapshot)?;
                return Ok(());
            }
            res = async {
                tokio::try_join!(
                    async { investigation_handle.await.map_err(anyhow::Error::from) },
                    async { trading_handle.await.map_err(anyhow::Error::from) },
                    async { dashboard_handle.await.map_err(anyhow::Error::from) },
                    async { alerts_handle.await.map_err(anyhow::Error::from) },
                    async { state_flush_handle.await.map_err(anyhow::Error::from) },
                )
            } => {
                res?;
            }
        }
    } else {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("Shutdown signal received; flushing state snapshot");
                let snapshot = shared_state.read().await.snapshot();
                persistence.save_bot_state(&snapshot)?;
                return Ok(());
            }
            res = async {
                tokio::try_join!(
                    async { investigation_handle.await.map_err(anyhow::Error::from) },
                    async { trading_handle.await.map_err(anyhow::Error::from) },
                    async { dashboard_handle.await.map_err(anyhow::Error::from) },
                    async { state_flush_handle.await.map_err(anyhow::Error::from) },
                )
            } => {
                res?;
            }
        }
    }

    Ok(())
}
