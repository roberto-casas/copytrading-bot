//! Kelly criterion implementation for Polymarket binary prediction markets.
//!
//! ## Background
//!
//! On Polymarket you buy YES (or NO) outcome tokens priced at *p* USDC each
//! (where *p* ∈ (0, 1)).  If you spend 1 USDC on YES tokens at price *p*:
//! - **Win**: you receive 1/p USDC → net profit = (1 − p)/p per USDC staked.
//! - **Lose**: you lose 1 USDC.
//!
//! This gives us the Kelly net-odds:  **b = (1 − p) / p**
//!
//! The classic Kelly fraction is then:
//!   f* = (b·q_est − (1 − q_est)) / b
//!      = q_est − (1 − q_est)·p / (1 − p)
//!
//! where `q_est` is our *estimated* probability that the outcome resolves YES
//! (derived from the whale's historical win rate).
//!
//! We apply two safety controls:
//! 1. **Fractional Kelly** — multiply f* by `kelly_fraction` (e.g. 0.5).
//! 2. **Hard cap** — clamp the result to `max_position_fraction`.

/// Result of a Kelly criterion calculation.
#[derive(Debug, Clone, PartialEq)]
pub struct KellyResult {
    /// Raw Kelly fraction before any safety adjustments (may be negative).
    pub raw_fraction: f64,
    /// Adjusted fraction after fractional Kelly and hard cap (always ≥ 0).
    pub adjusted_fraction: f64,
    /// Recommended USDC position size = bankroll × adjusted_fraction.
    pub position_size_usdc: f64,
}

/// Compute the Kelly position size for a binary Polymarket outcome token.
///
/// # Arguments
/// * `market_price`        – Current YES token price in USDC (0 < p < 1).
/// * `whale_win_rate`      – Fraction of resolved trades the whale won (our p_est).
/// * `bankroll_usdc`       – Total USDC bankroll available to the bot.
/// * `kelly_fraction`      – Fractional Kelly multiplier (0 < f ≤ 1).
/// * `max_position_fraction` – Hard cap as a fraction of bankroll.
///
/// # Returns
/// `None` when the raw Kelly fraction is non-positive (no edge, skip trade).
pub fn kelly(
    market_price: f64,
    whale_win_rate: f64,
    bankroll_usdc: f64,
    kelly_fraction: f64,
    max_position_fraction: f64,
) -> Option<KellyResult> {
    // Price must be strictly in (0, 1): 0 and 1 are degenerate cases.
    if market_price <= 0.0 || market_price >= 1.0 {
        return None;
    }
    if !(0.0..=1.0).contains(&whale_win_rate) {
        return None;
    }
    if bankroll_usdc <= 0.0 || kelly_fraction <= 0.0 || max_position_fraction <= 0.0 {
        return None;
    }

    // Net odds per unit staked: winning 1 USDC of YES tokens at price p yields
    // (1-p)/p net USDC profit.
    let b = (1.0 - market_price) / market_price;

    // Our estimated win probability comes from the whale's historical win rate.
    let p_est = whale_win_rate;
    let q_est = 1.0 - p_est;

    // Full Kelly: f* = (b·p − q) / b
    let raw_fraction = (b * p_est - q_est) / b;

    // No edge: skip the trade.
    if raw_fraction <= 0.0 {
        return None;
    }

    // Apply fractional Kelly multiplier then hard cap.
    let adjusted_fraction = (raw_fraction * kelly_fraction).min(max_position_fraction);
    let position_size_usdc = bankroll_usdc * adjusted_fraction;

    Some(KellyResult {
        raw_fraction,
        adjusted_fraction,
        position_size_usdc,
    })
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    // At market_price = 0.5, b = 1.  p_est = 0.6, q = 0.4.
    // f* = (1·0.6 − 0.4)/1 = 0.2
    #[test]
    fn known_kelly_value() {
        let r = kelly(0.5, 0.6, 1000.0, 1.0, 1.0).unwrap();
        assert_relative_eq!(r.raw_fraction, 0.2, epsilon = 1e-9);
        assert_relative_eq!(r.adjusted_fraction, 0.2, epsilon = 1e-9);
        assert_relative_eq!(r.position_size_usdc, 200.0, epsilon = 1e-6);
    }

    #[test]
    fn half_kelly_halves_fraction() {
        let full = kelly(0.5, 0.6, 1000.0, 1.0, 1.0).unwrap();
        let half = kelly(0.5, 0.6, 1000.0, 0.5, 1.0).unwrap();
        assert_relative_eq!(half.adjusted_fraction, full.raw_fraction * 0.5, epsilon = 1e-9);
    }

    #[test]
    fn hard_cap_is_respected() {
        // Very high raw fraction but cap at 20 %.
        let r = kelly(0.1, 0.95, 1000.0, 1.0, 0.20).unwrap();
        assert!(r.adjusted_fraction <= 0.20 + 1e-10);
        assert_relative_eq!(r.position_size_usdc, 200.0, epsilon = 1e-6);
    }

    #[test]
    fn no_edge_returns_none() {
        // Win rate at 50 % on a 50-cent market → raw Kelly = 0, no edge.
        assert!(kelly(0.5, 0.5, 1000.0, 0.5, 0.20).is_none());
    }

    #[test]
    fn negative_edge_returns_none() {
        // Whale wins only 30 % on a 50-cent market → negative edge.
        assert!(kelly(0.5, 0.3, 1000.0, 0.5, 0.20).is_none());
    }

    #[test]
    fn invalid_price_returns_none() {
        assert!(kelly(0.0, 0.6, 1000.0, 0.5, 0.20).is_none());
        assert!(kelly(1.0, 0.6, 1000.0, 0.5, 0.20).is_none());
        assert!(kelly(-0.1, 0.6, 1000.0, 0.5, 0.20).is_none());
    }

    #[test]
    fn position_size_scales_with_bankroll() {
        let r1 = kelly(0.5, 0.6, 1000.0, 0.5, 1.0).unwrap();
        let r2 = kelly(0.5, 0.6, 2000.0, 0.5, 1.0).unwrap();
        assert_relative_eq!(r2.position_size_usdc, 2.0 * r1.position_size_usdc, epsilon = 1e-9);
    }

    #[test]
    fn high_priced_token_lower_kelly() {
        // At price 0.6 with a 70% win rate there is edge (Kelly > 0).
        // At price 0.3 with the same win rate there is more edge (higher Kelly).
        // Buying a 60-cent token (market pricing it at 60%) with a 70% win rate
        // gives less Kelly fraction than a 30-cent token with the same win rate.
        let cheap = kelly(0.3, 0.7, 1000.0, 1.0, 1.0).unwrap();
        let expensive = kelly(0.6, 0.7, 1000.0, 1.0, 1.0).unwrap();
        assert!(cheap.raw_fraction > expensive.raw_fraction);
    }

    #[test]
    fn sell_side_uses_inverted_price() {
        // When selling YES at price 0.7, the whale is effectively buying NO at 0.3.
        // The trading loop passes effective_price = 1 - 0.7 = 0.3 for sell trades.
        // Kelly on the inverted price should produce a valid result with a 70% win rate.
        let result = kelly(0.3, 0.7, 1000.0, 0.5, 0.20);
        assert!(result.is_some());
        let r = result.unwrap();
        assert!(r.adjusted_fraction > 0.0);
        assert!(r.position_size_usdc > 0.0);
    }

    #[test]
    fn near_boundary_prices_handled() {
        // Very close to 0 or 1 but still valid
        let near_zero = kelly(0.01, 0.6, 1000.0, 0.5, 0.20);
        assert!(near_zero.is_some());
        let near_one = kelly(0.99, 0.6, 1000.0, 0.5, 0.20);
        // Near price=1, odds are very poor, should return None (no edge)
        assert!(near_one.is_none());
    }

    #[test]
    fn tiny_bankroll_produces_small_position() {
        let r = kelly(0.4, 0.7, 10.0, 0.5, 0.20).unwrap();
        assert!(r.position_size_usdc <= 2.0); // at most 20% of $10
    }
}
