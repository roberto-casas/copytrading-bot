//! Web dashboard — serves a single-page auto-refreshing HTML dashboard at `/`
//! and a JSON API at `/api/state`.
//!
//! ## Endpoints
//! - `GET /`          → Full HTML dashboard (auto-refreshes every 10 s)
//! - `GET /api/state` → Raw JSON of [`BotState`] for external consumers
//! - `GET /api/whales`→ JSON array of current whale profiles

use std::sync::Arc;

use axum::extract::State;
use axum::response::Html;
use axum::routing::get;
use axum::{Json, Router};
use tokio::sync::RwLock;
use tracing::info;

use crate::investigation::scoring::TraderProfile;
use crate::state::{BotState, SharedState};

/// How often the dashboard HTML page auto-refreshes (seconds).
const DASHBOARD_REFRESH_SECS: u32 = 10;

/// Combined state passed to axum handlers.
#[derive(Clone)]
pub struct DashboardState {
    pub bot: SharedState,
    pub whales: Arc<RwLock<Vec<TraderProfile>>>,
}

/// Spawn the dashboard HTTP server on the given port.
pub fn spawn(
    port: u16,
    bot: SharedState,
    whales: Arc<RwLock<Vec<TraderProfile>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let ds = DashboardState { bot, whales };

        let app = Router::new()
            .route("/", get(handle_dashboard))
            .route("/api/state", get(handle_api_state))
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
    Json(serde_json::json!({
        "dry_run": state.dry_run,
        "balance_usdc": state.balance_usdc,
        "initial_balance_usdc": state.initial_balance_usdc,
        "total_pnl_usdc": state.total_pnl(),
        "uptime": state.uptime(),
        "whale_count": state.whale_count,
        "trade_count": state.trade_count,
        "recent_trades": state.recent_trades,
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
                "period_pnl": w.period_pnl.to_string(),
                "volume": w.volume.to_string(),
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

    let mode_badge = if state.dry_run {
        r#"<span class="badge dry">📋 DRY RUN</span>"#
    } else {
        r#"<span class="badge live">⚡ LIVE</span>"#
    };

    let pnl = state.total_pnl();
    let pnl_class = if pnl >= 0.0 { "pos" } else { "neg" };
    let pnl_str = format!("{:+.2}", pnl);

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
            <div class="label">P&amp;L</div>
            <div class="value {pnl_class}">${pnl_str}</div>
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
        </div>"#,
        state.balance_usdc,
        state.uptime(),
        state.whale_count,
        state.trade_count,
    );

    // ── Whale table ───────────────────────────────────────────────────────────
    let whale_rows: String = whales
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let short_addr = format!("{}…{}", &w.address[..6], &w.address[w.address.len() - 4..]);
            let win_pct = format!("{:.1}%", w.win_rate * 100.0);
            let score = format!("{:.4}", w.score);
            let pnl_disp = w.period_pnl.to_string();
            format!(
                r#"<tr>
                  <td class="center">#{}</td>
                  <td class="mono" title="{}">{short_addr}</td>
                  <td class="center">{score}</td>
                  <td class="center">{win_pct}</td>
                  <td class="right">${pnl_disp}</td>
                  <td class="center">{}</td>
                  <td class="center">{}</td>
                </tr>"#,
                i + 1,
                w.address,
                w.resolved_trade_count,
                w.active_asset_ids.len(),
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
                  <th>Win Rate</th><th>Period PnL</th>
                  <th>Trades</th><th>Active Markets</th>
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
        .take(50)
        .map(|t| {
            let short_whale = if t.whale_address.len() >= 10 {
                format!(
                    "{}…{}",
                    &t.whale_address[..6],
                    &t.whale_address[t.whale_address.len() - 4..]
                )
            } else {
                t.whale_address.clone()
            };
            let short_market = if t.market.len() >= 10 {
                format!("{}…{}", &t.market[..6], &t.market[t.market.len() - 4..])
            } else {
                t.market.clone()
            };
            let side_class = if t.side == "Buy" { "pos" } else { "neg" };
            let sim_tag = if t.simulated { " 📋" } else { "" };
            format!(
                r#"<tr>
                  <td class="mono">{}</td>
                  <td class="mono" title="{}">{short_whale}</td>
                  <td class="mono" title="{}">{short_market}</td>
                  <td class="{side_class} center">{}{sim_tag}</td>
                  <td class="right">{:.4}</td>
                  <td class="right">${:.2}</td>
                  <td class="right">{:.2}%</td>
                </tr>"#,
                t.timestamp.format("%H:%M:%S"),
                t.whale_address,
                t.market,
                t.side,
                t.price,
                t.size_usdc,
                t.kelly_fraction * 100.0,
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
                  <th>Side</th><th>Price</th><th>Size (USDC)</th><th>Kelly %</th>
                </tr>
              </thead>
              <tbody>{trade_rows}</tbody>
            </table>"#
        )
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
    footer {{ text-align: center; color: var(--muted); font-size: 0.75rem; margin-top: 2rem; }}
  </style>
</head>
<body>
  <header>
    <h1>🐋 Copytrading Bot</h1>
    <span class="refresh">Auto-refreshes every {DASHBOARD_REFRESH_SECS} s &nbsp;·&nbsp; <a href="/api/state" style="color:var(--accent)">JSON API</a></span>
  </header>

  {status_cards}

  <section>
    <h2>Tracked Whales</h2>
    {whale_section}
  </section>

  <section>
    <h2>Recent Trades</h2>
    {trade_section}
  </section>

  <footer>copytrading-bot &nbsp;·&nbsp; Data refreshed at {}</footer>
</body>
</html>"#,
        chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC"),
    ))
}
