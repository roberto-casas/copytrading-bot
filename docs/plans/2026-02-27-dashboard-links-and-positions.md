# Dashboard Links & Open Positions Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add Polymarket profile/market hyperlinks, an Open Positions section, full trade columns, and bump trade history to 200 rows.

**Architecture:** All changes are confined to `src/dashboard.rs`. The dashboard is a pure server-side HTML string built in `handle_dashboard`. Data already flows in through `DashboardState` — no new API endpoints or state fields needed. Each task is a self-contained edit to one function or string block.

**Tech Stack:** Rust, axum, chrono, serde_json. No new dependencies.

---

### Task 1: Whale table — add profile links

**Files:**
- Modify: `src/dashboard.rs` — `whale_rows` block (lines ~319–347)

**Context:** The whale address column currently shows a truncated address with the full address as a `title` tooltip. We want to wrap it in an `<a>` tag linking to `https://polymarket.com/profile/{full_address}`.

**Step 1: Locate the whale row format string**

Find the `whale_rows` map closure in `handle_dashboard` (~line 319). The relevant `<td>` is:

```rust
<td class="mono" title="{}">{short_addr}</td>
```

where `{}` is `w.address`.

**Step 2: Replace the address `<td>` with a linked version**

Change that `<td>` to:

```rust
<td class="mono"><a href="https://polymarket.com/profile/{}" target="_blank" title="{}" style="color:var(--accent)">{short_addr}</a></td>
```

Both `{}` format args are `w.address` (first for href, second for title).

The full updated `format!` row becomes:

```rust
format!(
    r#"<tr>
      <td class="center">#{}</td>
      <td class="mono"><a href="https://polymarket.com/profile/{}" target="_blank" title="{}" style="color:var(--accent)">{short_addr}</a></td>
      <td class="center">{score}</td>
      <td class="center">{win_pct}</td>
      <td class="center">{conservative_pct}</td>
      <td class="right">${pnl_disp}</td>
      <td class="center">{}</td>
      <td class="center">{}</td>
      <td class="center">{}</td>
    </tr>"#,
    i + 1,
    w.address,   // href
    w.address,   // title
    w.wins,
    w.resolved_trade_count,
    w.active_asset_ids.len(),
)
```

**Step 3: Build and check for compile errors**

```bash
cargo build 2>&1 | head -40
```

Expected: no errors.

**Step 4: Commit**

```bash
git add src/dashboard.rs
git commit -m "feat(dashboard): add Polymarket profile links to whale table"
```

---

### Task 2: Trade history — add market links + full columns

**Files:**
- Modify: `src/dashboard.rs` — `trade_rows` block (lines ~368–413)

**Context:** `TradeRecord` has these fields not currently shown:
- `outcome` ("YES" / "NO")
- `effective_price` (f64)
- `shares` (f64)

The `market` column shows a truncated `condition_id`. We link it to `https://polymarket.com/event/{market}`. The whale column also gets a profile link (same pattern as Task 1).

**Step 1: Update `trade_rows` format string**

Replace the existing `trade_rows` map closure with:

```rust
let trade_rows: String = state
    .recent_trades
    .iter()
    .take(200)                          // was 50 — show full history
    .map(|t| {
        let short_whale = truncate_middle(&t.whale_address, 6, 4);
        let short_market = truncate_middle(&t.market, 6, 4);
        let side_class = if t.side == "Buy" { "pos" } else { "neg" };
        let outcome_class = if t.outcome == "YES" { "pos" } else { "neg" };
        let sim_tag = if t.simulated { " 📋" } else { "" };
        format!(
            r#"<tr>
              <td class="mono">{}</td>
              <td class="mono"><a href="https://polymarket.com/profile/{}" target="_blank" title="{}" style="color:var(--accent)">{short_whale}</a></td>
              <td class="mono"><a href="https://polymarket.com/event/{}" target="_blank" title="{}" style="color:var(--accent)">{short_market}</a></td>
              <td class="{side_class} center">{}{sim_tag}</td>
              <td class="{outcome_class} center">{}</td>
              <td class="right">{:.4}</td>
              <td class="right">{:.4}</td>
              <td class="right">${:.2}</td>
              <td class="right">{:.4}</td>
              <td class="right">{:.2}%</td>
            </tr>"#,
            t.timestamp.format("%H:%M:%S"),
            t.whale_address,    // href
            t.whale_address,    // title
            t.market,           // href
            t.market,           // title
            t.side,
            t.outcome,
            t.price,
            t.effective_price,
            t.size_usdc,
            t.shares,
            t.kelly_fraction * 100.0,
        )
    })
    .collect();
```

**Step 2: Update the trade table `<thead>`**

Change the header row from:

```rust
<th>Time</th><th>Whale</th><th>Market</th>
<th>Side</th><th>Price</th><th>Size (USDC)</th><th>Kelly %</th>
```

to:

```rust
<th>Time</th><th>Whale</th><th>Market</th>
<th>Side</th><th>Outcome</th><th>Price</th><th>Eff. Price</th>
<th>Size (USDC)</th><th>Shares</th><th>Kelly %</th>
```

**Step 3: Build**

```bash
cargo build 2>&1 | head -40
```

Expected: no errors.

**Step 4: Commit**

```bash
git add src/dashboard.rs
git commit -m "feat(dashboard): add market links, outcome/eff-price/shares columns, show all 200 trades"
```

---

### Task 3: Add Open Positions section

**Files:**
- Modify: `src/dashboard.rs` — `handle_dashboard` function body and HTML template string

**Context:** `BotState.market_positions` is a `HashMap<String, MarketPosition>` where `MarketPosition` has `yes_shares`, `yes_cost_usdc`, `no_shares`, `no_cost_usdc`. `BotState.last_yes_prices` gives the latest mark price per market. We compute unrealized P&L per market as:

```
mark_value = yes_shares * yes_price + no_shares * (1 - yes_price)
cost_basis = yes_cost_usdc + no_cost_usdc
unr_pnl    = mark_value - cost_basis
```

The section goes between the status cards and the alert section (i.e., above Recent Trades).

**Step 1: Build the positions HTML block**

Add this code block in `handle_dashboard`, after the `alert_section` block and before `drop(state)`:

```rust
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

    // Sort by absolute unrealized P&L descending (biggest positions first)
    pos_rows.sort_by(|a, b| b.6.abs().partial_cmp(&a.6.abs()).unwrap_or(std::cmp::Ordering::Equal));

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
                      <td class="mono"><a href="https://polymarket.com/event/{}" target="_blank" title="{}" style="color:var(--accent)">{short_market}</a></td>
                      <td class="right">{:.4}</td>
                      <td class="right">${:.2}</td>
                      <td class="right">{:.4}</td>
                      <td class="right">${:.2}</td>
                      <td class="center">{:.4}</td>
                      <td class="right {pnl_class}">${pnl_str}</td>
                    </tr>"#,
                    market, market,
                    yes_sh, yes_cost,
                    no_sh, no_cost,
                    yes_price,
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
```

**Step 2: Insert the section into the HTML template**

In the `Html(format!(...))` block at the bottom, insert the new section between `{status_cards}{alert_section}` and the `<section>` for Recent Trades:

```html
  {status_cards}
  {alert_section}

  <section>
    <h2>Open Positions</h2>
    {positions_section}
  </section>

  <section>
    <h2>Recent Trades</h2>
    {trade_section}
  </section>
```

**Step 3: Build**

```bash
cargo build 2>&1 | head -40
```

Expected: no errors.

**Step 4: Run existing tests to confirm nothing broke**

```bash
cargo test 2>&1 | tail -20
```

Expected: all tests pass.

**Step 5: Commit**

```bash
git add src/dashboard.rs
git commit -m "feat(dashboard): add Open Positions section with per-market mark and unrealized P&L"
```

---

### Task 4: Save design doc and verify

**Step 1: Confirm design doc is committed**

```bash
git add docs/plans/2026-02-27-dashboard-links-and-positions.md
git commit -m "docs: add dashboard links and positions design plan"
```

**Step 2: Final build + test**

```bash
cargo build && cargo test
```

Expected: clean build, all tests green.
