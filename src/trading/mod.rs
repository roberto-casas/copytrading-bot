//! Trading thread — subscribes to real-time Polymarket orderbook updates via
//! WebSocket and copies new whale trades using the Kelly criterion.
//!
//! ## How it works
//!
//! 1. Reads the current whale list from the shared [`WhaleList`].
//! 2. Collects every asset ID the whales currently hold (from their open
//!    positions recorded during investigation).
//! 3. Opens a WebSocket connection to the Polymarket CLOB and subscribes to
//!    orderbook updates for those asset IDs.
//! 4. Each orderbook event signals activity in that market.  The thread then
//!    checks — via the Data API — whether any whale made a *new* trade in that
//!    market since the last check.
//! 5. When a fresh whale trade is confirmed it computes a Kelly-sized position
//!    and submits a market order through the authenticated CLOB client.
//!
//! The whale list is re-read at the start of every subscription loop so that
//! investigation-cycle updates are picked up automatically.

pub mod kelly;

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result};
use futures::StreamExt;
use polymarket_client_sdk::auth::{LocalSigner, Signer as _};
use polymarket_client_sdk::clob::types::{Amount, Side};
use polymarket_client_sdk::clob::ws::Client as WsClient;
use polymarket_client_sdk::clob::{self};
use polymarket_client_sdk::data::types::request::TradesRequest as DataTradesRequest;
use polymarket_client_sdk::data::Client as DataClient;
use polymarket_client_sdk::types::{Address, Decimal, U256};
use polymarket_client_sdk::POLYGON;
use rust_decimal::prelude::ToPrimitive;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::investigation::scoring::TraderProfile;

/// Alias for the shared whale list.
pub type WhaleList = Arc<RwLock<Vec<TraderProfile>>>;

/// Spawn the trading task.
pub fn spawn(config: Config, whale_list: WhaleList) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Err(e) = run_trading_loop(&config, &whale_list).await {
                error!("Trading loop error: {e:#}");
            }
            warn!("Trading loop restarting in 5 s…");
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        }
    })
}

/// One continuous trading loop iteration.
/// Exits (to trigger a restart) only on unrecoverable errors.
async fn run_trading_loop(config: &Config, whale_list: &WhaleList) -> Result<()> {
    // ── Authenticated CLOB client for order placement ─────────────────────
    let signer = LocalSigner::from_str(&config.private_key)
        .context("Invalid POLYMARKET_PRIVATE_KEY")?
        .with_chain_id(Some(POLYGON));

    let clob_client = clob::Client::default()
        .authentication_builder(&signer)
        .authenticate()
        .await
        .context("CLOB authentication failed")?;

    info!("CLOB client authenticated");

    // ── Track the last trade seen per whale to avoid duplicate copies ─────
    // Maps whale_address → last seen trade ID.
    let mut last_seen: HashMap<String, String> = HashMap::new();

    loop {
        // Re-read whale list so investigation-cycle updates take effect.
        let whales: Vec<TraderProfile> = whale_list.read().await.clone();

        if whales.is_empty() {
            info!("Whale list is empty — waiting for investigation cycle…");
            tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
            continue;
        }

        // Collect unique asset IDs across all whales for WS subscription.
        let asset_ids: Vec<U256> = whales
            .iter()
            .flat_map(|w| w.active_asset_ids.iter())
            .collect::<HashSet<_>>()
            .into_iter()
            .filter_map(|s| U256::from_str(s).ok())
            .collect();

        if asset_ids.is_empty() {
            info!("No active asset IDs yet — waiting…");
            tokio::time::sleep(tokio::time::Duration::from_secs(
                config.poll_interval_secs,
            ))
            .await;
            continue;
        }

        info!(
            whales = whales.len(),
            markets = asset_ids.len(),
            "Starting WebSocket subscription"
        );

        // ── WebSocket: subscribe to orderbook updates ─────────────────────
        let ws_client = WsClient::default();
        let stream = ws_client
            .subscribe_orderbook(asset_ids.clone())
            .context("Failed to create orderbook subscription")?;
        let mut stream = Box::pin(stream);

        // ── Main event loop ───────────────────────────────────────────────
        // We interleave WS events with periodic polls.  A WS orderbook event
        // signals that something is happening in a market; we then query the
        // Data API to see if a whale was the trader.
        let poll_duration =
            tokio::time::Duration::from_secs(config.poll_interval_secs);
        let mut poll_interval = tokio::time::interval(poll_duration);
        poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        // Snapshot whale list version so we can detect changes and re-subscribe.
        let whale_snapshot: Vec<String> =
            whales.iter().map(|w| w.address.clone()).collect();

        loop {
            // Check whether the whale list has changed → restart subscription.
            {
                let current: Vec<String> = whale_list
                    .read()
                    .await
                    .iter()
                    .map(|w| w.address.clone())
                    .collect();
                if current != whale_snapshot {
                    info!("Whale list changed — restarting WebSocket subscription");
                    break; // Re-enter outer loop with fresh subscription.
                }
            }

            tokio::select! {
                maybe_event = stream.next() => {
                    match maybe_event {
                        Some(Ok(book)) => {
                            debug!(asset = %book.asset_id, "Orderbook update received");
                            // An orderbook update means activity in this market.
                            // Check whale trades for the affected asset.
                            let whales_snap = whale_list.read().await.clone();
                            check_and_copy(
                                config,
                                &signer,
                                &clob_client,
                                &whales_snap,
                                &mut last_seen,
                            ).await;
                        }
                        Some(Err(e)) => {
                            warn!("WebSocket error: {e}");
                            break; // Reconnect.
                        }
                        None => {
                            warn!("WebSocket stream ended");
                            break;
                        }
                    }
                }

                _ = poll_interval.tick() => {
                    // Periodic baseline poll regardless of WS activity.
                    let whales_snap = whale_list.read().await.clone();
                    check_and_copy(
                        config,
                        &signer,
                        &clob_client,
                        &whales_snap,
                        &mut last_seen,
                    ).await;
                }
            }
        }
    }
}

/// Query each whale's recent trades via the Data API, detect new ones, and
/// place a Kelly-sized copy order for any fresh trade found.
async fn check_and_copy<S>(
    config: &Config,
    signer: &S,
    clob_client: &clob::Client<polymarket_client_sdk::auth::state::Authenticated<polymarket_client_sdk::auth::Normal>>,
    whales: &[TraderProfile],
    last_seen: &mut HashMap<String, String>,
) where
    S: polymarket_client_sdk::auth::Signer + Sync,
{
    let data_client = DataClient::default();

    for whale in whales {
        let addr: Address = match Address::from_str(&whale.address) {
            Ok(a) => a,
            Err(e) => {
                warn!(address = %whale.address, "Cannot parse address: {e}");
                continue;
            }
        };

        // Fetch the whale's most recent trades (limit 5 to stay fast).
        let trades = match data_client
            .trades(
                &DataTradesRequest::builder()
                    .user(addr)
                    .limit(5)
                    .expect("5 is within valid range")
                    .build(),
            )
            .await
        {
            Ok(t) => t,
            Err(e) => {
                debug!(address = %whale.address, "Data API error: {e}");
                continue;
            }
        };

        // The API returns trades newest-first.
        let Some(newest) = trades.first() else {
            continue;
        };

        // Build a stable unique key from condition_id + timestamp (Trade has no id field).
        let trade_key = format!("{}-{}", newest.condition_id, newest.timestamp);

        // Skip if we've already processed this trade.
        if last_seen.get(&whale.address).map(|s| s.as_str()) == Some(&trade_key) {
            continue;
        }

        last_seen.insert(whale.address.clone(), trade_key.clone());

        info!(
            whale = %whale.address,
            trade_key = %trade_key,
            side = ?newest.side,
            price = %newest.price,
            size = %newest.size,
            "New whale trade detected — evaluating copy"
        );

        // Determine the market price (from the trade itself).
        let market_price = match newest.price.to_f64() {
            Some(p) if p > 0.0 && p < 1.0 => p,
            _ => {
                warn!(trade_key = %trade_key, "Unusual price; skipping");
                continue;
            }
        };

        // Apply Kelly criterion.
        let kelly_result = kelly::kelly(
            market_price,
            whale.win_rate,
            config.bankroll_usdc,
            config.kelly_fraction,
            config.max_position_fraction,
        );

        let kr = match kelly_result {
            Some(k) => k,
            None => {
                info!(
                    whale = %whale.address,
                    "No edge detected by Kelly criterion — trade skipped"
                );
                continue;
            }
        };

        info!(
            whale = %whale.address,
            kelly_fraction = format_args!("{:.4}", kr.adjusted_fraction),
            position_usdc = format_args!("{:.2}", kr.position_size_usdc),
            "Placing copy order"
        );

        // Determine which token to buy (same side as whale).
        // `newest.asset` is already a U256; convert via its string representation.
        let token_id = match U256::from_str(&newest.asset.to_string()) {
            Ok(id) => id,
            Err(e) => {
                warn!(trade_key = %trade_key, "Cannot parse asset ID as U256: {e}; skipping");
                continue;
            }
        };

        let side = match newest.side {
            polymarket_client_sdk::data::types::Side::Buy => Side::Buy,
            polymarket_client_sdk::data::types::Side::Sell => Side::Sell,
            _ => {
                warn!(trade_key = %trade_key, "Unknown trade side; skipping");
                continue;
            }
        };

        // Build a market order for the Kelly-computed USDC amount.
        let usdc_amount = match Decimal::try_from(kr.position_size_usdc) {
            Ok(d) => d,
            Err(e) => {
                warn!(trade_key = %trade_key, "Cannot convert Kelly size to Decimal: {e}; skipping");
                continue;
            }
        };
        let amount = match Amount::usdc(usdc_amount) {
            Ok(a) => a,
            Err(e) => {
                warn!("Invalid Kelly amount: {e}");
                continue;
            }
        };

        let order = match clob_client
            .market_order()
            .token_id(token_id)
            .amount(amount)
            .side(side)
            .build()
            .await
        {
            Ok(o) => o,
            Err(e) => {
                error!("Failed to build market order: {e}");
                continue;
            }
        };

        let signed = match clob_client.sign(signer, order).await {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to sign order: {e}");
                continue;
            }
        };

        match clob_client.post_order(signed).await {
            Ok(r) => {
                info!(
                    order_id = %r.order_id,
                    success = r.success,
                    "Copy order placed"
                );
            }
            Err(e) => {
                error!("Failed to post order: {e}");
            }
        }
    }
}
