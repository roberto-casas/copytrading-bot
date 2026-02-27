//! Shared bot state accessible by the trading thread, investigation thread,
//! and the web dashboard.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// Maximum number of recent trades stored in memory.
const MAX_RECENT_TRADES: usize = 200;
/// Maximum number of skip events retained for rolling-window analytics.
const MAX_SKIP_EVENTS: usize = 5_000;

/// A single trade record (simulated in dry-run mode, real in live mode).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeRecord {
    /// UTC timestamp when the trade was (or would have been) executed.
    pub timestamp: DateTime<Utc>,
    /// Whale wallet whose activity triggered this copy trade.
    pub whale_address: String,
    /// Market condition ID.
    pub market: String,
    /// Original whale side on YES token: "Buy" or "Sell".
    pub side: String,
    /// Whale YES-token execution price (0..1).
    pub price: f64,
    /// Effective copied outcome side: "YES" or "NO".
    pub outcome: String,
    /// Effective copied outcome price (YES price for YES bets, 1-YES for NO bets).
    pub effective_price: f64,
    /// Number of shares acquired for the copied outcome.
    pub shares: f64,
    /// USDC notional used for the copy position.
    pub size_usdc: f64,
    /// Adjusted Kelly fraction used to size the position.
    pub kelly_fraction: f64,
    /// `true` when in dry-run mode (no real order was placed).
    pub simulated: bool,
}

/// A skipped-signal telemetry event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkipEvent {
    pub timestamp: DateTime<Utc>,
    pub reason: String,
}

/// Aggregated per-market YES/NO exposures.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MarketPosition {
    pub yes_shares: f64,
    pub yes_cost_usdc: f64,
    pub no_shares: f64,
    pub no_cost_usdc: f64,
}

/// Persisted runtime snapshot of [`BotState`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotStateSnapshot {
    pub dry_run: bool,
    pub balance_usdc: f64,
    pub initial_balance_usdc: f64,
    pub started_at: DateTime<Utc>,
    pub whale_count: usize,
    pub trade_count: usize,
    pub recent_trades: VecDeque<TradeRecord>,
    pub skip_reasons: HashMap<String, usize>,
    pub recent_skip_events: VecDeque<SkipEvent>,
    pub market_positions: HashMap<String, MarketPosition>,
    pub last_yes_prices: HashMap<String, f64>,
    pub unrealized_pnl_usdc: f64,
    pub realized_pnl_usdc: f64,
    pub realized_pnl_baseline_usdc: Option<f64>,
    pub equity_usdc: f64,
    pub day_marker_utc: String,
    pub day_start_equity_usdc: f64,
    pub consecutive_loss_events: usize,
    pub alert_success_count: usize,
    pub alert_failure_count: usize,
    pub last_alert_sent_at: Option<DateTime<Utc>>,
    pub last_alert_error: Option<String>,
}

/// Top-level shared bot state.
#[derive(Debug, Serialize)]
pub struct BotState {
    /// Whether the bot is running in dry-run (paper trading) mode.
    pub dry_run: bool,
    /// Free USDC cash balance.
    pub balance_usdc: f64,
    /// Balance at startup (used to compute total P&L).
    pub initial_balance_usdc: f64,
    /// UTC timestamp when the bot started.
    #[serde(with = "chrono::serde::ts_seconds")]
    pub started_at: DateTime<Utc>,
    /// Number of whales currently being tracked.
    pub whale_count: usize,
    /// Total number of trades recorded since startup.
    pub trade_count: usize,
    /// Ring buffer of the most recent trades.
    pub recent_trades: VecDeque<TradeRecord>,
    /// Calibration counters: why potential trades were skipped.
    pub skip_reasons: HashMap<String, usize>,
    /// Recent skipped-signal events for rolling-window counters.
    #[serde(skip_serializing)]
    recent_skip_events: VecDeque<SkipEvent>,
    /// Per-market YES/NO exposure book.
    pub market_positions: HashMap<String, MarketPosition>,
    /// Latest observed YES price per market for mark-to-market valuation.
    pub last_yes_prices: HashMap<String, f64>,
    /// Mark-to-market unrealized P&L from open inventory.
    pub unrealized_pnl_usdc: f64,
    /// Realized P&L (closed/redeemed positions) since bot start.
    pub realized_pnl_usdc: f64,
    /// Baseline of cumulative realized P&L at bot boot time.
    pub realized_pnl_baseline_usdc: Option<f64>,
    /// Cash + marked position value.
    pub equity_usdc: f64,
    /// Current UTC day marker (`YYYY-MM-DD`) for drawdown resets.
    pub day_marker_utc: String,
    /// Equity snapshot at UTC day start.
    pub day_start_equity_usdc: f64,
    /// Number of consecutive loss events based on total P&L deltas.
    pub consecutive_loss_events: usize,
    /// Alert delivery stats.
    pub alert_success_count: usize,
    pub alert_failure_count: usize,
    pub last_alert_sent_at: Option<DateTime<Utc>>,
    pub last_alert_error: Option<String>,
}

impl BotState {
    /// Create a new `BotState`.
    pub fn new(dry_run: bool, initial_balance_usdc: f64) -> Self {
        let today = Utc::now().date_naive().to_string();
        Self {
            dry_run,
            balance_usdc: initial_balance_usdc,
            initial_balance_usdc,
            started_at: Utc::now(),
            whale_count: 0,
            trade_count: 0,
            recent_trades: VecDeque::new(),
            skip_reasons: HashMap::new(),
            recent_skip_events: VecDeque::new(),
            market_positions: HashMap::new(),
            last_yes_prices: HashMap::new(),
            unrealized_pnl_usdc: 0.0,
            realized_pnl_usdc: 0.0,
            realized_pnl_baseline_usdc: None,
            equity_usdc: initial_balance_usdc,
            day_marker_utc: today,
            day_start_equity_usdc: initial_balance_usdc,
            consecutive_loss_events: 0,
            alert_success_count: 0,
            alert_failure_count: 0,
            last_alert_sent_at: None,
            last_alert_error: None,
        }
    }

    /// Restore internal state from a persisted snapshot.
    pub fn restore_from_snapshot(&mut self, snapshot: BotStateSnapshot) {
        self.dry_run = snapshot.dry_run;
        self.balance_usdc = snapshot.balance_usdc;
        self.initial_balance_usdc = snapshot.initial_balance_usdc;
        self.started_at = snapshot.started_at;
        self.whale_count = snapshot.whale_count;
        self.trade_count = snapshot.trade_count;
        self.recent_trades = snapshot.recent_trades;
        self.skip_reasons = snapshot.skip_reasons;
        self.recent_skip_events = snapshot.recent_skip_events;
        self.market_positions = snapshot.market_positions;
        self.last_yes_prices = snapshot.last_yes_prices;
        self.unrealized_pnl_usdc = snapshot.unrealized_pnl_usdc;
        self.realized_pnl_usdc = snapshot.realized_pnl_usdc;
        self.realized_pnl_baseline_usdc = snapshot.realized_pnl_baseline_usdc;
        self.equity_usdc = snapshot.equity_usdc;
        self.day_marker_utc = snapshot.day_marker_utc;
        self.day_start_equity_usdc = snapshot.day_start_equity_usdc;
        self.consecutive_loss_events = snapshot.consecutive_loss_events;
        self.alert_success_count = snapshot.alert_success_count;
        self.alert_failure_count = snapshot.alert_failure_count;
        self.last_alert_sent_at = snapshot.last_alert_sent_at;
        self.last_alert_error = snapshot.last_alert_error;
    }

    /// Create a persistable snapshot of the current state.
    pub fn snapshot(&self) -> BotStateSnapshot {
        BotStateSnapshot {
            dry_run: self.dry_run,
            balance_usdc: self.balance_usdc,
            initial_balance_usdc: self.initial_balance_usdc,
            started_at: self.started_at,
            whale_count: self.whale_count,
            trade_count: self.trade_count,
            recent_trades: self.recent_trades.clone(),
            skip_reasons: self.skip_reasons.clone(),
            recent_skip_events: self.recent_skip_events.clone(),
            market_positions: self.market_positions.clone(),
            last_yes_prices: self.last_yes_prices.clone(),
            unrealized_pnl_usdc: self.unrealized_pnl_usdc,
            realized_pnl_usdc: self.realized_pnl_usdc,
            realized_pnl_baseline_usdc: self.realized_pnl_baseline_usdc,
            equity_usdc: self.equity_usdc,
            day_marker_utc: self.day_marker_utc.clone(),
            day_start_equity_usdc: self.day_start_equity_usdc,
            consecutive_loss_events: self.consecutive_loss_events,
            alert_success_count: self.alert_success_count,
            alert_failure_count: self.alert_failure_count,
            last_alert_sent_at: self.last_alert_sent_at,
            last_alert_error: self.last_alert_error.clone(),
        }
    }

    /// Append a trade record and update balance/exposure/equity.
    pub fn record_trade(&mut self, trade: TradeRecord) {
        let prev_total_pnl = self.total_pnl();

        self.balance_usdc -= trade.size_usdc;
        self.balance_usdc = self.balance_usdc.max(0.0);

        if (0.0..1.0).contains(&trade.price) {
            self.last_yes_prices
                .insert(trade.market.clone(), trade.price);
        }

        let pos = self
            .market_positions
            .entry(trade.market.clone())
            .or_default();
        match trade.outcome.as_str() {
            "YES" => {
                pos.yes_shares += trade.shares;
                pos.yes_cost_usdc += trade.size_usdc;
            }
            "NO" => {
                pos.no_shares += trade.shares;
                pos.no_cost_usdc += trade.size_usdc;
            }
            _ => {}
        }

        self.trade_count += 1;
        self.recent_trades.push_front(trade);
        self.recent_trades.truncate(MAX_RECENT_TRADES);

        self.recompute_mark_to_market();
        self.refresh_day_marker_if_needed();

        let current_total_pnl = self.total_pnl();
        if current_total_pnl + 1e-9 < prev_total_pnl {
            self.consecutive_loss_events += 1;
        } else {
            self.consecutive_loss_events = 0;
        }
    }

    /// Increment a skip-reason counter for calibration.
    pub fn record_skip(&mut self, reason: &str) {
        self.record_skip_at(reason, Utc::now());
    }

    fn record_skip_at(&mut self, reason: &str, timestamp: DateTime<Utc>) {
        *self.skip_reasons.entry(reason.to_string()).or_insert(0) += 1;
        self.recent_skip_events.push_front(SkipEvent {
            timestamp,
            reason: reason.to_string(),
        });
        self.recent_skip_events.truncate(MAX_SKIP_EVENTS);
    }

    /// Aggregate skip reasons within the given trailing window in seconds.
    pub fn skip_reasons_since(&self, window_secs: i64) -> HashMap<String, usize> {
        if window_secs <= 0 {
            return HashMap::new();
        }
        let cutoff = Utc::now() - chrono::Duration::seconds(window_secs);
        let mut counts = HashMap::new();
        for event in &self.recent_skip_events {
            if event.timestamp >= cutoff {
                *counts.entry(event.reason.clone()).or_insert(0) += 1;
            }
        }
        counts
    }

    /// Record webhook alert delivery outcome for reliability telemetry.
    pub fn record_alert_result(&mut self, ok: bool, error: Option<String>) {
        if ok {
            self.alert_success_count += 1;
            self.last_alert_sent_at = Some(Utc::now());
            self.last_alert_error = None;
        } else {
            self.alert_failure_count += 1;
            self.last_alert_error = error;
        }
    }

    /// Current intraday drawdown (UTC day boundary).
    pub fn daily_drawdown_usdc(&self) -> f64 {
        (self.day_start_equity_usdc - self.equity_usdc).max(0.0)
    }

    /// Current total notional deployed in a market (YES + NO cost basis).
    pub fn market_notional_usdc(&self, market: &str) -> f64 {
        self.market_positions
            .get(market)
            .map(|p| p.yes_cost_usdc + p.no_cost_usdc)
            .unwrap_or(0.0)
    }

    /// Maximum deployed notional across all markets.
    pub fn max_market_notional_usdc(&self) -> f64 {
        self.market_positions
            .values()
            .map(|p| p.yes_cost_usdc + p.no_cost_usdc)
            .fold(0.0_f64, f64::max)
    }

    /// Replace live account inventory/marks from external account sync.
    pub fn apply_live_account_snapshot(
        &mut self,
        market_positions: HashMap<String, MarketPosition>,
        last_yes_prices: HashMap<String, f64>,
        open_mark_value_usdc: f64,
        open_unrealized_pnl_usdc: f64,
        total_realized_all_time_usdc: f64,
    ) {
        let prev_total_pnl = self.total_pnl();

        let baseline = self
            .realized_pnl_baseline_usdc
            .get_or_insert(total_realized_all_time_usdc);
        self.realized_pnl_usdc = total_realized_all_time_usdc - *baseline;

        self.market_positions = market_positions;
        self.last_yes_prices = last_yes_prices;
        self.unrealized_pnl_usdc = open_unrealized_pnl_usdc;
        self.equity_usdc =
            self.initial_balance_usdc + self.realized_pnl_usdc + self.unrealized_pnl_usdc;
        self.balance_usdc = (self.equity_usdc - open_mark_value_usdc).max(0.0);

        self.refresh_day_marker_if_needed();

        let current_total_pnl = self.total_pnl();
        if current_total_pnl + 1e-9 < prev_total_pnl {
            self.consecutive_loss_events += 1;
        } else {
            self.consecutive_loss_events = 0;
        }
    }

    /// Net P&L relative to starting balance (equity-based).
    pub fn total_pnl(&self) -> f64 {
        self.equity_usdc - self.initial_balance_usdc
    }

    /// Bot uptime as a human-readable string (e.g. "2h 15m 30s").
    pub fn uptime(&self) -> String {
        let secs = (Utc::now() - self.started_at).num_seconds().max(0) as u64;
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        let s = secs % 60;
        if h > 0 {
            format!("{h}h {m}m {s}s")
        } else if m > 0 {
            format!("{m}m {s}s")
        } else {
            format!("{s}s")
        }
    }

    fn refresh_day_marker_if_needed(&mut self) {
        let today = Utc::now().date_naive().to_string();
        if self.day_marker_utc != today {
            self.day_marker_utc = today;
            self.day_start_equity_usdc = self.equity_usdc;
            self.consecutive_loss_events = 0;
        }
    }

    fn recompute_mark_to_market(&mut self) {
        let mut invested = 0.0_f64;
        let mut mark_value = 0.0_f64;

        for (market, pos) in &self.market_positions {
            let avg_yes = if pos.yes_shares > 0.0 {
                pos.yes_cost_usdc / pos.yes_shares
            } else {
                0.5
            };
            let avg_no = if pos.no_shares > 0.0 {
                pos.no_cost_usdc / pos.no_shares
            } else {
                0.5
            };
            let fallback_yes = if pos.yes_shares > 0.0 {
                avg_yes
            } else if pos.no_shares > 0.0 {
                1.0 - avg_no
            } else {
                0.5
            }
            .clamp(0.0001, 0.9999);

            let yes_price = self
                .last_yes_prices
                .get(market)
                .copied()
                .unwrap_or(fallback_yes)
                .clamp(0.0001, 0.9999);
            let no_price = (1.0 - yes_price).clamp(0.0001, 0.9999);

            invested += pos.yes_cost_usdc + pos.no_cost_usdc;
            mark_value += pos.yes_shares * yes_price + pos.no_shares * no_price;
        }

        self.unrealized_pnl_usdc = mark_value - invested;
        self.equity_usdc = self.balance_usdc + mark_value;
    }
}

/// Thread-safe handle to [`BotState`].
pub type SharedState = Arc<RwLock<BotState>>;

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_trade(size_usdc: f64) -> TradeRecord {
        TradeRecord {
            timestamp: Utc::now(),
            whale_address: "0xABC".to_string(),
            market: "0xDEF".to_string(),
            side: "Buy".to_string(),
            price: 0.5,
            outcome: "YES".to_string(),
            effective_price: 0.5,
            shares: size_usdc / 0.5,
            size_usdc,
            kelly_fraction: 0.1,
            simulated: true,
        }
    }

    #[test]
    fn initial_balance_is_set() {
        let state = BotState::new(true, 100.0);
        assert_eq!(state.balance_usdc, 100.0);
        assert_eq!(state.equity_usdc, 100.0);
        assert_eq!(state.initial_balance_usdc, 100.0);
        assert_eq!(state.trade_count, 0);
        assert!(state.skip_reasons.is_empty());
    }

    #[test]
    fn trade_reduces_cash_and_keeps_equity_flat_at_entry() {
        let mut state = BotState::new(true, 100.0);
        state.record_trade(make_trade(10.0));
        assert_eq!(state.balance_usdc, 90.0);
        assert_eq!(state.trade_count, 1);
        assert!((state.equity_usdc - 100.0).abs() < 1e-9);
        assert!((state.total_pnl() - 0.0).abs() < 1e-9);
    }

    #[test]
    fn pnl_changes_with_price_marks() {
        let mut state = BotState::new(true, 100.0);
        let mut t = make_trade(20.0);
        t.price = 0.40;
        t.effective_price = 0.40;
        t.shares = 50.0;
        state.record_trade(t);

        let mut t2 = make_trade(0.0);
        t2.market = "0xDEF".to_string();
        t2.size_usdc = 0.0;
        t2.shares = 0.0;
        t2.price = 0.60;
        state.record_trade(t2);

        assert!(state.total_pnl() > 0.0);
    }

    #[test]
    fn recent_trades_ring_buffer_caps_at_200() {
        let mut state = BotState::new(false, 0.0);
        for _ in 0..250 {
            state.record_trade(make_trade(0.0));
        }
        assert_eq!(state.recent_trades.len(), 200);
        assert_eq!(state.trade_count, 250);
    }

    #[test]
    fn uptime_returns_non_empty_string() {
        let state = BotState::new(true, 100.0);
        let s = state.uptime();
        assert!(!s.is_empty());
    }

    #[test]
    fn skip_reasons_are_counted() {
        let mut state = BotState::new(true, 100.0);
        state.record_skip("net_edge_below_threshold");
        state.record_skip("net_edge_below_threshold");
        state.record_skip("kelly_no_edge");
        assert_eq!(state.skip_reasons["net_edge_below_threshold"], 2);
        assert_eq!(state.skip_reasons["kelly_no_edge"], 1);
    }

    #[test]
    fn rolling_skip_reasons_window_counts_correctly() {
        let mut state = BotState::new(true, 100.0);
        let now = Utc::now();
        state.record_skip_at(
            "net_edge_below_threshold",
            now - chrono::Duration::minutes(10),
        );
        state.record_skip_at("net_edge_below_threshold", now - chrono::Duration::hours(2));
        state.record_skip_at("kelly_no_edge", now - chrono::Duration::hours(8));
        state.record_skip_at("data_api_error", now - chrono::Duration::hours(30));

        let h1 = state.skip_reasons_since(3600);
        assert_eq!(h1["net_edge_below_threshold"], 1);
        assert!(!h1.contains_key("kelly_no_edge"));

        let h24 = state.skip_reasons_since(24 * 3600);
        assert_eq!(h24["net_edge_below_threshold"], 2);
        assert_eq!(h24["kelly_no_edge"], 1);
        assert!(!h24.contains_key("data_api_error"));
    }

    #[test]
    fn snapshot_roundtrip_restores_state() {
        let mut state = BotState::new(true, 100.0);
        state.record_trade(make_trade(10.0));
        state.record_skip("x");
        let snap = state.snapshot();

        let mut restored = BotState::new(true, 1.0);
        restored.restore_from_snapshot(snap);

        assert_eq!(restored.trade_count, 1);
        assert_eq!(restored.skip_reasons["x"], 1);
        assert_eq!(restored.balance_usdc, 90.0);
    }
}
