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

/// Minimum USDC position size — below this Polymarket rejects orders and it's
/// not worth the gas/fees.
const MIN_POSITION_USDC: f64 = 1.0;

/// Minimum remaining balance (USDC) before the bot stops opening new positions.
const MIN_BALANCE_USDC: f64 = 5.0;

/// Maximum fraction of bankroll that can be deployed into a single market
/// across all whales combined (concentration limit).
const MAX_MARKET_CONCENTRATION: f64 = 0.30;

/// Maximum age (in seconds) of a whale profile before it's considered stale.
/// If the investigation cycle hasn't refreshed profiles in this window, the
/// trading loop will skip copying trades from stale profiles.
const MAX_WHALE_STALENESS_SECS: i64 = 3 * 3600; // 3 hours

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

    // ── Track recent trade keys per whale to avoid duplicate copies ───────
    // We store a *set* of recent trade keys per whale (not just the latest)
    // so that if a whale makes multiple trades between polls, we catch all
    // of them instead of silently missing intermediate ones.
    let mut seen_trades: HashMap<String, HashSet<String>> = HashMap::new();

    // ── Reuse a single Data API client across all polls ──────────────────
    let data_client = DataClient::default();

    // ── Per-market exposure tracking ─────────────────────────────────────
    let mut market_exposure: HashMap<String, f64> = HashMap::new();

    loop {
        let whales: Vec<TraderProfile> = whale_list.read().await.clone();

        if whales.is_empty() {
            info!("Whale list is empty — waiting for investigation cycle…");
            tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
            continue;
        }

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
                            check_and_copy(config, &clob_auth, &whales_snap, &mut seen_trades, &mut market_exposure, shared_state, &data_client).await;
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
                    check_and_copy(config, &clob_auth, &whales_snap, &mut seen_trades, &mut market_exposure, shared_state, &data_client).await;
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
    seen_trades: &mut HashMap<String, HashSet<String>>,
    market_exposure: &mut HashMap<String, f64>,
    shared_state: &SharedState,
    data_client: &DataClient,
) where
    S: polymarket_client_sdk::auth::Signer + Sync,
{
    // ── Check minimum balance before doing anything ──────────────────────
    {
        let state = shared_state.read().await;
        if state.balance_usdc < MIN_BALANCE_USDC {
            debug!(
                balance = state.balance_usdc,
                "Balance below minimum threshold (${MIN_BALANCE_USDC}) — skipping trade cycle"
            );
            return;
        }
    }

    for whale in whales {
        // Skip whales with stale profiles — if the investigation cycle has
        // not refreshed data in MAX_WHALE_STALENESS_SECS, the win_rate and
        // score may be outdated and unreliable.
        let age_secs = (Utc::now() - whale.updated_at).num_seconds();
        if age_secs > MAX_WHALE_STALENESS_SECS {
            warn!(
                whale = %whale.address,
                age_hours = age_secs / 3600,
                "Whale profile is stale — skipping until investigation refreshes"
            );
            continue;
        }

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

        // Process all new trades (not just the newest) to avoid missing
        // intermediate trades between polls.
        let whale_seen = seen_trades
            .entry(whale.address.clone())
            .or_insert_with(HashSet::new);

        for trade in &trades {
            let trade_key = format!("{}-{}", trade.condition_id, trade.timestamp);
            if whale_seen.contains(&trade_key) {
                continue;
            }
            whale_seen.insert(trade_key.clone());

            info!(
                whale = %whale.address,
                trade_key = %trade_key,
                side = ?trade.side,
                price = %trade.price,
                size = %trade.size,
                "New whale trade detected — evaluating copy"
            );

            let market_price = match trade.price.to_f64() {
                Some(p) if p > 0.0 && p < 1.0 => p,
                _ => {
                    warn!(trade_key = %trade_key, "Unusual price; skipping");
                    continue;
                }
            };

            // ── Adjust price for Sell (NO) trades ───────────────────────
            // When a whale sells YES tokens, they're effectively betting NO.
            // The Kelly formula needs the effective price from the whale's
            // perspective: for a NO bet, effective_price = 1 - market_price.
            let is_sell = matches!(
                trade.side,
                polymarket_client_sdk::data::types::Side::Sell
            );
            let effective_price = if is_sell {
                1.0 - market_price
            } else {
                market_price
            };

            // ── Use current balance as bankroll, not the static config ──
            let current_balance = shared_state.read().await.balance_usdc;
            if current_balance < MIN_BALANCE_USDC {
                info!(
                    balance = current_balance,
                    "Remaining balance below ${MIN_BALANCE_USDC} — stopping trades"
                );
                return;
            }

            let kelly_result = kelly::kelly(
                effective_price,
                whale.win_rate,
                current_balance,
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

            // ── Enforce minimum position size ───────────────────────────
            if kr.position_size_usdc < MIN_POSITION_USDC {
                info!(
                    whale = %whale.address,
                    position_usdc = format_args!("{:.2}", kr.position_size_usdc),
                    "Position below minimum ${MIN_POSITION_USDC} — skipping"
                );
                continue;
            }

            // ── Check per-market concentration ──────────────────────────
            let market_id = trade.condition_id.to_string();
            let current_market_exp = market_exposure.get(&market_id).copied().unwrap_or(0.0);
            let max_market_usdc = config.bankroll_usdc * MAX_MARKET_CONCENTRATION;
            if current_market_exp + kr.position_size_usdc > max_market_usdc {
                let remaining = (max_market_usdc - current_market_exp).max(0.0);
                if remaining < MIN_POSITION_USDC {
                    info!(
                        whale = %whale.address,
                        market = %market_id,
                        exposure = format_args!("{:.2}", current_market_exp),
                        "Market concentration limit reached — skipping"
                    );
                    continue;
                }
                // Reduce position to fit within the concentration limit.
                info!(
                    whale = %whale.address,
                    market = %market_id,
                    original = format_args!("{:.2}", kr.position_size_usdc),
                    capped = format_args!("{:.2}", remaining),
                    "Capping position to market concentration limit"
                );
            }
            let final_size = kr.position_size_usdc.min(
                max_market_usdc - current_market_exp,
            ).max(0.0);
            if final_size < MIN_POSITION_USDC {
                continue;
            }

            let side_str = match trade.side {
                polymarket_client_sdk::data::types::Side::Buy => "Buy",
                polymarket_client_sdk::data::types::Side::Sell => "Sell",
                _ => {
                    warn!(trade_key = %trade_key, "Unknown side; skipping");
                    continue;
                }
            };

            info!(
                whale = %whale.address,
                kelly_fraction = format_args!("{:.4}", kr.adjusted_fraction),
                position_usdc = format_args!("{:.2}", final_size),
                dry_run = config.dry_run,
                "{}",
                if config.dry_run { "Simulating copy trade" } else { "Placing copy order" }
            );

            // Record the trade in shared state (always, live and dry-run).
            {
                let record = TradeRecord {
                    timestamp: Utc::now(),
                    whale_address: whale.address.clone(),
                    market: market_id.clone(),
                    side: side_str.to_string(),
                    price: market_price,
                    size_usdc: final_size,
                    kelly_fraction: kr.adjusted_fraction,
                    simulated: config.dry_run,
                };
                shared_state.write().await.record_trade(record);
            }

            // Update per-market exposure tracking.
            *market_exposure.entry(market_id).or_insert(0.0) += final_size;

            if config.dry_run {
                info!(
                    position_usdc = format_args!("{:.2}", final_size),
                    "[DRY RUN] Trade simulated — no real order placed"
                );
                continue;
            }

            // ── Live order placement ────────────────────────────────────
            let (signer, clob_client) = match clob_auth {
                Some(pair) => pair,
                None => {
                    error!("clob_auth is None in live mode — this is a bug");
                    continue;
                }
            };

            let token_id = match U256::from_str(&trade.asset.to_string()) {
                Ok(id) => id,
                Err(e) => {
                    warn!(trade_key = %trade_key, "Cannot parse asset ID: {e}; skipping");
                    continue;
                }
            };

            let side = match trade.side {
                polymarket_client_sdk::data::types::Side::Buy => Side::Buy,
                polymarket_client_sdk::data::types::Side::Sell => Side::Sell,
                _ => continue,
            };

            let usdc_amount = match Decimal::try_from(final_size) {
                Ok(d) => d,
                Err(e) => {
                    warn!(trade_key = %trade_key, "Cannot convert position size to Decimal: {e}; skipping");
                    continue;
                }
            };
            let amount = match Amount::usdc(usdc_amount) {
                Ok(a) => a,
                Err(e) => {
                    warn!("Invalid position amount: {e}");
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

        // Cap the seen-trades set per whale to prevent unbounded memory growth.
        if whale_seen.len() > 100 {
            whale_seen.clear();
            // After clearing, re-insert keys from the current batch so we
            // don't re-process these trades on the next poll.
            for t in &trades {
                whale_seen.insert(format!("{}-{}", t.condition_id, t.timestamp));
            }
        }
    }
}

