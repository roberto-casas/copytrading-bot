# Copy Trading Strategy Review

## Scope
This review is based on the current implementation in:
- `src/investigation/mod.rs`
- `src/investigation/scoring.rs`
- `src/trading/mod.rs`
- `src/trading/kelly.rs`
- `src/state.rs`
- `src/config.rs`

---

## 1) Current Strategy (as implemented)

### Universe selection
- Pull monthly Polymarket leaderboard candidates (`OVERALL`, `MONTH`).
- Evaluate up to `LEADERBOARD_FETCH_SIZE` candidates (default 50).
- For each candidate:
  - Fetch closed positions, compute win rate (`wins / resolved_count`).
  - Fetch open positions and collect active `asset_id`s.
  - Compute composite score:
    - `0.40 * sigmoid(ROI_30d)`
    - `0.35 * win_rate`
    - `0.25 * sigmoid(volume)`
- Keep top `MAX_WHALES` (default 10), subject to `MIN_RESOLVED_TRADES`.

### Signal generation
- Subscribe to orderbook updates for active whale markets.
- Also run periodic polling (`POLL_INTERVAL_SECS`).
- On check cycle:
  - Pull whale trades with pagination and dedupe via seen trade keys.
  - Skip stale whale profiles (older than 3h).
  - For each new whale trade:
    - Infer effective side/price (sell YES treated as NO via `1 - price`).
    - Estimate edge using whale historical win rate.

### Position sizing and execution
- Kelly sizing:
  - Uses current available balance as bankroll.
  - Applies `KELLY_FRACTION` (fractional Kelly).
  - Enforces `MAX_POSITION_FRACTION`.
- Additional controls:
  - Min trade size: `$1`.
  - Min remaining balance: `$5`.
  - Per-market concentration cap: 30% of bankroll.
- Dry-run: records simulated trade and decrements virtual balance.
- Live: posts market order; only on success records trade and decrements tracked balance.

### State/risk tracking
- Recent trades ring buffer capped (200 records).
- Per-whale seen trade keys capped.
- Inactive whales and stale market exposure entries are pruned.

---

## 2) Validity Assessment

## Verdict
**Concept is directionally valid, but alpha quality is currently fragile.**  
Main issue: your edge estimate (`whale win_rate`) is too weak on its own for execution in fast, fee/slippage-sensitive prediction markets.

## What is good
- Clear pipeline: selection -> signal -> risk controls -> execution.
- Fractional Kelly + position caps are sensible.
- Staleness and duplicate protections are in place.
- Live-mode accounting now avoids false balance reduction on failed orders.

## Critical weaknesses affecting PnL quality
1. **Edge model is under-specified**
- Win rate alone is a poor probability estimator.
- It ignores market odds context, event type, regime, and sample uncertainty.

2. **Selection bias / instability**
- Monthly leaderboard is prone to survivorship and hot-hand effects.
- Score mixes ROI and volume but does not penalize variance/tail risk enough.

3. **Execution drag not modeled**
- Market orders can lose edge quickly due to spread/slippage/latency.
- No explicit expected-value-after-cost check before copying.

4. **Intent ambiguity**
- Whale sells can be hedges/unwinds, not directional alpha.
- Blindly copying all detected trades can import noise.

5. **No robust lifecycle accounting**
- Strategy balance mostly reflects deployed capital, not realized PnL.
- Hard to evaluate true strategy quality without close/resolution tracking.

---

## 3) Recommended Changes (Priority Order)

## P0 (highest impact, do first)
1. **Upgrade edge model from raw win-rate to Bayesian posterior**
- Use Beta-Binomial shrinkage:
  - `p_hat = (wins + a) / (trades + a + b)` with conservative priors.
- Use lower-confidence bound (not mean) for Kelly input.
- Require stronger minimum evidence (for example 50+ resolved trades).

2. **Add net-edge filter before any order**
- Compute expected value net of:
  - fees
  - slippage
  - adverse selection buffer
- Skip if net expected edge <= 0.

3. **Use limit/price-protected execution**
- Replace pure market order behavior with max slippage guardrails.
- Abort if fill price exceeds tolerance.

## P1 (risk-adjusted performance)
4. **Introduce intent filters**
- Prefer copying only:
  - larger notional trades
  - first/opening leg signals
  - non-flip behavior (ignore rapid in/out churn)
- De-prioritize likely hedging/unwind patterns.

5. **Portfolio-level risk budgeting**
- Add daily loss cap, max concurrent exposure, and correlated-event caps.
- Lower per-market concentration from 30% -> **15-20%**.

6. **Kelly hardening**
- Cap `KELLY_FRACTION` to conservative range (0.25-0.5).
- Cap per-position exposure to **5-10%** in live mode by default.

## P2 (model quality / robustness)
7. **Rework whale scoring**
- Score by risk-adjusted metrics:
  - drawdown-adjusted return
  - calibration vs implied odds
  - consistency across market categories and time windows
- Penalize high turnover + unstable performance.

8. **Track realized PnL correctly**
- Persist positions and mark-to-market/settlement outcomes.
- Separate:
  - deployed capital
  - unrealized PnL
  - realized PnL

---

## 4) Practical Parameter Recommendations (starting point)

- `KELLY_FRACTION`: **0.25** (start more conservative).
- `MAX_POSITION_FRACTION`: **0.05 to 0.10**.
- `MAX_MARKET_CONCENTRATION` (code const): **0.15 to 0.20**.
- `MIN_RESOLVED_TRADES`: **>= 50**.
- Add per-whale cooldown after copy execution (for churn control).

---

## 5) Fast Validation Plan (to prove improvement)

1. Build a replay/backtest harness on historical whale trades with realistic latency/slippage.
2. Run walk-forward tests by month (train on prior window, evaluate next window).
3. Compare baseline vs upgraded model on:
- hit rate
- average edge net costs
- max drawdown
- Sharpe-like risk-adjusted returns
- tail loss scenarios
4. Promote to paper trading with production logging before live capital.

---

## Bottom line
Your current implementation is a good engineering base, but not yet a statistically strong trading strategy.  
If you implement **Bayesian edge estimation + net-of-cost execution filter + tighter risk caps**, you should materially improve robustness and reduce false-positive copy trades.
