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
//!    CLOB client.  In **dry-run mode** the order is only simulated.
//!
//! The whale list is re-read at the start of every subscription loop so that
//! investigation-cycle updates are picked up automatically.

pub mod kelly;

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Utc;
use futures::StreamExt;
use polymarket_client_sdk::auth::{LocalSigner, Signer as _};
use polymarket_client_sdk::clob::types::request::PriceRequest as ClobPriceRequest;
use polymarket_client_sdk::clob::types::{Amount, Side};
use polymarket_client_sdk::clob::ws::Client as WsClient;
use polymarket_client_sdk::clob::{self};
use polymarket_client_sdk::data::types::request::{
    ClosedPositionsRequest, PositionsRequest, TradesRequest as DataTradesRequest,
};
use polymarket_client_sdk::data::types::response::Trade as DataTrade;
use polymarket_client_sdk::data::types::response::{ClosedPosition, Position};
use polymarket_client_sdk::data::Client as DataClient;
use polymarket_client_sdk::types::{Address, Decimal, U256};
use polymarket_client_sdk::POLYGON;
use rust_decimal::prelude::ToPrimitive;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::investigation::scoring::TraderProfile;
use crate::persistence::PersistenceStore;
use crate::state::{MarketPosition, SharedState, TradeRecord};

/// Alias for the shared whale list.
pub type WhaleList = Arc<RwLock<Vec<TraderProfile>>>;

/// Minimum USDC position size — below this Polymarket rejects orders and it's
/// not worth the gas/fees.
const MIN_POSITION_USDC: f64 = 1.0;

/// Minimum remaining balance (USDC) before the bot stops opening new positions.
const MIN_BALANCE_USDC: f64 = 5.0;
/// Minimum shares size accepted for sell-side conversion.
const MIN_SHARES: f64 = 0.01;

/// Maximum fraction of bankroll that can be deployed into a single market
/// across all whales combined (concentration limit).
const MAX_MARKET_CONCENTRATION: f64 = 0.30;

/// Maximum age (in seconds) of a whale profile before it's considered stale.
/// If the investigation cycle hasn't refreshed profiles in this window, the
/// trading loop will skip copying trades from stale profiles.
const MAX_WHALE_STALENESS_SECS: i64 = 3 * 3600; // 3 hours

/// Burst-control debounce for orderbook-triggered checks. The periodic poll
/// still runs independently, so this only smooths WS spikes.
const WS_CHECK_DEBOUNCE_MS: u64 = 750;

/// Number of recent trades to fetch per Data API page.
const TRADE_PAGE_LIMIT: i32 = 25;

/// Maximum pages to fetch per whale on each check (bounded for latency/rate).
const MAX_TRADE_PAGES: i32 = 4;

/// Keep at most this many seen trade keys per whale.
const MAX_SEEN_TRADE_KEYS_PER_WHALE: usize = 200;

/// Drop stale market exposure entries after this many seconds to avoid
/// unbounded growth in long-running processes.
const MARKET_EXPOSURE_TTL_SECS: i64 = 24 * 3600;

/// Persist seen-trade cache at this cadence while it is changing.
const SEEN_TRADES_FLUSH_SECS: u64 = 15;
/// Page size used for account reconciliation endpoints.
const ACCOUNT_SYNC_PAGE_LIMIT: i32 = 200;

#[derive(Debug, Clone)]
struct MarketExposure {
    amount_usdc: f64,
    updated_at: chrono::DateTime<Utc>,
}

struct TradingRuntime {
    seen_trades: HashMap<String, HashSet<String>>,
    market_exposure: HashMap<String, MarketExposure>,
    seen_dirty: bool,
    last_seen_persist_at: Instant,
    last_account_sync_at: Instant,
}

impl TradingRuntime {
    fn new(seen_trades: HashMap<String, HashSet<String>>) -> Self {
        Self {
            seen_trades,
            market_exposure: HashMap::new(),
            seen_dirty: false,
            last_seen_persist_at: Instant::now(),
            last_account_sync_at: Instant::now() - Duration::from_secs(3600),
        }
    }

    fn mark_seen_dirty(&mut self) {
        self.seen_dirty = true;
    }
}

struct TradingEngine<'a, S> {
    config: &'a Config,
    clob_auth: &'a Option<(
        S,
        clob::Client<
            polymarket_client_sdk::auth::state::Authenticated<polymarket_client_sdk::auth::Normal>,
        >,
    )>,
    shared_state: &'a SharedState,
    data_client: &'a DataClient,
    persistence: &'a PersistenceStore,
}

fn bps_to_fraction(bps: f64) -> f64 {
    bps / 10_000.0
}

async fn record_skip_reason(shared_state: &SharedState, reason: &'static str) {
    shared_state.write().await.record_skip(reason);
}

fn trade_key(trade: &DataTrade) -> String {
    format!(
        "{}-{}-{}",
        trade.transaction_hash, trade.asset, trade.timestamp
    )
}

async fn fetch_recent_unseen_trades(
    data_client: &DataClient,
    addr: Address,
    whale_seen: &HashSet<String>,
) -> Result<Vec<DataTrade>> {
    let mut unseen = Vec::new();
    let mut offset = 0_i32;

    for _ in 0..MAX_TRADE_PAGES {
        let page = data_client
            .trades(
                &DataTradesRequest::builder()
                    .user(addr)
                    .limit(TRADE_PAGE_LIMIT)
                    .expect("TRADE_PAGE_LIMIT is within valid range")
                    .offset(offset)
                    .expect("offset is within valid range")
                    .build(),
            )
            .await?;

        if page.is_empty() {
            break;
        }

        let page_len = page.len() as i32;
        let mut found_seen = false;
        for trade in page {
            if whale_seen.contains(&trade_key(&trade)) {
                found_seen = true;
            } else {
                unseen.push(trade);
            }
        }

        if found_seen || page_len < TRADE_PAGE_LIMIT {
            break;
        }
        offset += TRADE_PAGE_LIMIT;
    }

    Ok(unseen)
}

async fn fetch_all_open_positions(
    data_client: &DataClient,
    user: Address,
) -> Result<Vec<Position>> {
    let mut offset = 0_i32;
    let mut out = Vec::new();
    loop {
        let page = data_client
            .positions(
                &PositionsRequest::builder()
                    .user(user)
                    .limit(ACCOUNT_SYNC_PAGE_LIMIT)?
                    .offset(offset)?
                    .build(),
            )
            .await?;
        if page.is_empty() {
            break;
        }
        let page_len = page.len() as i32;
        out.extend(page);
        if page_len < ACCOUNT_SYNC_PAGE_LIMIT || offset >= 10_000 - ACCOUNT_SYNC_PAGE_LIMIT {
            break;
        }
        offset += ACCOUNT_SYNC_PAGE_LIMIT;
    }
    Ok(out)
}

async fn fetch_all_closed_positions(
    data_client: &DataClient,
    user: Address,
) -> Result<Vec<ClosedPosition>> {
    let mut offset = 0_i32;
    let mut out = Vec::new();
    loop {
        let page = data_client
            .closed_positions(
                &ClosedPositionsRequest::builder()
                    .user(user)
                    .limit(ACCOUNT_SYNC_PAGE_LIMIT)?
                    .offset(offset)?
                    .build(),
            )
            .await?;
        if page.is_empty() {
            break;
        }
        let page_len = page.len() as i32;
        out.extend(page);
        if page_len < ACCOUNT_SYNC_PAGE_LIMIT || offset >= 10_000 - ACCOUNT_SYNC_PAGE_LIMIT {
            break;
        }
        offset += ACCOUNT_SYNC_PAGE_LIMIT;
    }
    Ok(out)
}

/// Spawn the trading task.
pub fn spawn(
    config: Config,
    whale_list: WhaleList,
    shared_state: SharedState,
    persistence: Arc<PersistenceStore>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Err(e) =
                run_trading_loop(&config, &whale_list, &shared_state, &persistence).await
            {
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
    persistence: &PersistenceStore,
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

    // ── Restore seen-trade dedupe cache ───────────────────────────────────
    let restored_seen = match persistence.load_seen_trades() {
        Ok(v) => v,
        Err(e) => {
            warn!("Failed loading seen-trades cache: {e:#}");
            HashMap::new()
        }
    };
    let mut runtime = TradingRuntime::new(restored_seen);

    // ── Reuse a single Data API client across all polls ──────────────────
    let data_client = DataClient::default();
    let account_address = Address::from_str(&config.address).ok();

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
            tokio::time::sleep(tokio::time::Duration::from_secs(config.poll_interval_secs)).await;
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

        let poll_duration = tokio::time::Duration::from_secs(config.poll_interval_secs);
        let mut poll_interval = tokio::time::interval(poll_duration);
        poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let whale_snapshot: Vec<String> = whales.iter().map(|w| w.address.clone()).collect();
        let mut last_check_at = Instant::now() - Duration::from_millis(WS_CHECK_DEBOUNCE_MS);

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

            let engine = TradingEngine {
                config,
                clob_auth: &clob_auth,
                shared_state,
                data_client: &data_client,
                persistence,
            };

            if !config.dry_run
                && runtime.last_account_sync_at.elapsed()
                    >= Duration::from_secs(config.account_sync_interval_secs)
            {
                if let Some(addr) = account_address {
                    if let Err(e) = sync_live_account_state(&engine, addr).await {
                        warn!("Live account sync failed: {e:#}");
                    } else {
                        runtime.last_account_sync_at = Instant::now();
                    }
                }
            }

            tokio::select! {
                maybe_event = stream.next() => {
                    match maybe_event {
                        Some(Ok(book)) => {
                            debug!(asset = %book.asset_id, "Orderbook update received");
                            if last_check_at.elapsed() < Duration::from_millis(WS_CHECK_DEBOUNCE_MS) {
                                debug!(asset = %book.asset_id, "Skipping check due to debounce");
                                continue;
                            }
                            last_check_at = Instant::now();
                            let whales_snap = whale_list.read().await.clone();
                            check_and_copy(&engine, &whales_snap, &mut runtime, Some(book.asset_id.to_string())).await;
                            maybe_persist_seen_trades(&engine, &mut runtime);
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
                    last_check_at = Instant::now();
                    let whales_snap = whale_list.read().await.clone();
                    check_and_copy(&engine, &whales_snap, &mut runtime, None).await;
                    maybe_persist_seen_trades(&engine, &mut runtime);
                }
            }
        }
    }
}

async fn sync_live_account_state<S>(engine: &TradingEngine<'_, S>, user: Address) -> Result<()>
where
    S: polymarket_client_sdk::auth::Signer + Sync,
{
    let open_positions = fetch_all_open_positions(engine.data_client, user).await?;
    let closed_positions = fetch_all_closed_positions(engine.data_client, user).await?;

    let mut positions_by_market: HashMap<String, MarketPosition> = HashMap::new();
    let mut yes_prices: HashMap<String, f64> = HashMap::new();
    let mut open_mark_value = 0.0_f64;
    let mut open_unrealized = 0.0_f64;

    for p in open_positions {
        let market = p.condition_id.to_string();
        let shares = p.size.to_f64().unwrap_or(0.0).max(0.0);
        if shares <= 0.0 {
            continue;
        }
        let avg_price = p.avg_price.to_f64().unwrap_or(0.0).clamp(0.0001, 0.9999);
        let cur_price = p
            .cur_price
            .to_f64()
            .unwrap_or(avg_price)
            .clamp(0.0001, 0.9999);
        open_mark_value += shares * cur_price;
        open_unrealized += p.cash_pnl.to_f64().unwrap_or(0.0);

        let pos = positions_by_market.entry(market.clone()).or_default();
        if p.outcome.eq_ignore_ascii_case("yes") {
            pos.yes_shares += shares;
            pos.yes_cost_usdc += shares * avg_price;
            yes_prices.insert(market, cur_price);
        } else {
            pos.no_shares += shares;
            pos.no_cost_usdc += shares * avg_price;
            yes_prices.insert(market, (1.0 - cur_price).clamp(0.0001, 0.9999));
        }
    }

    let total_realized_all_time = closed_positions
        .iter()
        .map(|p| p.realized_pnl.to_f64().unwrap_or(0.0))
        .sum::<f64>();

    engine
        .shared_state
        .write()
        .await
        .apply_live_account_snapshot(
            positions_by_market,
            yes_prices,
            open_mark_value,
            open_unrealized,
            total_realized_all_time,
        );
    Ok(())
}

fn maybe_persist_seen_trades<S>(engine: &TradingEngine<'_, S>, runtime: &mut TradingRuntime)
where
    S: polymarket_client_sdk::auth::Signer + Sync,
{
    if !runtime.seen_dirty {
        return;
    }
    if runtime.last_seen_persist_at.elapsed() < Duration::from_secs(SEEN_TRADES_FLUSH_SECS) {
        return;
    }
    if let Err(e) = engine.persistence.save_seen_trades(&runtime.seen_trades) {
        warn!("Failed persisting seen-trades cache: {e:#}");
        return;
    }
    runtime.seen_dirty = false;
    runtime.last_seen_persist_at = Instant::now();
}

/// Query each whale's recent trades via the Data API, detect new ones, and
/// either place a real order (live mode) or simulate it (dry-run mode).
async fn check_and_copy<S>(
    engine: &TradingEngine<'_, S>,
    whales: &[TraderProfile],
    runtime: &mut TradingRuntime,
    triggered_asset_id: Option<String>,
) where
    S: polymarket_client_sdk::auth::Signer + Sync,
{
    // ── Account-level risk circuit breakers ───────────────────────────────
    {
        let state = engine.shared_state.read().await;
        if state.balance_usdc < MIN_BALANCE_USDC {
            debug!(
                balance = state.balance_usdc,
                "Balance below minimum threshold (${MIN_BALANCE_USDC}) — skipping trade cycle"
            );
            record_skip_reason(engine.shared_state, "balance_below_minimum").await;
            return;
        }
        if state.daily_drawdown_usdc() >= engine.config.daily_loss_limit_usdc {
            warn!(
                drawdown = format_args!("{:.2}", state.daily_drawdown_usdc()),
                limit = format_args!("{:.2}", engine.config.daily_loss_limit_usdc),
                "Daily loss limit reached — skipping trade cycle"
            );
            record_skip_reason(engine.shared_state, "daily_loss_limit_reached").await;
            return;
        }
        if state.consecutive_loss_events >= engine.config.max_consecutive_losses {
            warn!(
                streak = state.consecutive_loss_events,
                max = engine.config.max_consecutive_losses,
                "Consecutive-loss circuit breaker triggered — skipping trade cycle"
            );
            record_skip_reason(engine.shared_state, "max_consecutive_losses_reached").await;
            return;
        }
    }

    let now = Utc::now();
    let active_whales: HashSet<String> = whales.iter().map(|w| w.address.clone()).collect();
    runtime
        .seen_trades
        .retain(|addr, _| active_whales.contains(addr));
    runtime
        .market_exposure
        .retain(|_, e| (now - e.updated_at).num_seconds() <= MARKET_EXPOSURE_TTL_SECS);

    for whale in whales {
        if let Some(asset_id) = triggered_asset_id.as_ref() {
            if !whale.active_asset_ids.iter().any(|a| a == asset_id) {
                continue;
            }
        }

        // Skip whales with stale profiles — if the investigation cycle has
        // not refreshed data in MAX_WHALE_STALENESS_SECS, the win_rate and
        // score may be outdated and unreliable.
        let age_secs = (now - whale.updated_at).num_seconds();
        if age_secs > MAX_WHALE_STALENESS_SECS {
            warn!(
                whale = %whale.address,
                age_hours = age_secs / 3600,
                "Whale profile is stale — skipping until investigation refreshes"
            );
            record_skip_reason(engine.shared_state, "stale_whale_profile").await;
            continue;
        }

        let addr: Address = match Address::from_str(&whale.address) {
            Ok(a) => a,
            Err(e) => {
                warn!(address = %whale.address, "Cannot parse address: {e}");
                record_skip_reason(engine.shared_state, "invalid_whale_address").await;
                continue;
            }
        };

        // Process all new trades (not just the newest) to avoid missing
        // intermediate trades between polls.
        let whale_seen = runtime
            .seen_trades
            .entry(whale.address.clone())
            .or_default();
        let mut seen_changed = false;
        let trades = match fetch_recent_unseen_trades(engine.data_client, addr, whale_seen).await {
            Ok(t) => t,
            Err(e) => {
                debug!(address = %whale.address, "Data API error: {e}");
                record_skip_reason(engine.shared_state, "data_api_error").await;
                continue;
            }
        };
        if trades.is_empty() {
            continue;
        }

        // On first observation of a whale, prime dedupe state with current
        // recent trades and skip copying them. This avoids backfilling stale
        // historical trades on startup/restarts.
        if whale_seen.is_empty() {
            for trade in &trades {
                whale_seen.insert(trade_key(trade));
            }
            seen_changed = true;
            info!(
                whale = %whale.address,
                primed_trades = trades.len(),
                "Primed seen-trade cache for whale; skipping historical backfill"
            );
            record_skip_reason(engine.shared_state, "primed_historical_backfill").await;
            if seen_changed {
                runtime.mark_seen_dirty();
            }
            continue;
        }

        let mut processed_trade_keys = Vec::new();

        // Trades are returned newest-first; process oldest-first for a more
        // stable bankroll/exposure progression when multiple trades are copied.
        for trade in trades.into_iter().rev() {
            let trade_id = trade_key(&trade);
            whale_seen.insert(trade_id.clone());
            processed_trade_keys.push(trade_id.clone());
            seen_changed = true;

            info!(
                whale = %whale.address,
                trade_key = %trade_id,
                side = ?trade.side,
                price = %trade.price,
                size = %trade.size,
                "New whale trade detected — evaluating copy"
            );

            let market_price = match trade.price.to_f64() {
                Some(p) if p > 0.0 && p < 1.0 => p,
                _ => {
                    warn!(trade_key = %trade_id, "Unusual price; skipping");
                    record_skip_reason(engine.shared_state, "invalid_market_price").await;
                    continue;
                }
            };

            // ── Adjust price for Sell (NO) trades ───────────────────────
            // When a whale sells YES tokens, they're effectively betting NO.
            // The Kelly formula needs the effective price from the whale's
            // perspective: for a NO bet, effective_price = 1 - market_price.
            let is_sell = matches!(trade.side, polymarket_client_sdk::data::types::Side::Sell);
            let outcome = if is_sell { "NO" } else { "YES" };
            let effective_price = if is_sell {
                1.0 - market_price
            } else {
                market_price
            };

            // ── Use current balance as bankroll, not the static config ──
            let current_balance = engine.shared_state.read().await.balance_usdc;
            if current_balance < MIN_BALANCE_USDC {
                info!(
                    balance = current_balance,
                    "Remaining balance below ${MIN_BALANCE_USDC} — stopping trades"
                );
                record_skip_reason(engine.shared_state, "balance_below_minimum").await;
                return;
            }

            // Conservative edge estimation: use Bayesian lower-bound win rate.
            let p_est = whale.conservative_win_rate;
            if !(0.0..=1.0).contains(&p_est) {
                warn!(
                    whale = %whale.address,
                    p_est = p_est,
                    "Invalid conservative win-rate estimate; skipping"
                );
                record_skip_reason(engine.shared_state, "invalid_conservative_win_rate").await;
                continue;
            }
            let gross_edge = p_est - effective_price;
            let estimated_cost = bps_to_fraction(
                engine.config.estimated_fee_bps
                    + engine.config.estimated_slippage_bps
                    + engine.config.adverse_selection_bps,
            );
            let min_net_edge = bps_to_fraction(engine.config.min_net_edge_bps);
            let net_edge = gross_edge - estimated_cost;
            if net_edge <= min_net_edge {
                info!(
                    whale = %whale.address,
                    gross_edge_bps = format_args!("{:.1}", gross_edge * 10_000.0),
                    net_edge_bps = format_args!("{:.1}", net_edge * 10_000.0),
                    min_required_bps = format_args!("{:.1}", engine.config.min_net_edge_bps),
                    "Net edge below threshold — trade skipped"
                );
                record_skip_reason(engine.shared_state, "net_edge_below_threshold").await;
                continue;
            }

            let kelly_result = kelly::kelly(
                effective_price,
                p_est,
                current_balance,
                engine.config.kelly_fraction,
                engine.config.max_position_fraction,
            );

            let kr = match kelly_result {
                Some(k) => k,
                None => {
                    info!(whale = %whale.address, "No edge detected — trade skipped");
                    record_skip_reason(engine.shared_state, "kelly_no_edge").await;
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
                record_skip_reason(engine.shared_state, "position_below_minimum").await;
                continue;
            }

            // ── Check per-market concentration ──────────────────────────
            let market_id = trade.condition_id.to_string();
            let current_market_exp = runtime
                .market_exposure
                .get(&market_id)
                .map(|e| e.amount_usdc)
                .unwrap_or(0.0);
            let max_market_usdc = engine.config.bankroll_usdc * MAX_MARKET_CONCENTRATION;
            if current_market_exp + kr.position_size_usdc > max_market_usdc {
                let remaining = (max_market_usdc - current_market_exp).max(0.0);
                if remaining < MIN_POSITION_USDC {
                    info!(
                        whale = %whale.address,
                        market = %market_id,
                        exposure = format_args!("{:.2}", current_market_exp),
                        "Market concentration limit reached — skipping"
                    );
                    record_skip_reason(engine.shared_state, "market_concentration_limit").await;
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
            let mut final_size = kr
                .position_size_usdc
                .min(max_market_usdc - current_market_exp)
                .max(0.0);

            // ── Absolute per-market notional breaker ────────────────────
            let current_notional = engine
                .shared_state
                .read()
                .await
                .market_notional_usdc(&market_id);
            if current_notional + final_size > engine.config.max_notional_per_market_usdc {
                let remaining =
                    (engine.config.max_notional_per_market_usdc - current_notional).max(0.0);
                if remaining < MIN_POSITION_USDC {
                    info!(
                        market = %market_id,
                        notional = format_args!("{:.2}", current_notional),
                        limit = format_args!("{:.2}", engine.config.max_notional_per_market_usdc),
                        "Per-market notional breaker reached — skipping"
                    );
                    record_skip_reason(engine.shared_state, "max_notional_per_market_reached")
                        .await;
                    continue;
                }
                final_size = final_size.min(remaining);
            }

            if final_size < MIN_POSITION_USDC {
                record_skip_reason(engine.shared_state, "position_below_minimum").await;
                continue;
            }

            let side_str = match trade.side {
                polymarket_client_sdk::data::types::Side::Buy => "Buy",
                polymarket_client_sdk::data::types::Side::Sell => "Sell",
                _ => {
                    warn!(trade_key = %trade_id, "Unknown side; skipping");
                    record_skip_reason(engine.shared_state, "unknown_trade_side").await;
                    continue;
                }
            };

            let shares = (final_size / effective_price).max(0.0);
            if shares < MIN_SHARES {
                record_skip_reason(engine.shared_state, "shares_below_minimum").await;
                continue;
            }

            info!(
                whale = %whale.address,
                kelly_fraction = format_args!("{:.4}", kr.adjusted_fraction),
                position_usdc = format_args!("{:.2}", final_size),
                net_edge_bps = format_args!("{:.1}", net_edge * 10_000.0),
                dry_run = engine.config.dry_run,
                "{}",
                if engine.config.dry_run { "Simulating copy trade" } else { "Placing copy order" }
            );

            if engine.config.dry_run {
                let record = TradeRecord {
                    timestamp: Utc::now(),
                    whale_address: whale.address.clone(),
                    market: market_id.clone(),
                    side: side_str.to_string(),
                    price: market_price,
                    outcome: outcome.to_string(),
                    effective_price,
                    shares,
                    size_usdc: final_size,
                    kelly_fraction: kr.adjusted_fraction,
                    simulated: true,
                };
                engine.shared_state.write().await.record_trade(record);
                runtime.market_exposure.insert(
                    market_id,
                    MarketExposure {
                        amount_usdc: current_market_exp + final_size,
                        updated_at: Utc::now(),
                    },
                );
                info!(
                    position_usdc = format_args!("{:.2}", final_size),
                    "[DRY RUN] Trade simulated — no real order placed"
                );
                continue;
            }

            // ── Live order placement ────────────────────────────────────
            let (signer, clob_client) = match engine.clob_auth {
                Some(pair) => pair,
                None => {
                    error!("clob_auth is None in live mode — this is a bug");
                    record_skip_reason(engine.shared_state, "missing_live_auth").await;
                    continue;
                }
            };

            let token_id = match U256::from_str(&trade.asset.to_string()) {
                Ok(id) => id,
                Err(e) => {
                    warn!(trade_key = %trade_id, "Cannot parse asset ID: {e}; skipping");
                    record_skip_reason(engine.shared_state, "invalid_asset_id").await;
                    continue;
                }
            };

            let side = match trade.side {
                polymarket_client_sdk::data::types::Side::Buy => Side::Buy,
                polymarket_client_sdk::data::types::Side::Sell => Side::Sell,
                _ => {
                    record_skip_reason(engine.shared_state, "unknown_trade_side").await;
                    continue;
                }
            };

            // Price guardrails: reject large drift from whale print and set a
            // protective execution price on the market-order builder.
            let quote = match clob_client
                .price(
                    &ClobPriceRequest::builder()
                        .token_id(token_id)
                        .side(side)
                        .build(),
                )
                .await
            {
                Ok(q) => q,
                Err(e) => {
                    warn!(trade_key = %trade_id, "Failed to fetch live quote: {e}; skipping");
                    record_skip_reason(engine.shared_state, "live_quote_fetch_failed").await;
                    continue;
                }
            };
            let quote_price = match quote.price.to_f64() {
                Some(p) if p > 0.0 && p < 1.0 => p,
                _ => {
                    warn!(trade_key = %trade_id, "Invalid live quote; skipping");
                    record_skip_reason(engine.shared_state, "invalid_live_quote").await;
                    continue;
                }
            };
            let drift_bps = ((quote_price - market_price).abs() / market_price) * 10_000.0;
            if drift_bps > engine.config.max_copy_price_drift_bps {
                info!(
                    trade_key = %trade_id,
                    whale_price = format_args!("{:.4}", market_price),
                    live_quote = format_args!("{:.4}", quote_price),
                    drift_bps = format_args!("{:.1}", drift_bps),
                    max_drift_bps = format_args!("{:.1}", engine.config.max_copy_price_drift_bps),
                    "Price drift too large — trade skipped"
                );
                record_skip_reason(engine.shared_state, "price_drift_too_large").await;
                continue;
            }

            let slip = bps_to_fraction(engine.config.max_order_slippage_bps);
            let protected_price = if side == Side::Buy {
                (quote_price * (1.0 + slip)).min(0.9999)
            } else {
                (quote_price * (1.0 - slip)).max(0.0001)
            };
            let protected_price_decimal = match Decimal::try_from(protected_price) {
                Ok(p) => p,
                Err(e) => {
                    warn!(trade_key = %trade_id, "Cannot convert protected price to Decimal: {e}; skipping");
                    record_skip_reason(engine.shared_state, "protected_price_conversion_failed")
                        .await;
                    continue;
                }
            };

            let amount = if side == Side::Buy {
                let usdc_amount = match Decimal::try_from(final_size) {
                    Ok(d) => d,
                    Err(e) => {
                        warn!(trade_key = %trade_id, "Cannot convert position size to Decimal: {e}; skipping");
                        record_skip_reason(
                            engine.shared_state,
                            "position_amount_conversion_failed",
                        )
                        .await;
                        continue;
                    }
                };
                match Amount::usdc(usdc_amount) {
                    Ok(a) => a,
                    Err(e) => {
                        warn!("Invalid position amount: {e}");
                        record_skip_reason(engine.shared_state, "invalid_position_amount").await;
                        continue;
                    }
                }
            } else {
                let sell_shares = (final_size / market_price).max(0.0);
                if sell_shares < MIN_SHARES {
                    info!(
                        trade_key = %trade_id,
                        shares = format_args!("{:.4}", sell_shares),
                        "Sell-side converted shares below minimum — skipping"
                    );
                    record_skip_reason(engine.shared_state, "sell_shares_below_minimum").await;
                    continue;
                }
                let shares_decimal = match Decimal::try_from(sell_shares) {
                    Ok(d) => d.trunc_with_scale(2),
                    Err(e) => {
                        warn!(trade_key = %trade_id, "Cannot convert share size to Decimal: {e}; skipping");
                        record_skip_reason(engine.shared_state, "share_amount_conversion_failed")
                            .await;
                        continue;
                    }
                };
                match Amount::shares(shares_decimal) {
                    Ok(a) => a,
                    Err(e) => {
                        warn!(trade_key = %trade_id, "Invalid share amount: {e}");
                        record_skip_reason(engine.shared_state, "invalid_share_amount").await;
                        continue;
                    }
                }
            };

            let order = match clob_client
                .market_order()
                .token_id(token_id)
                .amount(amount)
                .side(side)
                .price(protected_price_decimal)
                .build()
                .await
            {
                Ok(o) => o,
                Err(e) => {
                    error!("Failed to build market order: {e}");
                    record_skip_reason(engine.shared_state, "order_build_failed").await;
                    continue;
                }
            };

            let signed = match clob_client.sign(signer, order).await {
                Ok(s) => s,
                Err(e) => {
                    error!("Failed to sign order: {e}");
                    record_skip_reason(engine.shared_state, "order_sign_failed").await;
                    continue;
                }
            };

            match clob_client.post_order(signed).await {
                Ok(r) => {
                    info!(order_id = %r.order_id, success = r.success, "Copy order placed");
                    // In live mode only mutate balances/exposure after successful post.
                    let record = TradeRecord {
                        timestamp: Utc::now(),
                        whale_address: whale.address.clone(),
                        market: market_id.clone(),
                        side: side_str.to_string(),
                        price: market_price,
                        outcome: outcome.to_string(),
                        effective_price,
                        shares,
                        size_usdc: final_size,
                        kelly_fraction: kr.adjusted_fraction,
                        simulated: false,
                    };
                    engine.shared_state.write().await.record_trade(record);
                    runtime.market_exposure.insert(
                        market_id,
                        MarketExposure {
                            amount_usdc: current_market_exp + final_size,
                            updated_at: Utc::now(),
                        },
                    );
                }
                Err(e) => {
                    error!("Failed to post order: {e}");
                    record_skip_reason(engine.shared_state, "order_post_failed").await;
                }
            }
        }

        // Cap the seen-trades set per whale to prevent unbounded memory growth.
        if whale_seen.len() > MAX_SEEN_TRADE_KEYS_PER_WHALE {
            whale_seen.clear();
            // After clearing, re-insert keys from the current batch so we
            // don't re-process these trades on the next poll.
            for key in processed_trade_keys {
                whale_seen.insert(key);
            }
            seen_changed = true;
        }
        if seen_changed {
            runtime.mark_seen_dirty();
        }
    }
}
