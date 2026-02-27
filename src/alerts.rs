//! Outbound alert worker for skip-pressure spikes.

use std::sync::Arc;

use chrono::Utc;
use hmac::{Hmac, Mac};
use reqwest::Client;
use sha2::Sha256;
use tokio::time::{Duration, MissedTickBehavior};
use tracing::{error, info, warn};

use crate::config::Config;
use crate::persistence::{AlertPersistedState, PersistenceStore};
use crate::state::SharedState;

/// Maximum number of top spike reasons included in webhook payloads.
const SKIP_SPIKE_TOP_REASONS: usize = 5;

type HmacSha256 = Hmac<Sha256>;

/// Spawn webhook-based skip-pressure alerting if configured.
pub fn spawn(
    config: Config,
    shared_state: SharedState,
    persistence: Arc<PersistenceStore>,
) -> Option<tokio::task::JoinHandle<()>> {
    let webhook_url = config.skip_alert_webhook_url.clone()?;
    let threshold = config.skip_alert_1h_threshold;
    let cooldown_secs = config.skip_alert_cooldown_secs;
    let eval_interval_secs = config.skip_alert_eval_interval_secs;
    let hmac_secret = config.skip_alert_hmac_secret.clone();
    let max_retries = config.skip_alert_max_retries;
    let backoff_ms = config.skip_alert_retry_backoff_ms;
    let daily_loss_limit_usdc = config.daily_loss_limit_usdc;
    let max_consecutive_losses = config.max_consecutive_losses;
    let max_notional_per_market_usdc = config.max_notional_per_market_usdc;

    Some(tokio::spawn(async move {
        let client = match Client::builder().timeout(Duration::from_secs(10)).build() {
            Ok(c) => c,
            Err(e) => {
                error!("Failed to initialize webhook HTTP client: {e}");
                return;
            }
        };

        let restored = match persistence.load_alert_state() {
            Ok(s) => s.unwrap_or_default(),
            Err(e) => {
                warn!("Failed to load persisted alert state: {e:#}");
                AlertPersistedState::default()
            }
        };
        let mut last_signature = restored.last_signature;
        let mut last_sent_at = restored.last_sent_at;
        let mut last_breaker_signature = restored.last_breaker_signature;

        info!(
            threshold,
            cooldown_secs,
            eval_interval_secs,
            max_retries,
            backoff_ms,
            hmac_enabled = hmac_secret.is_some(),
            "Skip alert webhook worker started"
        );

        let mut ticker = tokio::time::interval(Duration::from_secs(eval_interval_secs));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            ticker.tick().await;

            let (top_spikes, total_skips_1h, total_skips_24h, breaker_payload) = {
                let state = shared_state.read().await;
                let skip_reasons_1h = state.skip_reasons_since(3600);
                let skip_reasons_24h = state.skip_reasons_since(24 * 3600);

                let mut top: Vec<(String, usize)> = skip_reasons_1h
                    .iter()
                    .filter(|(_, count)| **count >= threshold)
                    .map(|(reason, count)| (reason.clone(), *count))
                    .collect();
                top.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                top.truncate(SKIP_SPIKE_TOP_REASONS);

                let skips_1h = skip_reasons_1h.values().copied().sum::<usize>();
                let skips_24h = skip_reasons_24h.values().copied().sum::<usize>();
                let drawdown = state.daily_drawdown_usdc();
                let streak = state.consecutive_loss_events;
                let max_market_notional = state.max_market_notional_usdc();
                let drawdown_active = drawdown >= daily_loss_limit_usdc;
                let streak_active = streak >= max_consecutive_losses;
                let notional_active = max_market_notional >= max_notional_per_market_usdc;
                let breaker_signature = format!(
                    "drawdown:{drawdown_active}|streak:{streak_active}|notional:{notional_active}"
                );
                let breaker_payload = serde_json::json!({
                    "signature": breaker_signature,
                    "states": {
                        "daily_loss_limit": {
                            "active": drawdown_active,
                            "value_usdc": drawdown,
                            "limit_usdc": daily_loss_limit_usdc
                        },
                        "max_consecutive_losses": {
                            "active": streak_active,
                            "value": streak,
                            "limit": max_consecutive_losses
                        },
                        "max_notional_per_market": {
                            "active": notional_active,
                            "value_usdc": max_market_notional,
                            "limit_usdc": max_notional_per_market_usdc
                        }
                    }
                });
                (top, skips_1h, skips_24h, breaker_payload)
            };

            let current_breaker_signature = breaker_payload["signature"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            if current_breaker_signature != last_breaker_signature {
                let transition_payload = serde_json::json!({
                    "type": "breaker_state_change",
                    "timestamp_utc": Utc::now().to_rfc3339(),
                    "breaker": breaker_payload["states"],
                });
                let body = serde_json::to_vec(&transition_payload).unwrap_or_default();
                let delivered = send_with_retry(
                    &client,
                    &webhook_url,
                    hmac_secret.as_deref(),
                    &body,
                    max_retries,
                    backoff_ms,
                )
                .await;
                if delivered {
                    shared_state.write().await.record_alert_result(true, None);
                    last_breaker_signature = current_breaker_signature.clone();
                    if let Err(e) = persistence.save_alert_state(&AlertPersistedState {
                        last_signature: last_signature.clone(),
                        last_sent_at,
                        last_breaker_signature: last_breaker_signature.clone(),
                    }) {
                        warn!("Failed persisting breaker alert state: {e:#}");
                    }
                }
            }

            if top_spikes.is_empty() {
                continue;
            }

            let signature = top_spikes
                .iter()
                .map(|(reason, count)| format!("{reason}:{count}"))
                .collect::<Vec<_>>()
                .join("|");

            let cooldown_elapsed = last_sent_at
                .map(|at| {
                    Utc::now().signed_duration_since(at).num_seconds() >= cooldown_secs as i64
                })
                .unwrap_or(true);
            if signature == last_signature && !cooldown_elapsed {
                continue;
            }

            let payload = serde_json::json!({
                "type": "skip_pressure_alert",
                "timestamp_utc": Utc::now().to_rfc3339(),
                "window": "1h",
                "threshold": threshold,
                "top_reasons": top_spikes.iter().map(|(reason, count)| {
                    serde_json::json!({ "reason": reason, "count_1h": count })
                }).collect::<Vec<_>>(),
                "summary": {
                    "total_skips_1h": total_skips_1h,
                    "total_skips_24h": total_skips_24h,
                }
            });

            let body = match serde_json::to_vec(&payload) {
                Ok(v) => v,
                Err(e) => {
                    let msg = format!("Failed to serialize alert payload: {e}");
                    warn!("{msg}");
                    shared_state
                        .write()
                        .await
                        .record_alert_result(false, Some(msg));
                    continue;
                }
            };

            let (delivered, last_error) = send_with_retry_with_error(
                &client,
                &webhook_url,
                hmac_secret.as_deref(),
                &body,
                max_retries,
                backoff_ms,
            )
            .await;

            if delivered {
                info!(
                    top_reason_count = top_spikes.len(),
                    total_skips_1h, "Skip alert webhook sent"
                );
                shared_state.write().await.record_alert_result(true, None);
                last_signature = signature;
                last_sent_at = Some(Utc::now());
                if let Err(e) = persistence.save_alert_state(&AlertPersistedState {
                    last_signature: last_signature.clone(),
                    last_sent_at,
                    last_breaker_signature: last_breaker_signature.clone(),
                }) {
                    warn!("Failed persisting alert dedupe state: {e:#}");
                }
            } else {
                warn!(
                    retries = max_retries,
                    error = %last_error,
                    "Skip alert webhook delivery failed after retries"
                );
                shared_state
                    .write()
                    .await
                    .record_alert_result(false, Some(last_error));
            }
        }
    }))
}

async fn send_with_retry(
    client: &Client,
    webhook_url: &str,
    hmac_secret: Option<&str>,
    body: &[u8],
    max_retries: usize,
    backoff_ms: u64,
) -> bool {
    let (ok, _) = send_with_retry_with_error(
        client,
        webhook_url,
        hmac_secret,
        body,
        max_retries,
        backoff_ms,
    )
    .await;
    ok
}

async fn send_with_retry_with_error(
    client: &Client,
    webhook_url: &str,
    hmac_secret: Option<&str>,
    body: &[u8],
    max_retries: usize,
    backoff_ms: u64,
) -> (bool, String) {
    let mut last_error = String::new();
    for attempt in 0..=max_retries {
        let mut req = client
            .post(webhook_url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_vec());
        if let Some(secret) = hmac_secret {
            req = req.header("X-CopyBot-Signature", sign_hmac(secret, body));
        }

        match req.send().await {
            Ok(resp) if resp.status().is_success() => return (true, String::new()),
            Ok(resp) => {
                let status = resp.status();
                let response_body = match resp.text().await {
                    Ok(s) => truncate_for_log(&s, 400),
                    Err(e) => format!("<failed reading response body: {e}>"),
                };
                last_error = format!("status={status} body={response_body}");
            }
            Err(e) => {
                last_error = e.to_string();
            }
        }

        if attempt < max_retries {
            let backoff = retry_delay_ms(backoff_ms, attempt);
            tokio::time::sleep(Duration::from_millis(backoff)).await;
        }
    }
    (false, last_error)
}

fn sign_hmac(secret: &str, payload: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .expect("HMAC can initialize with arbitrary key length");
    mac.update(payload);
    let sig = mac.finalize().into_bytes();
    hex::encode(sig)
}

fn retry_delay_ms(base_ms: u64, attempt: usize) -> u64 {
    let exp = 1_u64.checked_shl((attempt as u32).min(12)).unwrap_or(1);
    let jitter = (Utc::now().timestamp_subsec_millis() as u64) % base_ms.max(1);
    base_ms.saturating_mul(exp).saturating_add(jitter)
}

fn truncate_for_log(s: &str, max_chars: usize) -> String {
    let mut out = s.chars().take(max_chars).collect::<String>();
    if s.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}
