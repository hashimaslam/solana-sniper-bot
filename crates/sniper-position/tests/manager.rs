//! Position manager tests against an in-memory mock executor.

use async_trait::async_trait;
use chrono::Utc;
use parking_lot::Mutex;
use solana_sdk::{pubkey::Pubkey, signature::Signature};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

use sniper_core::{
    config::PositionConfig, curve, programs, BondingCurveState, Error, ExecutionResult, Pool,
    PoolType, Result,
};
use sniper_executor::Executor;
use sniper_position::{ExitReason, PositionEvent, PositionManager, PositionStatus};

/// Mock executor whose curve the test can move.
struct MockExec {
    curve: Mutex<BondingCurveState>,
    sells: Mutex<Vec<(u64, f64)>>,
    fail_sells: AtomicU32,
}

impl MockExec {
    fn new() -> Arc<Self> {
        let mut c = BondingCurveState::initial();
        c.real_sol_reserves = 50_000_000_000;
        Arc::new(Self {
            curve: Mutex::new(c),
            sells: Mutex::new(vec![]),
            fail_sells: AtomicU32::new(0),
        })
    }

    /// Simulate other traders buying (`+`) or selling (`-`) SOL into the curve.
    fn flow(&self, lamports: i64) {
        let mut c = self.curve.lock();
        if lamports >= 0 {
            let tokens = c.buy_quote(lamports as u64, 0);
            c.virtual_sol_reserves += lamports as u64;
            c.virtual_token_reserves -= tokens;
        } else {
            let sol = (-lamports) as u64;
            // tokens needed to pull `sol` out: inverse of x*y=k
            let k = c.virtual_sol_reserves as u128 * c.virtual_token_reserves as u128;
            let new_sol = c.virtual_sol_reserves - sol;
            c.virtual_token_reserves = (k / new_sol as u128) as u64;
            c.virtual_sol_reserves = new_sol;
        }
    }

    fn sell_count(&self) -> usize {
        self.sells.lock().len()
    }
}

fn result(ok: bool, tokens: Option<u64>, lamports: u64) -> ExecutionResult {
    ExecutionResult {
        signature: Signature::new_unique(),
        success: ok,
        tokens_received: tokens,
        sol_spent_lamports: lamports,
        latency_ms: 1,
        error: (!ok).then(|| "mock failure".to_string()),
        confirmed_slot: ok.then_some(1),
    }
}

#[async_trait]
impl Executor for MockExec {
    async fn execute_buy(&self, _: &Pool, _: f64, _: f64) -> Result<ExecutionResult> {
        unimplemented!()
    }

    async fn execute_sell(&self, _: &Pool, tokens: u64, slippage: f64) -> Result<ExecutionResult> {
        self.sells.lock().push((tokens, slippage));
        if self.fail_sells.load(Ordering::SeqCst) > 0 {
            self.fail_sells.fetch_sub(1, Ordering::SeqCst);
            return Ok(result(false, None, 0));
        }
        let out = self.curve.lock().sell_quote(tokens, curve::DEFAULT_FEE_BPS);
        Ok(result(true, None, out))
    }

    async fn curve_state(&self, _: &Pool) -> Result<BondingCurveState> {
        Ok(*self.curve.lock())
    }

    async fn simulate(&self, _: &Pool, _: f64, _: f64) -> Result<()> {
        Err(Error::Unknown("n/a".into()))
    }

    fn name(&self) -> &'static str {
        "mock"
    }
}

fn pool() -> Pool {
    let curve = Pubkey::new_unique();
    Pool {
        address: curve,
        token_mint: Pubkey::new_unique(),
        quote_mint: *programs::WSOL_MINT,
        pool_type: PoolType::PumpFun,
        initial_liquidity_lamports: 0,
        token_reserve: 0,
        creation_slot: 0,
        created_at: Utc::now(),
        creator: None,
        bonding_curve: Some(curve),
    }
}

fn cfg() -> PositionConfig {
    PositionConfig {
        take_profit_pct: 50.0,
        stop_loss_pct: 20.0,
        trailing_stop_pct: 0.0,
        max_hold_secs: 0,
        poll_interval_ms: 100,
        sell_slippage: 0.25,
        max_sell_retries: 3,
        enabled: true,
    }
}

/// Buy `sol` against the mock curve and open the position.
fn open(mgr: &PositionManager, exec: &MockExec, sol: u64) -> Pubkey {
    let tokens = exec.curve.lock().buy_quote(sol, curve::DEFAULT_FEE_BPS);
    exec.flow(sol as i64);
    let p = pool();
    let mint = p.token_mint;
    assert!(mgr.open(p, &result(true, Some(tokens), sol)));
    mint
}

#[tokio::test]
async fn ignores_failed_buys() {
    let exec = MockExec::new();
    let mgr = PositionManager::new(cfg(), exec.clone());
    assert!(!mgr.open(pool(), &result(false, Some(1), 1)));
    assert!(!mgr.open(pool(), &result(true, Some(0), 1)));
    assert_eq!(mgr.open_count(), 0);
}

#[tokio::test]
async fn holds_then_takes_profit() {
    let exec = MockExec::new();
    let mut mgr = PositionManager::new(cfg(), exec.clone());
    let mut events = mgr.subscribe();
    let mint = open(&mgr, &exec, 1_000_000_000);

    // Fresh position: round-trip fees put it slightly underwater, not at SL.
    mgr.tick().await;
    assert_eq!(exec.sell_count(), 0);
    assert!(mgr.get(&mint).unwrap().pnl_pct() < 0.0);

    // Big inflow pumps the price.
    exec.flow(40_000_000_000);
    mgr.tick().await;
    assert_eq!(exec.sell_count(), 1);
    let pos = mgr.get(&mint).unwrap();
    assert_eq!(pos.status, PositionStatus::Closed);
    assert!(matches!(pos.exit_reason, Some(ExitReason::TakeProfit { .. })));
    assert!(pos.pnl_pct() >= 50.0, "pnl {}", pos.pnl_pct());
    assert_eq!(exec.sells.lock()[0].1, 0.25, "uses sell_slippage");

    assert!(matches!(events.recv().await, Some(PositionEvent::Opened { .. })));
    assert!(matches!(events.recv().await, Some(PositionEvent::Closed { .. })));

    // Closed positions are never sold again.
    mgr.tick().await;
    assert_eq!(exec.sell_count(), 1);
}

#[tokio::test]
async fn stop_loss_on_dump() {
    let exec = MockExec::new();
    let mgr = PositionManager::new(cfg(), exec.clone());
    exec.flow(10_000_000_000); // some prior buys so there's SOL to dump
    let mint = open(&mgr, &exec, 1_000_000_000);
    exec.flow(-8_000_000_000);
    mgr.tick().await;
    let pos = mgr.get(&mint).unwrap();
    assert!(matches!(pos.exit_reason, Some(ExitReason::StopLoss { .. })), "{:?}", pos);
    assert!(pos.pnl_pct() <= -20.0);
}

#[tokio::test]
async fn trailing_stop_after_peak() {
    let exec = MockExec::new();
    let mut c = cfg();
    c.take_profit_pct = 1_000.0;
    c.trailing_stop_pct = 15.0;
    let mgr = PositionManager::new(c, exec.clone());
    let mint = open(&mgr, &exec, 1_000_000_000);

    exec.flow(20_000_000_000);
    mgr.tick().await;
    let peak = mgr.get(&mint).unwrap().peak_value_lamports;
    assert!(peak > 1_000_000_000);
    assert_eq!(exec.sell_count(), 0);

    exec.flow(-6_000_000_000);
    mgr.tick().await;
    let pos = mgr.get(&mint).unwrap();
    assert!(
        matches!(pos.exit_reason, Some(ExitReason::TrailingStop { .. })),
        "{:?}",
        pos
    );
    // Trailed out still in profit.
    assert!(pos.pnl_pct() > 0.0);
}

#[tokio::test(start_paused = true)]
async fn timeout_exit() {
    let exec = MockExec::new();
    let mut c = cfg();
    c.max_hold_secs = 30;
    let mgr = PositionManager::new(c, exec.clone());
    let mint = open(&mgr, &exec, 1_000_000_000);

    tokio::time::advance(Duration::from_secs(29)).await;
    mgr.tick().await;
    assert_eq!(exec.sell_count(), 0);

    tokio::time::advance(Duration::from_secs(1)).await;
    mgr.tick().await;
    assert_eq!(
        mgr.get(&mint).unwrap().exit_reason,
        Some(ExitReason::Timeout { held_secs: 30 })
    );
}

#[tokio::test]
async fn retries_then_gives_up() {
    let exec = MockExec::new();
    let mut mgr = PositionManager::new(cfg(), exec.clone());
    let mut events = mgr.subscribe();
    let mint = open(&mgr, &exec, 1_000_000_000);
    exec.fail_sells.store(10, Ordering::SeqCst);
    exec.flow(40_000_000_000);

    for _ in 0..5 {
        mgr.tick().await;
    }
    // max_sell_retries = 3 -> exactly 3 attempts, then Failed.
    assert_eq!(exec.sell_count(), 3);
    assert_eq!(mgr.get(&mint).unwrap().status, PositionStatus::Failed);

    let _opened = events.recv().await;
    for i in 1..=3 {
        match events.recv().await {
            Some(PositionEvent::SellFailed { attempt, gave_up, .. }) => {
                assert_eq!(attempt, i);
                assert_eq!(gave_up, i == 3);
            }
            other => panic!("unexpected {:?}", other),
        }
    }
}

#[tokio::test]
async fn retry_succeeds_after_transient_failure() {
    let exec = MockExec::new();
    let mgr = PositionManager::new(cfg(), exec.clone());
    let mint = open(&mgr, &exec, 1_000_000_000);
    exec.fail_sells.store(1, Ordering::SeqCst);
    exec.flow(40_000_000_000);
    mgr.tick().await;
    assert_eq!(mgr.get(&mint).unwrap().status, PositionStatus::Open);
    mgr.tick().await;
    assert_eq!(mgr.get(&mint).unwrap().status, PositionStatus::Closed);
    assert_eq!(exec.sell_count(), 2);
}

#[tokio::test]
async fn curve_completion_flags_position() {
    let exec = MockExec::new();
    let mgr = PositionManager::new(cfg(), exec.clone());
    let mint = open(&mgr, &exec, 1_000_000_000);
    exec.curve.lock().complete = true;
    mgr.tick().await;
    let pos = mgr.get(&mint).unwrap();
    assert_eq!(pos.status, PositionStatus::Failed);
    assert_eq!(pos.exit_reason, Some(ExitReason::CurveComplete));
    assert_eq!(exec.sell_count(), 0);
}

#[tokio::test]
async fn manual_close() {
    let exec = MockExec::new();
    let mgr = PositionManager::new(cfg(), exec.clone());
    let mint = open(&mgr, &exec, 1_000_000_000);
    mgr.close(&mint).await.unwrap();
    assert_eq!(mgr.get(&mint).unwrap().exit_reason, Some(ExitReason::Manual));
}

#[tokio::test(start_paused = true)]
async fn run_loop_sells_and_shuts_down() {
    let exec = MockExec::new();
    let mgr = Arc::new(PositionManager::new(cfg(), exec.clone()));
    let mint = open(&mgr, &exec, 1_000_000_000);
    let (stop_tx, stop_rx) = watch::channel(false);
    let handle = tokio::spawn(mgr.clone().run(stop_rx));

    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(exec.sell_count(), 0);
    exec.flow(40_000_000_000);
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(mgr.get(&mint).unwrap().status, PositionStatus::Closed);

    stop_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .expect("run() exits on shutdown")
        .unwrap();
}
