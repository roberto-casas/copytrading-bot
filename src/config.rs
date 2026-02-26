use anyhow::{Context, Result, bail};
use std::env;

/// All runtime configuration loaded from environment variables / `.env`.
#[derive(Debug, Clone)]
pub struct Config {
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

    // ── Polymarket credentials ───────────────────────────────────────────────
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

        Ok(Self {
            leaderboard_fetch_size,
            max_whales,
            min_resolved_trades: parse_env_or("MIN_RESOLVED_TRADES", 10_i32),
            investigation_interval_secs: parse_env_or("INVESTIGATION_INTERVAL_SECS", 3600_u64),
            bankroll_usdc: parse_env_or("BANKROLL_USDC", 1000.0_f64),
            kelly_fraction,
            max_position_fraction,
            poll_interval_secs: parse_env_or("POLL_INTERVAL_SECS", 30_u64),
            private_key: require_env("POLYMARKET_PRIVATE_KEY")?,
            api_key: require_env("POLYMARKET_API_KEY")?,
            api_secret: require_env("POLYMARKET_API_SECRET")?,
            api_passphrase: require_env("POLYMARKET_API_PASSPHRASE")?,
            address: require_env("POLYMARKET_ADDRESS")?,
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
