//! Shared bot state accessible by the trading thread, investigation thread,
//! and the web dashboard.

use std::collections::VecDeque;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::RwLock;

/// Maximum number of recent trades stored in memory.
const MAX_RECENT_TRADES: usize = 200;

/// A single trade record (simulated in dry-run mode, real in live mode).
#[derive(Debug, Clone, Serialize)]
pub struct TradeRecord {
    /// UTC timestamp when the trade was (or would have been) executed.
    pub timestamp: DateTime<Utc>,
    /// Whale wallet whose activity triggered this copy trade.
    pub whale_address: String,
    /// Market condition ID.
    pub market: String,
    /// "Buy" or "Sell".
    pub side: String,
    /// Execution price (0–1).
    pub price: f64,
    /// USDC size of the copy position.
    pub size_usdc: f64,
    /// Adjusted Kelly fraction used to size the position.
    pub kelly_fraction: f64,
    /// `true` when in dry-run mode (no real order was placed).
    pub simulated: bool,
}

/// Top-level shared bot state.
#[derive(Debug, Serialize)]
pub struct BotState {
    /// Whether the bot is running in dry-run (paper trading) mode.
    pub dry_run: bool,
    /// Current balance (real for live mode; simulated for dry-run).
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
}

impl BotState {
    /// Create a new `BotState`.
    pub fn new(dry_run: bool, initial_balance_usdc: f64) -> Self {
        Self {
            dry_run,
            balance_usdc: initial_balance_usdc,
            initial_balance_usdc,
            started_at: Utc::now(),
            whale_count: 0,
            trade_count: 0,
            recent_trades: VecDeque::new(),
        }
    }

    /// Append a trade record and update the running balance.
    ///
    /// In dry-run mode the position cost is deducted from the simulated balance.
    ///
    /// # Balance tracking note
    /// Only position *costs* are tracked here (not future P&L), because the bot
    /// cannot know whether a prediction market position ultimately resolves in
    /// profit without subscribing to settlement events.  The balance therefore
    /// decreases with each trade and represents the remaining capital available
    /// for new positions.  True P&L tracking would require monitoring market
    /// resolution events, which is a planned future enhancement.
    pub fn record_trade(&mut self, trade: TradeRecord) {
        if self.dry_run {
            // Deduct the position size from the simulated balance.
            self.balance_usdc -= trade.size_usdc;
            self.balance_usdc = self.balance_usdc.max(0.0);
        }
        self.trade_count += 1;
        self.recent_trades.push_front(trade);
        self.recent_trades.truncate(MAX_RECENT_TRADES);
    }

    /// Net P&L relative to the starting balance.
    pub fn total_pnl(&self) -> f64 {
        self.balance_usdc - self.initial_balance_usdc
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
            size_usdc,
            kelly_fraction: 0.1,
            simulated: true,
        }
    }

    #[test]
    fn initial_balance_is_set() {
        let state = BotState::new(true, 100.0);
        assert_eq!(state.balance_usdc, 100.0);
        assert_eq!(state.initial_balance_usdc, 100.0);
        assert_eq!(state.trade_count, 0);
    }

    #[test]
    fn dry_run_deducts_trade_from_balance() {
        let mut state = BotState::new(true, 100.0);
        state.record_trade(make_trade(10.0));
        assert_eq!(state.balance_usdc, 90.0);
        assert_eq!(state.trade_count, 1);
    }

    #[test]
    fn pnl_reflects_deductions() {
        let mut state = BotState::new(true, 100.0);
        state.record_trade(make_trade(25.0));
        assert_eq!(state.total_pnl(), -25.0);
    }

    #[test]
    fn balance_floors_at_zero() {
        let mut state = BotState::new(true, 10.0);
        state.record_trade(make_trade(999.0));
        assert_eq!(state.balance_usdc, 0.0);
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
}
