use anyhow::{anyhow, bail, Context, Result};
use std::env;

/// All runtime configuration loaded from environment variables / `.env`.
#[derive(Debug, Clone)]
pub struct Config {
    // ── Dry-run mode ─────────────────────────────────────────────────────────
    /// When `true` the bot detects trades but never submits real orders.
    /// Polymarket credentials are not required in this mode.
    pub dry_run: bool,
    /// Starting (virtual) USDC balance for dry-run simulations.
    /// Ignored when `dry_run` is `false`.
    pub dry_run_balance_usdc: f64,

    // ── Dashboard ────────────────────────────────────────────────────────────
    /// TCP port the web dashboard listens on.
    pub dashboard_port: u16,
    /// Optional webhook URL for push alerts when skip-pressure spikes.
    pub skip_alert_webhook_url: Option<String>,
    /// Per-reason skip count threshold within 1h that triggers alerts.
    pub skip_alert_1h_threshold: usize,
    /// Minimum seconds between repeated alert sends for unchanged spikes.
    pub skip_alert_cooldown_secs: u64,
    /// How often to evaluate skip-pressure alerts.
    pub skip_alert_eval_interval_secs: u64,
    /// Optional shared secret for HMAC-SHA256 webhook signature header.
    pub skip_alert_hmac_secret: Option<String>,
    /// Max retry attempts for webhook delivery (in addition to first try).
    pub skip_alert_max_retries: usize,
    /// Base backoff delay (milliseconds) for webhook retries.
    pub skip_alert_retry_backoff_ms: u64,
    /// Directory for restart-safe runtime state files.
    pub state_dir: String,
    /// How often to flush shared bot telemetry to disk.
    pub state_flush_interval_secs: u64,
    /// How often to sync live account open/closed positions from Data API.
    pub account_sync_interval_secs: u64,

    // ── Investigation ────────────────────────────────────────────────────────
    /// How many candidate traders to pull from the leaderboard each cycle.
    pub leaderboard_fetch_size: i32,
    /// Maximum number of whales to track simultaneously.
    pub max_whales: usize,
    /// Minimum number of resolved trades a candidate must have.
    pub min_resolved_trades: i32,
    /// How often (in seconds) to re-run the investigation cycle.
    pub investigation_interval_secs: u64,
    /// Beta prior alpha for Bayesian win-rate smoothing.
    pub bayes_prior_alpha: f64,
    /// Beta prior beta for Bayesian win-rate smoothing.
    pub bayes_prior_beta: f64,
    /// Z-score used for conservative lower-confidence bound.
    pub bayes_confidence_z: f64,

    // ── Trading ──────────────────────────────────────────────────────────────
    /// Total USDC bankroll available to the bot.
    pub bankroll_usdc: f64,
    /// Fractional Kelly multiplier applied on top of the full Kelly fraction.
    /// 0.5 = half-Kelly (recommended), 1.0 = full Kelly.
    pub kelly_fraction: f64,
    /// Hard cap on any single position as a fraction of bankroll (e.g. 0.20 = 20%).
    pub max_position_fraction: f64,
    /// How often (in seconds) the trading thread polls whale trades via Data API
    /// between WebSocket orderbook events.
    pub poll_interval_secs: u64,
    /// Estimated round-trip fee cost in basis points.
    pub estimated_fee_bps: f64,
    /// Estimated slippage in basis points.
    pub estimated_slippage_bps: f64,
    /// Additional adverse selection buffer in basis points.
    pub adverse_selection_bps: f64,
    /// Minimum required net edge in basis points to take a trade.
    pub min_net_edge_bps: f64,
    /// Maximum allowed drift from whale execution price to current quote (bps).
    pub max_copy_price_drift_bps: f64,
    /// Protective market-order price buffer from current quote (bps).
    pub max_order_slippage_bps: f64,
    /// Stop opening new trades if intraday equity drawdown exceeds this value.
    pub daily_loss_limit_usdc: f64,
    /// Stop opening new trades when this many consecutive loss events occur.
    pub max_consecutive_losses: usize,
    /// Absolute notional cap per market across YES+NO exposure.
    pub max_notional_per_market_usdc: f64,

    // ── Polymarket credentials (required only when dry_run = false) ──────────
    /// Ethereum private key (hex, with or without 0x prefix).
    pub private_key: String,
    /// Ethereum address matching the private key.
    pub address: String,
}

impl Config {
    /// Load from the process environment (call `dotenv::dotenv()` first).
    pub fn from_env() -> Result<Self> {
        let dry_run = parse_env_or("DRY_RUN", false)?;

        let leaderboard_fetch_size = parse_env_or("LEADERBOARD_FETCH_SIZE", 50_i32)?;
        if !(1..=50).contains(&leaderboard_fetch_size) {
            bail!("LEADERBOARD_FETCH_SIZE must be between 1 and 50");
        }

        let max_whales = parse_env_or("MAX_WHALES", 10_usize)?;
        if max_whales == 0 || max_whales > 10 {
            bail!("MAX_WHALES must be between 1 and 10");
        }

        let skip_alert_webhook_url = env::var("SKIP_ALERT_WEBHOOK_URL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let skip_alert_1h_threshold = parse_env_or("SKIP_ALERT_1H_THRESHOLD", 25_usize)?;
        if skip_alert_1h_threshold == 0 {
            bail!("SKIP_ALERT_1H_THRESHOLD must be >= 1");
        }

        let skip_alert_cooldown_secs = parse_env_or("SKIP_ALERT_COOLDOWN_SECS", 900_u64)?;
        if skip_alert_cooldown_secs == 0 {
            bail!("SKIP_ALERT_COOLDOWN_SECS must be >= 1");
        }

        let skip_alert_eval_interval_secs = parse_env_or("SKIP_ALERT_EVAL_INTERVAL_SECS", 60_u64)?;
        if skip_alert_eval_interval_secs == 0 {
            bail!("SKIP_ALERT_EVAL_INTERVAL_SECS must be >= 1");
        }
        let skip_alert_hmac_secret = env::var("SKIP_ALERT_HMAC_SECRET")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let skip_alert_max_retries = parse_env_or("SKIP_ALERT_MAX_RETRIES", 3_usize)?;
        if skip_alert_max_retries > 10 {
            bail!("SKIP_ALERT_MAX_RETRIES must be between 0 and 10");
        }
        let skip_alert_retry_backoff_ms = parse_env_or("SKIP_ALERT_RETRY_BACKOFF_MS", 500_u64)?;
        if skip_alert_retry_backoff_ms == 0 {
            bail!("SKIP_ALERT_RETRY_BACKOFF_MS must be >= 1");
        }
        let state_dir = parse_env_or("STATE_DIR", ".runtime".to_string())?;
        if state_dir.trim().is_empty() {
            bail!("STATE_DIR cannot be empty");
        }
        let state_flush_interval_secs = parse_env_or("STATE_FLUSH_INTERVAL_SECS", 30_u64)?;
        if state_flush_interval_secs == 0 {
            bail!("STATE_FLUSH_INTERVAL_SECS must be >= 1");
        }
        let account_sync_interval_secs = parse_env_or("ACCOUNT_SYNC_INTERVAL_SECS", 120_u64)?;
        if account_sync_interval_secs == 0 {
            bail!("ACCOUNT_SYNC_INTERVAL_SECS must be >= 1");
        }

        let kelly_fraction = parse_env_or("KELLY_FRACTION", 0.5_f64)?;
        if !(0.0..=1.0).contains(&kelly_fraction) {
            bail!("KELLY_FRACTION must be between 0.0 and 1.0");
        }

        let max_position_fraction = parse_env_or("MAX_POSITION_FRACTION", 0.20_f64)?;
        if !(0.0..=1.0).contains(&max_position_fraction) {
            bail!("MAX_POSITION_FRACTION must be between 0.0 and 1.0");
        }

        let dry_run_balance_usdc = parse_env_or("DRY_RUN_BALANCE_USDC", 100.0_f64)?;
        if dry_run_balance_usdc <= 0.0 {
            bail!("DRY_RUN_BALANCE_USDC must be > 0");
        }

        let min_resolved_trades = parse_env_or("MIN_RESOLVED_TRADES", 10_i32)?;
        if min_resolved_trades <= 0 {
            bail!("MIN_RESOLVED_TRADES must be > 0");
        }

        let investigation_interval_secs = parse_env_or("INVESTIGATION_INTERVAL_SECS", 3600_u64)?;
        if investigation_interval_secs == 0 {
            bail!("INVESTIGATION_INTERVAL_SECS must be >= 1");
        }

        let bankroll_usdc = parse_env_or("BANKROLL_USDC", 1000.0_f64)?;
        if bankroll_usdc <= 0.0 {
            bail!("BANKROLL_USDC must be > 0");
        }

        let poll_interval_secs = parse_env_or("POLL_INTERVAL_SECS", 30_u64)?;
        if poll_interval_secs == 0 {
            bail!("POLL_INTERVAL_SECS must be >= 1");
        }

        let bayes_prior_alpha = parse_env_or("BAYES_PRIOR_ALPHA", 2.0_f64)?;
        if bayes_prior_alpha <= 0.0 {
            bail!("BAYES_PRIOR_ALPHA must be > 0");
        }
        let bayes_prior_beta = parse_env_or("BAYES_PRIOR_BETA", 2.0_f64)?;
        if bayes_prior_beta <= 0.0 {
            bail!("BAYES_PRIOR_BETA must be > 0");
        }
        let bayes_confidence_z = parse_env_or("BAYES_CONFIDENCE_Z", 1.64_f64)?;
        if !(0.0..=4.0).contains(&bayes_confidence_z) {
            bail!("BAYES_CONFIDENCE_Z must be between 0.0 and 4.0");
        }

        let estimated_fee_bps = parse_env_or("ESTIMATED_FEE_BPS", 20.0_f64)?;
        let estimated_slippage_bps = parse_env_or("ESTIMATED_SLIPPAGE_BPS", 25.0_f64)?;
        let adverse_selection_bps = parse_env_or("ADVERSE_SELECTION_BPS", 10.0_f64)?;
        let min_net_edge_bps = parse_env_or("MIN_NET_EDGE_BPS", 15.0_f64)?;
        let max_copy_price_drift_bps = parse_env_or("MAX_COPY_PRICE_DRIFT_BPS", 150.0_f64)?;
        let max_order_slippage_bps = parse_env_or("MAX_ORDER_SLIPPAGE_BPS", 80.0_f64)?;
        let daily_loss_limit_usdc = parse_env_or("DAILY_LOSS_LIMIT_USDC", 150.0_f64)?;
        if daily_loss_limit_usdc <= 0.0 {
            bail!("DAILY_LOSS_LIMIT_USDC must be > 0");
        }
        let max_consecutive_losses = parse_env_or("MAX_CONSECUTIVE_LOSSES", 8_usize)?;
        if max_consecutive_losses == 0 {
            bail!("MAX_CONSECUTIVE_LOSSES must be >= 1");
        }
        let max_notional_per_market_usdc = parse_env_or("MAX_NOTIONAL_PER_MARKET_USDC", 350.0_f64)?;
        if max_notional_per_market_usdc <= 0.0 {
            bail!("MAX_NOTIONAL_PER_MARKET_USDC must be > 0");
        }
        for (name, value) in [
            ("ESTIMATED_FEE_BPS", estimated_fee_bps),
            ("ESTIMATED_SLIPPAGE_BPS", estimated_slippage_bps),
            ("ADVERSE_SELECTION_BPS", adverse_selection_bps),
            ("MIN_NET_EDGE_BPS", min_net_edge_bps),
            ("MAX_COPY_PRICE_DRIFT_BPS", max_copy_price_drift_bps),
            ("MAX_ORDER_SLIPPAGE_BPS", max_order_slippage_bps),
        ] {
            if !(0.0..=5000.0).contains(&value) {
                bail!("{name} must be between 0 and 5000 bps");
            }
        }

        // Credentials are only required for live trading.
        let (private_key, address) = if dry_run {
            // Keep optional in dry-run for convenience.
            (
                env::var("POLYMARKET_PRIVATE_KEY").unwrap_or_default(),
                env::var("POLYMARKET_ADDRESS").unwrap_or_default(),
            )
        } else {
            // Still validate all live-trading credential env vars even though
            // only the private key is currently consumed by the SDK path.
            let key = require_env("POLYMARKET_PRIVATE_KEY")?;
            let address = require_env("POLYMARKET_ADDRESS")?;
            let _ = require_env("POLYMARKET_API_KEY")?;
            let _ = require_env("POLYMARKET_API_SECRET")?;
            let _ = require_env("POLYMARKET_API_PASSPHRASE")?;
            (key, address)
        };

        Ok(Self {
            dry_run,
            dry_run_balance_usdc,
            dashboard_port: parse_env_or("DASHBOARD_PORT", 8080_u16)?,
            skip_alert_webhook_url,
            skip_alert_1h_threshold,
            skip_alert_cooldown_secs,
            skip_alert_eval_interval_secs,
            skip_alert_hmac_secret,
            skip_alert_max_retries,
            skip_alert_retry_backoff_ms,
            state_dir,
            state_flush_interval_secs,
            account_sync_interval_secs,
            leaderboard_fetch_size,
            max_whales,
            min_resolved_trades,
            investigation_interval_secs,
            bayes_prior_alpha,
            bayes_prior_beta,
            bayes_confidence_z,
            bankroll_usdc,
            kelly_fraction,
            max_position_fraction,
            poll_interval_secs,
            estimated_fee_bps,
            estimated_slippage_bps,
            adverse_selection_bps,
            min_net_edge_bps,
            max_copy_price_drift_bps,
            max_order_slippage_bps,
            daily_loss_limit_usdc,
            max_consecutive_losses,
            max_notional_per_market_usdc,
            private_key,
            address,
        })
    }
}

fn require_env(key: &str) -> Result<String> {
    env::var(key).with_context(|| format!("Missing required env var: {key}"))
}

fn parse_env_or<T: std::str::FromStr>(key: &str, default: T) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    match env::var(key) {
        Ok(v) => v
            .parse::<T>()
            .map_err(|e| anyhow!("Invalid env var {key}: {e}")),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(e) => Err(anyhow!("Failed reading env var {key}: {e}")),
    }
}
