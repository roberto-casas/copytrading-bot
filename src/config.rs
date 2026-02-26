use anyhow::{Context, Result, bail};
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

    // ── Investigation ────────────────────────────────────────────────────────
    /// How many candidate traders to pull from the leaderboard each cycle.
    pub leaderboard_fetch_size: i32,
    /// Maximum number of whales to track simultaneously.
    pub max_whales: usize,
    /// Minimum number of resolved trades a candidate must have.
    pub min_resolved_trades: i32,
    /// How often (in seconds) to re-run the investigation cycle.
    pub investigation_interval_secs: u64,

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

    // ── Polymarket credentials (required only when dry_run = false) ──────────
    /// Ethereum private key (hex, with or without 0x prefix).
    pub private_key: String,
    /// Polymarket CLOB API key (UUID format).
    pub api_key: String,
    /// Polymarket CLOB API secret.
    pub api_secret: String,
    /// Polymarket CLOB API passphrase.
    pub api_passphrase: String,
    /// Ethereum address matching the private key.
    pub address: String,
}

impl Config {
    /// Load from the process environment (call `dotenv::dotenv()` first).
    pub fn from_env() -> Result<Self> {
        let dry_run = parse_env_or("DRY_RUN", false);

        let leaderboard_fetch_size = parse_env_or("LEADERBOARD_FETCH_SIZE", 50_i32);
        if !(1..=50).contains(&leaderboard_fetch_size) {
            bail!("LEADERBOARD_FETCH_SIZE must be between 1 and 50");
        }

        let max_whales = parse_env_or("MAX_WHALES", 10_usize);
        if max_whales == 0 || max_whales > 10 {
            bail!("MAX_WHALES must be between 1 and 10");
        }

        let kelly_fraction = parse_env_or("KELLY_FRACTION", 0.5_f64);
        if !(0.0..=1.0).contains(&kelly_fraction) {
            bail!("KELLY_FRACTION must be between 0.0 and 1.0");
        }

        let max_position_fraction = parse_env_or("MAX_POSITION_FRACTION", 0.20_f64);
        if !(0.0..=1.0).contains(&max_position_fraction) {
            bail!("MAX_POSITION_FRACTION must be between 0.0 and 1.0");
        }

        // Credentials are only required for live trading.
        let (private_key, api_key, api_secret, api_passphrase, address) = if dry_run {
            (
                env::var("POLYMARKET_PRIVATE_KEY").unwrap_or_default(),
                env::var("POLYMARKET_API_KEY").unwrap_or_default(),
                env::var("POLYMARKET_API_SECRET").unwrap_or_default(),
                env::var("POLYMARKET_API_PASSPHRASE").unwrap_or_default(),
                env::var("POLYMARKET_ADDRESS").unwrap_or_default(),
            )
        } else {
            (
                require_env("POLYMARKET_PRIVATE_KEY")?,
                require_env("POLYMARKET_API_KEY")?,
                require_env("POLYMARKET_API_SECRET")?,
                require_env("POLYMARKET_API_PASSPHRASE")?,
                require_env("POLYMARKET_ADDRESS")?,
            )
        };

        Ok(Self {
            dry_run,
            dry_run_balance_usdc: parse_env_or("DRY_RUN_BALANCE_USDC", 100.0_f64),
            dashboard_port: parse_env_or("DASHBOARD_PORT", 8080_u16),
            leaderboard_fetch_size,
            max_whales,
            min_resolved_trades: parse_env_or("MIN_RESOLVED_TRADES", 10_i32),
            investigation_interval_secs: parse_env_or("INVESTIGATION_INTERVAL_SECS", 3600_u64),
            bankroll_usdc: parse_env_or("BANKROLL_USDC", 1000.0_f64),
            kelly_fraction,
            max_position_fraction,
            poll_interval_secs: parse_env_or("POLL_INTERVAL_SECS", 30_u64),
            private_key,
            api_key,
            api_secret,
            api_passphrase,
            address,
        })
    }
}

fn require_env(key: &str) -> Result<String> {
    env::var(key).with_context(|| format!("Missing required env var: {key}"))
}

fn parse_env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
