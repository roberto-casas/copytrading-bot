//! Investigation thread — periodically fetches Polymarket leaderboard and
//! trade history, scores each candidate, and publishes the top-N whale list.
//!
//! Uses the official `polymarket-client-sdk` Data API client.

pub mod scoring;

use std::sync::Arc;

use anyhow::Result;
use polymarket_client_sdk::data::types::request::{
    ClosedPositionsRequest, PositionsRequest, TraderLeaderboardRequest,
};
use polymarket_client_sdk::data::types::{LeaderboardCategory, TimePeriod};
use polymarket_client_sdk::data::Client as DataClient;
use scoring::{TraderProfile, compute_score, rank_and_trim};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::config::Config;

/// Shared state between the investigation and trading threads.
pub type WhaleList = Arc<RwLock<Vec<TraderProfile>>>;

/// Spawn the investigation task. It runs in a loop, sleeping
/// `config.investigation_interval_secs` between cycles.
pub fn spawn(config: Config, whale_list: WhaleList) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Err(e) = run_cycle(&config, &whale_list).await {
                error!("Investigation cycle failed: {e:#}");
            }
            info!(
                sleep_secs = config.investigation_interval_secs,
                "Investigation sleeping until next cycle"
            );
            tokio::time::sleep(tokio::time::Duration::from_secs(
                config.investigation_interval_secs,
            ))
            .await;
        }
    })
}

/// Execute one full investigation cycle:
/// 1. Fetch the leaderboard (top-N by PnL, monthly window).
/// 2. For each candidate fetch closed positions → derive win_rate.
/// 3. Fetch open positions → collect active asset_ids for WS subscriptions.
/// 4. Score & rank; write top-`max_whales` to the shared whale list.
async fn run_cycle(config: &Config, whale_list: &WhaleList) -> Result<()> {
    let client = DataClient::default();

    info!("Investigation cycle: fetching leaderboard");

    let leaderboard = client
        .leaderboard(
            &TraderLeaderboardRequest::builder()
                .category(LeaderboardCategory::Overall)
                .time_period(TimePeriod::Month)
                .limit(config.leaderboard_fetch_size)?
                .build(),
        )
        .await?;

    info!(count = leaderboard.len(), "Leaderboard fetched");

    let mut profiles: Vec<TraderProfile> = Vec::new();

    for entry in &leaderboard {
        let addr = entry.proxy_wallet;
        let addr_str = addr.to_string();

        // ── Closed positions for win-rate ─────────────────────────────────
        let closed = match client
            .closed_positions(
                &ClosedPositionsRequest::builder()
                    .user(addr)
                    .limit(50)?
                    .build(),
            )
            .await
        {
            Ok(v) => v,
            Err(e) => {
                warn!(address = %addr_str, "closed_positions error: {e}");
                continue;
            }
        };

        let resolved_count = closed.len() as i32;
        let wins = closed
            .iter()
            .filter(|p| p.realized_pnl.is_sign_positive())
            .count();
        let win_rate = if resolved_count > 0 {
            wins as f64 / resolved_count as f64
        } else {
            0.0
        };

        // ── Score ─────────────────────────────────────────────────────────
        let score = match compute_score(
            entry.pnl,
            win_rate,
            entry.vol,
            resolved_count,
            config.min_resolved_trades,
        ) {
            Some(s) => s,
            None => {
                info!(
                    address = %addr_str,
                    resolved_count,
                    "Skipping trader — insufficient trade history"
                );
                continue;
            }
        };

        // ── Open positions for WebSocket asset subscriptions ──────────────
        let open = match client
            .positions(
                &PositionsRequest::builder()
                    .user(addr)
                    .limit(20)?
                    .build(),
            )
            .await
        {
            Ok(v) => v,
            Err(e) => {
                warn!(address = %addr_str, "positions error: {e}");
                vec![]
            }
        };

        let active_asset_ids: Vec<String> =
            open.iter().map(|p| p.asset.to_string()).collect();

        profiles.push(TraderProfile {
            address: addr_str,
            period_pnl: entry.pnl,
            win_rate,
            resolved_trade_count: resolved_count,
            volume: entry.vol,
            score,
            active_asset_ids,
        });
    }

    rank_and_trim(&mut profiles, config.max_whales);

    info!(
        whale_count = profiles.len(),
        "Investigation complete — whale list updated"
    );
    for (i, w) in profiles.iter().enumerate() {
        info!(
            rank = i + 1,
            address = %w.address,
            score = format_args!("{:.4}", w.score),
            win_rate = format_args!("{:.1}%", w.win_rate * 100.0),
            period_pnl = %w.period_pnl,
        );
    }

    *whale_list.write().await = profiles;
    Ok(())
}
