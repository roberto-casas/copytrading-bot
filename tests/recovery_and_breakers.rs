#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

#[path = "../src/persistence.rs"]
mod persistence;
#[path = "../src/state.rs"]
mod state;

use chrono::Utc;
use persistence::{AlertPersistedState, PersistenceStore};
use state::{BotState, MarketPosition, TradeRecord};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "copytrading-bot-tests-{}-{}-{}",
        name,
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn sample_trade() -> TradeRecord {
    TradeRecord {
        timestamp: Utc::now(),
        whale_address: "0xabc".to_string(),
        market: "0xmarket".to_string(),
        side: "Buy".to_string(),
        price: 0.45,
        outcome: "YES".to_string(),
        effective_price: 0.45,
        shares: 10.0,
        size_usdc: 4.5,
        kelly_fraction: 0.1,
        simulated: true,
    }
}

#[test]
fn restart_recovery_roundtrip_persists_all_runtime_state() {
    let dir = temp_dir("roundtrip");
    let store = PersistenceStore::new(&dir);
    store.ensure_dir().expect("ensure state dir");

    let mut bot = BotState::new(true, 100.0);
    bot.record_trade(sample_trade());
    bot.record_skip("net_edge_below_threshold");
    bot.record_alert_result(true, None);

    let snap = bot.snapshot();
    store.save_bot_state(&snap).expect("save bot state");

    let mut seen = HashMap::<String, HashSet<String>>::new();
    seen.insert(
        "0xabc".to_string(),
        ["tx-1".to_string(), "tx-2".to_string()]
            .into_iter()
            .collect(),
    );
    store
        .save_seen_trades(&seen)
        .expect("save seen-trades state");

    store
        .save_alert_state(&AlertPersistedState {
            last_signature: "skip:a".to_string(),
            last_sent_at: Some(Utc::now()),
            last_breaker_signature: "drawdown:false|streak:false|notional:false".to_string(),
        })
        .expect("save alert state");

    let loaded_snap = store
        .load_bot_state()
        .expect("load bot state")
        .expect("bot state exists");
    let loaded_seen = store.load_seen_trades().expect("load seen trades");
    let loaded_alert = store
        .load_alert_state()
        .expect("load alert state")
        .expect("alert state exists");

    assert_eq!(loaded_snap.trade_count, 1);
    assert_eq!(loaded_snap.skip_reasons["net_edge_below_threshold"], 1);
    assert!(loaded_seen["0xabc"].contains("tx-1"));
    assert_eq!(loaded_alert.last_signature, "skip:a");
    assert_eq!(
        loaded_alert.last_breaker_signature,
        "drawdown:false|streak:false|notional:false"
    );
}

#[test]
fn corrupted_state_file_is_safely_quarantined() {
    let dir = temp_dir("corrupt");
    let store = PersistenceStore::new(&dir);
    store.ensure_dir().expect("ensure state dir");

    let bad = dir.join("bot_state.json");
    fs::write(&bad, "{ definitely-not-json").expect("write corrupt state");

    let loaded = store.load_bot_state().expect("load should not hard-fail");
    assert!(loaded.is_none());

    let has_quarantine = fs::read_dir(&dir)
        .expect("read dir")
        .filter_map(Result::ok)
        .any(|e| e.file_name().to_string_lossy().contains("corrupt"));
    assert!(has_quarantine);
}

#[test]
fn breaker_metrics_move_with_live_account_snapshots() {
    let mut state = BotState::new(false, 1000.0);

    let mut positions = HashMap::new();
    positions.insert(
        "m1".to_string(),
        MarketPosition {
            yes_shares: 100.0,
            yes_cost_usdc: 60.0,
            no_shares: 0.0,
            no_cost_usdc: 0.0,
        },
    );
    let mut marks = HashMap::new();
    marks.insert("m1".to_string(), 0.60);

    state.apply_live_account_snapshot(positions.clone(), marks.clone(), 60.0, 0.0, 0.0);
    assert_eq!(state.daily_drawdown_usdc(), 0.0);
    assert_eq!(state.consecutive_loss_events, 0);
    assert_eq!(state.max_market_notional_usdc(), 60.0);

    // Simulate a sharp mark-to-market loss from account sync.
    state.apply_live_account_snapshot(positions, marks, 30.0, -30.0, 0.0);
    assert!(state.daily_drawdown_usdc() >= 30.0);
    assert!(state.consecutive_loss_events >= 1);
}
