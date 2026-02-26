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
//!    and — in **live mode** — submits a market order through the authenticated
//!    CLOB client.  In **dry-run mode** the order is only simulated and the
//!    position size is deducted from the virtual balance.
//!
//! The whale list is re-read at the start of every subscription loop so that
//! investigation-cycle updates are picked up automatically.

pub mod kelly;

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
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
use crate::state::{SharedState, TradeRecord};

/// Alias for the shared whale list.
pub type WhaleList = Arc<RwLock<Vec<TraderProfile>>>;

/// Spawn the trading task.
pub fn spawn(
    config: Config,
    whale_list: WhaleList,
    shared_state: SharedState,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Err(e) = run_trading_loop(&config, &whale_list, &shared_state).await {
                error!("Trading loop error: {e:#}");
            }
            warn!("Trading loop restarting in 5 s…");
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        }
    })
}

/// One continuous trading loop iteration.
/// Exits (to trigger a restart) only on unrecoverable errors.
async fn run_trading_loop(
    config: &Config,
    whale_list: &WhaleList,
    shared_state: &SharedState,
) -> Result<()> {
    // ── Authenticated CLOB client (live mode only) ────────────────────────
    // In dry-run mode we skip authentication entirely — no credentials needed.
    // Declare as Option<(signer, client)> with type inferred from the Some branch.
    let mut clob_auth = None;
    if !config.dry_run {
        let signer = LocalSigner::from_str(&config.private_key)
            .context("Invalid POLYMARKET_PRIVATE_KEY")?
            .with_chain_id(Some(POLYGON));
        let client = clob::Client::default()
            .authentication_builder(&signer)
            .authenticate()
            .await
            .context("CLOB authentication failed")?;
        info!("CLOB client authenticated");
        clob_auth = Some((signer, client));
    } else {
        info!("Dry-run mode: skipping CLOB authentication");
    }

    // ── Track the last trade seen per whale to avoid duplicate copies ─────
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

        let poll_duration =
            tokio::time::Duration::from_secs(config.poll_interval_secs);
        let mut poll_interval = tokio::time::interval(poll_duration);
        poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let whale_snapshot: Vec<String> =
            whales.iter().map(|w| w.address.clone()).collect();

        loop {
            // Restart subscription when whale list changes.
            {
                let current: Vec<String> = whale_list
                    .read()
                    .await
                    .iter()
                    .map(|w| w.address.clone())
                    .collect();
                if current != whale_snapshot {
                    info!("Whale list changed — restarting WebSocket subscription");
                    break;
                }
            }

            tokio::select! {
                maybe_event = stream.next() => {
                    match maybe_event {
                        Some(Ok(book)) => {
                            debug!(asset = %book.asset_id, "Orderbook update received");
                            let whales_snap = whale_list.read().await.clone();
                            check_and_copy(config, &clob_auth, &whales_snap, &mut last_seen, shared_state).await;
                        }
                        Some(Err(e)) => {
                            warn!("WebSocket error: {e}");
                            break;
                        }
                        None => {
                            warn!("WebSocket stream ended");
                            break;
                        }
                    }
                }

                _ = poll_interval.tick() => {
                    let whales_snap = whale_list.read().await.clone();
                    check_and_copy(config, &clob_auth, &whales_snap, &mut last_seen, shared_state).await;
                }
            }
        }
    }
}

/// Query each whale's recent trades via the Data API, detect new ones, and
/// either place a real order (live mode) or simulate it (dry-run mode).
async fn check_and_copy<S>(
    config: &Config,
    clob_auth: &Option<(S, clob::Client<polymarket_client_sdk::auth::state::Authenticated<polymarket_client_sdk::auth::Normal>>)>,
    whales: &[TraderProfile],
    last_seen: &mut HashMap<String, String>,
    shared_state: &SharedState,
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

        let Some(newest) = trades.first() else {
            continue;
        };

        let trade_key = format!("{}-{}", newest.condition_id, newest.timestamp);
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

        let market_price = match newest.price.to_f64() {
            Some(p) if p > 0.0 && p < 1.0 => p,
            _ => {
                warn!(trade_key = %trade_key, "Unusual price; skipping");
                continue;
            }
        };

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
                info!(whale = %whale.address, "No edge detected — trade skipped");
                continue;
            }
        };

        info!(
            whale = %whale.address,
            kelly_fraction = format_args!("{:.4}", kr.adjusted_fraction),
            position_usdc = format_args!("{:.2}", kr.position_size_usdc),
            dry_run = config.dry_run,
            "{}",
            if config.dry_run { "Simulating copy trade" } else { "Placing copy order" }
        );

        let side_str = match newest.side {
            polymarket_client_sdk::data::types::Side::Buy => "Buy",
            polymarket_client_sdk::data::types::Side::Sell => "Sell",
            _ => {
                warn!(trade_key = %trade_key, "Unknown side; skipping");
                continue;
            }
        };

        // Record the trade in shared state (always, live and dry-run).
        {
            let record = TradeRecord {
                timestamp: Utc::now(),
                whale_address: whale.address.clone(),
                market: newest.condition_id.to_string(),
                side: side_str.to_string(),
                price: market_price,
                size_usdc: kr.position_size_usdc,
                kelly_fraction: kr.adjusted_fraction,
                simulated: config.dry_run,
            };
            shared_state.write().await.record_trade(record);
        }

        if config.dry_run {
            info!(
                position_usdc = format_args!("{:.2}", kr.position_size_usdc),
                "[DRY RUN] Trade simulated — no real order placed"
            );
            continue;
        }

        // ── Live order placement ──────────────────────────────────────────
        let (signer, clob_client) = match clob_auth {
            Some(pair) => pair,
            None => {
                error!("clob_auth is None in live mode — this is a bug");
                continue;
            }
        };

        let token_id = match U256::from_str(&newest.asset.to_string()) {
            Ok(id) => id,
            Err(e) => {
                warn!(trade_key = %trade_key, "Cannot parse asset ID: {e}; skipping");
                continue;
            }
        };

        let side = match newest.side {
            polymarket_client_sdk::data::types::Side::Buy => Side::Buy,
            polymarket_client_sdk::data::types::Side::Sell => Side::Sell,
            _ => continue,
        };

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
                info!(order_id = %r.order_id, success = r.success, "Copy order placed");
            }
            Err(e) => {
                error!("Failed to post order: {e}");
            }
        }
    }
}

