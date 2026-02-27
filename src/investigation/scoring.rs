//! Trader scoring: converts raw leaderboard + trade data into a single
//! comparable score used to select the top-N whales.
//!
//! Score formula (all components normalised to [0, 1]):
//!   score = 0.40 * roi_30d_norm + 0.35 * win_rate + 0.25 * volume_norm
//!
//! Where:
//! - `roi_30d_norm` = sigmoid of the 30-day ROI — rewards high recent
//!   returns while dampening extreme outliers.
//! - `win_rate` = fraction of resolved trades that were profitable
//!   (already in [0, 1]).
//! - `volume_norm` = sigmoid of the lifetime volume — rewards proven
//!   market participation without being dominated by it.
//!
//! Traders with fewer than `min_trades` resolved trades are excluded entirely.

use chrono::{DateTime, Utc};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;

/// Enriched trader profile produced by the investigation module.
#[derive(Debug, Clone)]
pub struct TraderProfile {
    /// On-chain proxy wallet address (checksummed hex string).
    pub address: String,
    /// PnL in USDC for the investigation leaderboard period (currently: monthly).
    pub period_pnl: Decimal,
    /// Fraction of resolved trades that closed in profit (0.0–1.0).
    pub win_rate: f64,
    /// Number of resolved trades used for this estimate.
    pub resolved_trade_count: i32,
    /// Count of resolved profitable trades.
    pub wins: i32,
    /// Bayesian posterior mean win-rate.
    pub posterior_win_rate: f64,
    /// Conservative lower-confidence bound for win-rate.
    pub conservative_win_rate: f64,
    /// Trading volume in USDC for the leaderboard period.
    pub volume: Decimal,
    /// Composite ranking score (higher is better).
    pub score: f64,
    /// Asset IDs (token IDs) the trader is currently active in, used for
    /// WebSocket orderbook subscriptions.
    pub active_asset_ids: Vec<String>,
    /// When this profile was last updated by the investigation cycle.
    pub updated_at: DateTime<Utc>,
}

/// Bayesian win-rate estimate and conservative lower bound.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BayesianWinRate {
    /// Posterior mean.
    pub mean: f64,
    /// Lower confidence bound (normal approximation).
    pub lower_bound: f64,
}

/// Compute a Bayesian win-rate estimate using a Beta prior.
///
/// - prior: Beta(alpha, beta)
/// - posterior: Beta(wins + alpha, losses + beta)
/// - lower bound: mean - z * stddev (clamped to [0, 1])
pub fn bayesian_win_rate(
    wins: i32,
    resolved_trade_count: i32,
    prior_alpha: f64,
    prior_beta: f64,
    confidence_z: f64,
) -> Option<BayesianWinRate> {
    if resolved_trade_count <= 0 || wins < 0 || wins > resolved_trade_count {
        return None;
    }
    if prior_alpha <= 0.0 || prior_beta <= 0.0 || confidence_z < 0.0 {
        return None;
    }

    let w = wins as f64;
    let n = resolved_trade_count as f64;
    let losses = n - w;

    let alpha_post = prior_alpha + w;
    let beta_post = prior_beta + losses;
    let denom = alpha_post + beta_post;
    if denom <= 0.0 {
        return None;
    }

    let mean = alpha_post / denom;
    let variance = (alpha_post * beta_post) / (denom * denom * (denom + 1.0));
    let stddev = variance.max(0.0).sqrt();
    let lower_bound = (mean - confidence_z * stddev).clamp(0.0, 1.0);

    Some(BayesianWinRate { mean, lower_bound })
}

/// Compute the composite score for a single trader.
///
/// Returns `None` when the trader has too few resolved trades.
pub fn compute_score(
    pnl_30d: Decimal,
    win_rate: f64,
    volume: Decimal,
    resolved_trade_count: i32,
    min_trades: i32,
) -> Option<f64> {
    if resolved_trade_count < min_trades {
        return None;
    }

    let roi_norm = sigmoid(pnl_30d.to_f64().unwrap_or(0.0) / 1000.0);
    let vol_norm = sigmoid(volume.to_f64().unwrap_or(0.0) / 50_000.0);
    let win = win_rate.clamp(0.0, 1.0);

    let score = 0.40 * roi_norm + 0.35 * win + 0.25 * vol_norm;
    Some(score)
}

/// Rank a mutable slice of profiles descending by score in-place and keep
/// only the top `max_whales` entries.
/// Profiles with NaN scores (which cannot arise from validated inputs but are
/// guarded against defensively) are sorted to the bottom.
pub fn rank_and_trim(profiles: &mut Vec<TraderProfile>, max_whales: usize) {
    profiles.sort_by(|a, b| {
        match (a.score.is_nan(), b.score.is_nan()) {
            (true, true) => std::cmp::Ordering::Equal,
            (true, false) => std::cmp::Ordering::Greater, // NaN sinks to bottom
            (false, true) => std::cmp::Ordering::Less,
            (false, false) => b
                .score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal),
        }
    });
    profiles.truncate(max_whales);
}

/// Logistic function: maps any real number to (0, 1).
/// Centred at 0; values >> 1 approach 1, values << -1 approach 0.
fn sigmoid(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn sigmoid_bounds() {
        // At ±10 the sigmoid is well within (0,1) but clearly near the extremes.
        assert!(sigmoid(10.0) < 1.0);
        assert!(sigmoid(10.0) > 0.99);
        assert!(sigmoid(-10.0) > 0.0);
        assert!(sigmoid(-10.0) < 0.01);
        // Midpoint
        let mid = sigmoid(0.0);
        assert!((mid - 0.5).abs() < 1e-10);
    }

    #[test]
    fn score_excluded_when_too_few_trades() {
        let result = compute_score(dec!(500), 0.7, dec!(20_000), 5, 10);
        assert!(result.is_none());
    }

    #[test]
    fn score_in_unit_interval() {
        let score = compute_score(dec!(500), 0.7, dec!(20_000), 20, 10).unwrap();
        assert!(score >= 0.0 && score <= 1.0, "score={score}");
    }

    #[test]
    fn better_trader_scores_higher() {
        let good = compute_score(dec!(2000), 0.75, dec!(100_000), 50, 10).unwrap();
        let poor = compute_score(dec!(50), 0.40, dec!(5_000), 15, 10).unwrap();
        assert!(good > poor, "good={good} poor={poor}");
    }

    #[test]
    fn rank_and_trim_keeps_top_n() {
        use rust_decimal_macros::dec;

        let make = |addr: &str, score: f64| TraderProfile {
            address: addr.to_string(),
            period_pnl: dec!(0),
            win_rate: 0.5,
            resolved_trade_count: 20,
            wins: 10,
            posterior_win_rate: 0.5,
            conservative_win_rate: 0.45,
            volume: dec!(0),
            score,
            active_asset_ids: vec![],
            updated_at: Utc::now(),
        };

        let mut profiles = vec![
            make("0xA", 0.3),
            make("0xB", 0.9),
            make("0xC", 0.6),
            make("0xD", 0.1),
            make("0xE", 0.8),
        ];

        rank_and_trim(&mut profiles, 3);

        assert_eq!(profiles.len(), 3);
        assert_eq!(profiles[0].address, "0xB");
        assert_eq!(profiles[1].address, "0xE");
        assert_eq!(profiles[2].address, "0xC");
    }

    #[test]
    fn bayesian_estimate_applies_shrinkage() {
        let est = bayesian_win_rate(6, 10, 2.0, 2.0, 1.64).unwrap();
        // Posterior mean should be between empirical 0.6 and prior mean 0.5.
        assert!(est.mean > 0.5 && est.mean < 0.6);
        assert!(est.lower_bound <= est.mean);
    }

    #[test]
    fn bayesian_invalid_inputs_return_none() {
        assert!(bayesian_win_rate(-1, 10, 2.0, 2.0, 1.0).is_none());
        assert!(bayesian_win_rate(11, 10, 2.0, 2.0, 1.0).is_none());
        assert!(bayesian_win_rate(5, 10, 0.0, 2.0, 1.0).is_none());
    }
}
