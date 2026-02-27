//! Web dashboard — serves a single-page auto-refreshing HTML dashboard at `/`
//! and a JSON API at `/api/state`.
//!
//! ## Endpoints
//! - `GET /`          → Full HTML dashboard (auto-refreshes every 10 s)
//! - `GET /api/state` → Raw JSON of [`BotState`] for external consumers
//! - `GET /api/whales`→ JSON array of current whale profiles

use std::collections::HashSet;
use std::sync::Arc;

use axum::extract::State;
use axum::response::Html;
use axum::routing::get;
use axum::{Json, Router};
use tokio::sync::RwLock;
use tracing::info;

use crate::investigation::scoring::TraderProfile;
use crate::state::SharedState;

/// How often the dashboard HTML page auto-refreshes (seconds).
const DASHBOARD_REFRESH_SECS: u32 = 10;
/// Maximum number of top spike reasons included in alerts.
const SKIP_SPIKE_TOP_REASONS: usize = 5;

/// Safely truncate a string to show the first `prefix` and last `suffix` chars
/// separated by "…". Returns the original string if it's too short.
fn truncate_middle(s: &str, prefix: usize, suffix: usize) -> String {
    if s.len() <= prefix + suffix + 1 {
        return s.to_string();
    }
    format!("{}…{}", &s[..prefix], &s[s.len() - suffix..])
}

/// Combined state passed to axum handlers.
#[derive(Clone)]
pub struct DashboardState {
    pub bot: SharedState,
    pub whales: Arc<RwLock<Vec<TraderProfile>>>,
    pub skip_alert_1h_threshold: usize,
    pub daily_loss_limit_usdc: f64,
    pub max_consecutive_losses: usize,
    pub max_notional_per_market_usdc: f64,
}

/// Spawn the dashboard HTTP server on the given port.
pub fn spawn(
    port: u16,
    bot: SharedState,
    whales: Arc<RwLock<Vec<TraderProfile>>>,
    skip_alert_1h_threshold: usize,
    daily_loss_limit_usdc: f64,
    max_consecutive_losses: usize,
    max_notional_per_market_usdc: f64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let ds = DashboardState {
            bot,
            whales,
            skip_alert_1h_threshold,
            daily_loss_limit_usdc,
            max_consecutive_losses,
            max_notional_per_market_usdc,
        };

        let app = Router::new()
            .route("/", get(handle_dashboard))
            .route("/api/state", get(handle_api_state))
            .route("/api/breakers", get(handle_api_breakers))
            .route("/api/whales", get(handle_api_whales))
            .with_state(ds);

        let addr = format!("0.0.0.0:{port}");
        let listener = match tokio::net::TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!("Dashboard failed to bind {addr}: {e}");
                return;
            }
        };

        info!("Dashboard listening on http://{addr}");
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("Dashboard server error: {e}");
        }
    })
}

// ── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_api_state(State(ds): State<DashboardState>) -> Json<serde_json::Value> {
    let state = ds.bot.read().await;
    let threshold = ds.skip_alert_1h_threshold;
    let skip_reasons_1h = state.skip_reasons_since(3600);
    let skip_reasons_24h = state.skip_reasons_since(24 * 3600);
    let mut skip_alerts_1h: Vec<(String, usize)> = skip_reasons_1h
        .iter()
        .filter(|(_, count)| **count >= threshold)
        .map(|(reason, count)| (reason.clone(), *count))
        .collect();
    skip_alerts_1h.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let skip_alerts_1h: Vec<serde_json::Value> = skip_alerts_1h
        .into_iter()
        .take(SKIP_SPIKE_TOP_REASONS)
        .map(|(reason, count)| serde_json::json!({ "reason": reason, "count_1h": count }))
        .collect();
    let breaker_status = serde_json::json!({
        "daily_loss_limit_active": state.daily_drawdown_usdc() >= ds.daily_loss_limit_usdc,
        "daily_drawdown_usdc": state.daily_drawdown_usdc(),
        "daily_loss_limit_usdc": ds.daily_loss_limit_usdc,
        "max_consecutive_losses_active": state.consecutive_loss_events >= ds.max_consecutive_losses,
        "consecutive_loss_events": state.consecutive_loss_events,
        "max_consecutive_losses": ds.max_consecutive_losses,
        "max_notional_per_market_active": state.max_market_notional_usdc() >= ds.max_notional_per_market_usdc,
        "max_market_notional_usdc": state.max_market_notional_usdc(),
        "max_notional_per_market_usdc": ds.max_notional_per_market_usdc,
    });
    Json(serde_json::json!({
        "dry_run": state.dry_run,
        "balance_usdc": state.balance_usdc,
        "equity_usdc": state.equity_usdc,
        "unrealized_pnl_usdc": state.unrealized_pnl_usdc,
        "realized_pnl_usdc": state.realized_pnl_usdc,
        "initial_balance_usdc": state.initial_balance_usdc,
        "total_pnl_usdc": state.total_pnl(),
        "daily_drawdown_usdc": state.daily_drawdown_usdc(),
        "consecutive_loss_events": state.consecutive_loss_events,
        "uptime": state.uptime(),
        "whale_count": state.whale_count,
        "trade_count": state.trade_count,
        "recent_trades": state.recent_trades,
        "skip_reasons_total": state.skip_reasons,
        "skip_reasons_1h": skip_reasons_1h,
        "skip_reasons_24h": skip_reasons_24h,
        "skip_spike_1h_threshold": threshold,
        "skip_alerts_1h": skip_alerts_1h,
        "alert_stats": {
            "success_count": state.alert_success_count,
            "failure_count": state.alert_failure_count,
            "last_sent_at": state.last_alert_sent_at,
            "last_error": state.last_alert_error,
        },
        "breaker_status": breaker_status,
        "skip_reasons": state.skip_reasons,
    }))
}

async fn handle_api_breakers(State(ds): State<DashboardState>) -> Json<serde_json::Value> {
    let state = ds.bot.read().await;
    Json(serde_json::json!({
        "daily_loss_limit": {
            "active": state.daily_drawdown_usdc() >= ds.daily_loss_limit_usdc,
            "value_usdc": state.daily_drawdown_usdc(),
            "limit_usdc": ds.daily_loss_limit_usdc,
        },
        "max_consecutive_losses": {
            "active": state.consecutive_loss_events >= ds.max_consecutive_losses,
            "value": state.consecutive_loss_events,
            "limit": ds.max_consecutive_losses,
        },
        "max_notional_per_market": {
            "active": state.max_market_notional_usdc() >= ds.max_notional_per_market_usdc,
            "value_usdc": state.max_market_notional_usdc(),
            "limit_usdc": ds.max_notional_per_market_usdc,
        }
    }))
}

async fn handle_api_whales(State(ds): State<DashboardState>) -> Json<serde_json::Value> {
    let whales = ds.whales.read().await;
    let list: Vec<serde_json::Value> = whales
        .iter()
        .enumerate()
        .map(|(i, w)| {
            serde_json::json!({
                "rank": i + 1,
                "address": w.address,
                "score": w.score,
                "win_rate": w.win_rate,
                "posterior_win_rate": w.posterior_win_rate,
                "conservative_win_rate": w.conservative_win_rate,
                "period_pnl": w.period_pnl.to_string(),
                "volume": w.volume.to_string(),
                "wins": w.wins,
                "resolved_trade_count": w.resolved_trade_count,
                "active_markets": w.active_asset_ids.len(),
            })
        })
        .collect();
    Json(serde_json::json!(list))
}

async fn handle_dashboard(State(ds): State<DashboardState>) -> Html<String> {
    let state = ds.bot.read().await;
    let whales = ds.whales.read().await;
    let threshold = ds.skip_alert_1h_threshold;

    let mode_badge = if state.dry_run {
        r#"<span class="badge dry">📋 DRY RUN</span>"#
    } else {
        r#"<span class="badge live">⚡ LIVE</span>"#
    };

    let pnl = state.total_pnl();
    let pnl_class = if pnl >= 0.0 { "pos" } else { "neg" };
    let pnl_str = format!("{:+.2}", pnl);
    let skip_reasons_1h = state.skip_reasons_since(3600);
    let skip_reasons_24h = state.skip_reasons_since(24 * 3600);
    let total_skips: usize = state.skip_reasons.values().copied().sum();
    let skips_1h: usize = skip_reasons_1h.values().copied().sum();
    let skips_24h: usize = skip_reasons_24h.values().copied().sum();
    let mut skip_alerts_1h: Vec<(String, usize)> = skip_reasons_1h
        .iter()
        .filter(|(_, count)| **count >= threshold)
        .map(|(reason, count)| (reason.clone(), *count))
        .collect();
    skip_alerts_1h.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    // ── Status cards ─────────────────────────────────────────────────────────
    let status_cards = format!(
        r#"
        <div class="cards">
          <div class="card">
            <div class="label">Mode</div>
            <div class="value">{mode_badge}</div>
          </div>
          <div class="card">
            <div class="label">Balance</div>
            <div class="value">${:.2}</div>
          </div>
          <div class="card">
            <div class="label">Equity</div>
            <div class="value">${:.2}</div>
          </div>
          <div class="card">
            <div class="label">P&amp;L</div>
            <div class="value {pnl_class}">${pnl_str}</div>
          </div>
          <div class="card">
            <div class="label">Realized PnL</div>
            <div class="value">${:.2}</div>
          </div>
          <div class="card">
            <div class="label">Unrealized PnL</div>
            <div class="value">${:.2}</div>
          </div>
          <div class="card">
            <div class="label">Uptime</div>
            <div class="value">{}</div>
          </div>
          <div class="card">
            <div class="label">Whales tracked</div>
            <div class="value">{}</div>
          </div>
          <div class="card">
            <div class="label">Trades</div>
            <div class="value">{}</div>
          </div>
          <div class="card">
            <div class="label">Skipped (1h)</div>
            <div class="value">{}</div>
          </div>
          <div class="card">
            <div class="label">Skipped (24h)</div>
            <div class="value">{}</div>
          </div>
          <div class="card">
            <div class="label">Skipped (Total)</div>
            <div class="value">{}</div>
          </div>
          <div class="card">
            <div class="label">Daily Drawdown</div>
            <div class="value neg">${:.2}</div>
          </div>
          <div class="card">
            <div class="label">Loss Streak</div>
            <div class="value">{}</div>
          </div>
        </div>"#,
        state.balance_usdc,
        state.equity_usdc,
        state.realized_pnl_usdc,
        state.unrealized_pnl_usdc,
        state.uptime(),
        state.whale_count,
        state.trade_count,
        skips_1h,
        skips_24h,
        total_skips,
        state.daily_drawdown_usdc(),
        state.consecutive_loss_events,
    );

    let alert_section = if skip_alerts_1h.is_empty() {
        String::new()
    } else {
        let alert_rows: String = skip_alerts_1h
            .iter()
            .take(SKIP_SPIKE_TOP_REASONS)
            .map(|(reason, count)| {
                format!(
                    r#"<li><span class="mono">{reason}</span>: <strong>{count}</strong>/h</li>"#
                )
            })
            .collect();
        format!(
            r#"<section>
    <div class="alert-panel">
      <div class="alert-title">Skip Pressure Alert (1h)</div>
      <p>One or more skip reasons crossed the threshold ({threshold}/h).</p>
      <ul>{alert_rows}</ul>
    </div>
  </section>"#
        )
    };

    // ── Whale table ───────────────────────────────────────────────────────────
    let whale_rows: String = whales
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let addr = &w.address;
            let short_addr = truncate_middle(addr, 6, 4);
            let win_pct = format!("{:.1}%", w.win_rate * 100.0);
            let conservative_pct = format!("{:.1}%", w.conservative_win_rate * 100.0);
            let score = format!("{:.4}", w.score);
            let pnl_disp = w.period_pnl.to_string();
            let wins = w.wins;
            let resolved = w.resolved_trade_count;
            let active = w.active_asset_ids.len();
            format!(
                r#"<tr>
                  <td class="center">#{rank}</td>
                  <td class="mono"><a href="https://polymarket.com/profile/{addr}" target="_blank" rel="noopener noreferrer" title="{addr}" style="color:var(--accent)">{short_addr}</a></td>
                  <td class="center">{score}</td>
                  <td class="center">{win_pct}</td>
                  <td class="center">{conservative_pct}</td>
                  <td class="right">${pnl_disp}</td>
                  <td class="center">{wins}</td>
                  <td class="center">{resolved}</td>
                  <td class="center">{active}</td>
                </tr>"#,
                rank = i + 1,
            )
        })
        .collect();

    let whale_section = if whales.is_empty() {
        r#"<p class="empty">No whales yet — waiting for the first investigation cycle…</p>"#
            .to_string()
    } else {
        format!(
            r#"<table>
              <thead>
                <tr>
                  <th>Rank</th><th>Address</th><th>Score</th>
                  <th>Win Rate</th><th>Conservative Win</th><th>Period PnL</th>
                  <th>Wins</th><th>Trades</th><th>Active Markets</th>
                </tr>
              </thead>
              <tbody>{whale_rows}</tbody>
            </table>"#
        )
    };

    // ── Trade history ─────────────────────────────────────────────────────────
    let trade_rows: String = state
        .recent_trades
        .iter()
        .take(200)
        .map(|t| {
            let short_whale = truncate_middle(&t.whale_address, 6, 4);
            let short_market = truncate_middle(&t.market, 6, 4);
            let side_class = if t.side == "Buy" { "pos" } else { "neg" };
            let outcome_class = if t.outcome == "YES" { "pos" } else { "neg" };
            let sim_tag = if t.simulated { " 📋" } else { "" };
            format!(
                r#"<tr>
              <td class="mono">{time}</td>
              <td class="mono"><a href="https://polymarket.com/profile/{whale_addr}" target="_blank" rel="noopener noreferrer" title="{whale_addr}" style="color:var(--accent)">{short_whale}</a></td>
              <td class="mono"><a href="https://polymarket.com/event/{market}" target="_blank" rel="noopener noreferrer" title="{market}" style="color:var(--accent)">{short_market}</a></td>
              <td class="{side_class} center">{side}{sim_tag}</td>
              <td class="{outcome_class} center">{outcome}</td>
              <td class="right">{price:.4}</td>
              <td class="right">{eff_price:.4}</td>
              <td class="right">${size:.2}</td>
              <td class="right">{shares:.4}</td>
              <td class="right">{kelly:.2}%</td>
            </tr>"#,
                time = t.timestamp.format("%H:%M:%S"),
                whale_addr = t.whale_address,
                market = t.market,
                side = t.side,
                outcome = t.outcome,
                price = t.price,
                eff_price = t.effective_price,
                size = t.size_usdc,
                shares = t.shares,
                kelly = t.kelly_fraction * 100.0,
            )
        })
        .collect();

    let trade_section = if state.recent_trades.is_empty() {
        r#"<p class="empty">No trades yet — the bot is watching for whale activity…</p>"#
            .to_string()
    } else {
        format!(
            r#"<table>
              <thead>
                <tr>
                  <th>Time</th><th>Whale</th><th>Market</th>
                  <th>Side</th><th>Outcome</th><th>Price</th><th>Eff. Price</th>
                  <th>Size (USDC)</th><th>Shares</th><th>Kelly %</th>
                </tr>
              </thead>
              <tbody>{trade_rows}</tbody>
            </table>"#
        )
    };

    let mut all_reasons: HashSet<String> = state.skip_reasons.keys().cloned().collect();
    all_reasons.extend(skip_reasons_1h.keys().cloned());
    all_reasons.extend(skip_reasons_24h.keys().cloned());

    let mut skip_reason_rows: Vec<(String, usize, usize, usize)> = all_reasons
        .into_iter()
        .map(|reason| {
            let total = state.skip_reasons.get(&reason).copied().unwrap_or(0);
            let count_24h = skip_reasons_24h.get(&reason).copied().unwrap_or(0);
            let count_1h = skip_reasons_1h.get(&reason).copied().unwrap_or(0);
            (reason, total, count_24h, count_1h)
        })
        .collect();
    skip_reason_rows.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| b.2.cmp(&a.2))
            .then_with(|| b.3.cmp(&a.3))
            .then_with(|| a.0.cmp(&b.0))
    });

    let skip_rows_html: String = skip_reason_rows
        .iter()
        .take(20)
        .map(|(reason, total, count_24h, count_1h)| {
            format!(
                r#"<tr>
                  <td class="mono">{reason}</td>
                  <td class="right">{total}</td>
                  <td class="right">{count_24h}</td>
                  <td class="right">{count_1h}</td>
                </tr>"#
            )
        })
        .collect();

    let skip_section = if skip_reason_rows.is_empty() {
        r#"<p class="empty">No skipped signals recorded yet.</p>"#.to_string()
    } else {
        format!(
            r#"<table>
              <thead>
                <tr>
                  <th>Reason</th><th>Total</th><th>24h</th><th>1h</th>
                </tr>
              </thead>
              <tbody>{skip_rows_html}</tbody>
            </table>"#
        )
    };

    // ── Open Positions section ────────────────────────────────────────────────
    let positions_section = {
        let mut pos_rows: Vec<(String, f64, f64, f64, f64, f64, f64)> = state
            .market_positions
            .iter()
            .filter(|(_, p)| p.yes_shares > 0.0 || p.no_shares > 0.0)
            .map(|(market, p)| {
                let yes_price = state
                    .last_yes_prices
                    .get(market)
                    .copied()
                    .unwrap_or(0.5)
                    .clamp(0.0001, 0.9999);
                let no_price = 1.0 - yes_price;
                let mark_value = p.yes_shares * yes_price + p.no_shares * no_price;
                let cost_basis = p.yes_cost_usdc + p.no_cost_usdc;
                let unr_pnl = mark_value - cost_basis;
                (
                    market.clone(),
                    p.yes_shares,
                    p.yes_cost_usdc,
                    p.no_shares,
                    p.no_cost_usdc,
                    yes_price,
                    unr_pnl,
                )
            })
            .collect();

        pos_rows.sort_by(|a, b| {
            b.6.abs()
                .partial_cmp(&a.6.abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        if pos_rows.is_empty() {
            r#"<p class="empty">No open positions.</p>"#.to_string()
        } else {
            let rows: String = pos_rows
                .iter()
                .map(|(market, yes_sh, yes_cost, no_sh, no_cost, yes_price, unr_pnl)| {
                    let short_market = truncate_middle(market, 6, 4);
                    let pnl_class = if *unr_pnl >= 0.0 { "pos" } else { "neg" };
                    let pnl_str = format!("{:+.2}", unr_pnl);
                    format!(
                        r#"<tr>
                      <td class="mono"><a href="https://polymarket.com/event/{market}" target="_blank" rel="noopener noreferrer" title="{market}" style="color:var(--accent)">{short_market}</a></td>
                      <td class="right">{yes_sh:.4}</td>
                      <td class="right">${yes_cost:.2}</td>
                      <td class="right">{no_sh:.4}</td>
                      <td class="right">${no_cost:.2}</td>
                      <td class="center">{yes_price:.4}</td>
                      <td class="right {pnl_class}">${pnl_str}</td>
                    </tr>"#,
                    )
                })
                .collect();
            format!(
                r#"<table>
              <thead>
                <tr>
                  <th>Market</th>
                  <th>YES Shares</th><th>YES Cost</th>
                  <th>NO Shares</th><th>NO Cost</th>
                  <th>Mark Price</th><th>Unr. P&amp;L</th>
                </tr>
              </thead>
              <tbody>{rows}</tbody>
            </table>"#
            )
        }
    };

    drop(state);
    drop(whales);

    Html(format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta http-equiv="refresh" content="{DASHBOARD_REFRESH_SECS}">
  <title>Copytrading Bot Dashboard</title>
    <style>
    :root {{
      --bg: #0f1117; --surface: #1a1d27; --border: #2d3047;
      --text: #e2e8f0; --muted: #8892a4; --accent: #6c63ff;
      --pos: #48bb78; --neg: #fc8181; --dry: #f6ad55; --live: #68d391;
    }}
    * {{ box-sizing: border-box; margin: 0; padding: 0; }}
    body {{ background: var(--bg); color: var(--text); font-family: 'Segoe UI', system-ui, sans-serif; padding: 1.5rem; }}
    header {{ display: flex; align-items: center; gap: 1rem; margin-bottom: 1.5rem; border-bottom: 1px solid var(--border); padding-bottom: 1rem; }}
    header h1 {{ font-size: 1.4rem; font-weight: 600; }}
    .refresh {{ margin-left: auto; font-size: 0.75rem; color: var(--muted); }}
    .cards {{ display: grid; grid-template-columns: repeat(auto-fit, minmax(150px, 1fr)); gap: 1rem; margin-bottom: 1.5rem; }}
    .card {{ background: var(--surface); border: 1px solid var(--border); border-radius: 10px; padding: 1rem; }}
    .card .label {{ font-size: 0.75rem; color: var(--muted); text-transform: uppercase; letter-spacing: 0.05em; margin-bottom: 0.4rem; }}
    .card .value {{ font-size: 1.3rem; font-weight: 600; }}
    section {{ margin-bottom: 2rem; }}
    section h2 {{ font-size: 1rem; font-weight: 600; color: var(--muted); text-transform: uppercase; letter-spacing: 0.05em; margin-bottom: 0.75rem; }}
    table {{ width: 100%; border-collapse: collapse; background: var(--surface); border: 1px solid var(--border); border-radius: 8px; overflow: hidden; font-size: 0.875rem; }}
    th {{ background: var(--border); padding: 0.6rem 0.75rem; text-align: left; font-weight: 600; font-size: 0.75rem; text-transform: uppercase; letter-spacing: 0.05em; color: var(--muted); }}
    td {{ padding: 0.55rem 0.75rem; border-top: 1px solid var(--border); }}
    tr:hover td {{ background: #222637; }}
    .mono {{ font-family: monospace; font-size: 0.8rem; }}
    .center {{ text-align: center; }}
    .right {{ text-align: right; }}
    .pos {{ color: var(--pos); }}
    .neg {{ color: var(--neg); }}
    .badge {{ display: inline-block; padding: 0.25rem 0.6rem; border-radius: 6px; font-size: 0.85rem; font-weight: 600; }}
    .badge.dry {{ background: rgba(246,173,85,0.15); color: var(--dry); }}
    .badge.live {{ background: rgba(104,211,145,0.15); color: var(--live); }}
    .empty {{ color: var(--muted); font-style: italic; padding: 1rem 0; }}
    .alert-panel {{
      background: rgba(252,129,129,0.12);
      border: 1px solid rgba(252,129,129,0.45);
      border-radius: 10px;
      padding: 0.9rem 1rem;
    }}
    .alert-title {{ color: var(--neg); font-weight: 700; margin-bottom: 0.3rem; }}
    .alert-panel p {{ color: var(--text); margin-bottom: 0.4rem; font-size: 0.9rem; }}
    .alert-panel ul {{ margin-left: 1.1rem; }}
    .alert-panel li {{ margin: 0.2rem 0; }}
    footer {{ text-align: center; color: var(--muted); font-size: 0.75rem; margin-top: 2rem; }}
  </style>
</head>
<body>
  <header>
    <h1>🐋 Copytrading Bot</h1>
    <span class="refresh">Auto-refreshes every {DASHBOARD_REFRESH_SECS} s &nbsp;·&nbsp; <a href="/api/state" style="color:var(--accent)">JSON API</a></span>
  </header>

  {status_cards}
  {alert_section}

  <section>
    <h2>Open Positions</h2>
    {positions_section}
  </section>

  <section>
    <h2>Tracked Whales</h2>
    {whale_section}
  </section>

  <section>
    <h2>Recent Trades</h2>
    {trade_section}
  </section>

  <section>
    <h2>Skip Reasons</h2>
    {skip_section}
  </section>

  <footer>copytrading-bot &nbsp;·&nbsp; Data refreshed at {}</footer>
</body>
</html>"#,
        chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC"),
    ))
}
