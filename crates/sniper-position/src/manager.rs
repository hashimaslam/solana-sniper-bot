//! Position tracking and the auto-sell loop.

use dashmap::DashMap;
use serde::Serialize;
use solana_sdk::pubkey::Pubkey;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

use sniper_core::{config::PositionConfig, curve, ExecutionResult, Pool, Result};
use sniper_executor::Executor;

use crate::rules::{self, ExitReason, Mark};

/// Lifecycle of a position.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionStatus {
    Open,
    /// A sell is in flight; skip it on other ticks.
    Closing,
    Closed,
    /// Sells kept failing; needs manual attention.
    Failed,
}

/// One open (or recently closed) position.
#[derive(Debug, Clone)]
pub struct Position {
    pub pool: Pool,
    pub token_amount: u64,
    pub cost_lamports: u64,
    pub opened_at: Instant,
    pub peak_value_lamports: u64,
    pub last_value_lamports: u64,
    pub status: PositionStatus,
    pub sell_attempts: u32,
    pub exit_reason: Option<ExitReason>,
    pub proceeds_lamports: Option<u64>,
}

impl Position {
    pub fn pnl_pct(&self) -> f64 {
        let v = self.proceeds_lamports.unwrap_or(self.last_value_lamports);
        rules::pct_change(self.cost_lamports, v)
    }
}

/// Events emitted for logging / alerting.
#[derive(Debug, Clone)]
pub enum PositionEvent {
    Opened {
        mint: Pubkey,
        tokens: u64,
        cost_lamports: u64,
    },
    Closed {
        mint: Pubkey,
        reason: ExitReason,
        cost_lamports: u64,
        proceeds_lamports: u64,
    },
    SellFailed {
        mint: Pubkey,
        attempt: u32,
        error: String,
        gave_up: bool,
    },
}

/// Tracks positions and sells them according to [`PositionConfig`].
pub struct PositionManager {
    cfg: PositionConfig,
    executor: Arc<dyn Executor>,
    positions: DashMap<Pubkey, Position>,
    events: Option<mpsc::UnboundedSender<PositionEvent>>,
}

impl PositionManager {
    pub fn new(cfg: PositionConfig, executor: Arc<dyn Executor>) -> Self {
        Self {
            cfg,
            executor,
            positions: DashMap::new(),
            events: None,
        }
    }

    /// Receive [`PositionEvent`]s on the returned channel.
    pub fn subscribe(&mut self) -> mpsc::UnboundedReceiver<PositionEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.events = Some(tx);
        rx
    }

    fn emit(&self, ev: PositionEvent) {
        if let Some(tx) = &self.events {
            let _ = tx.send(ev);
        }
    }

    /// Register a filled buy. Ignored if the buy failed or got no tokens.
    pub fn open(&self, pool: Pool, buy: &ExecutionResult) -> bool {
        let tokens = match buy.tokens_received {
            Some(t) if buy.success && t > 0 => t,
            _ => return false,
        };
        let mint = pool.token_mint;
        let cost = buy.sol_spent_lamports;
        info!(mint = %mint, tokens, cost_lamports = cost, "Position opened");
        self.positions.insert(
            mint,
            Position {
                pool,
                token_amount: tokens,
                cost_lamports: cost,
                opened_at: Instant::now(),
                peak_value_lamports: cost,
                last_value_lamports: cost,
                status: PositionStatus::Open,
                sell_attempts: 0,
                exit_reason: None,
                proceeds_lamports: None,
            },
        );
        self.emit(PositionEvent::Opened {
            mint,
            tokens,
            cost_lamports: cost,
        });
        true
    }

    /// Snapshot of all tracked positions.
    pub fn positions(&self) -> Vec<Position> {
        self.positions.iter().map(|p| p.value().clone()).collect()
    }

    pub fn get(&self, mint: &Pubkey) -> Option<Position> {
        self.positions.get(mint).map(|p| p.clone())
    }

    pub fn open_count(&self) -> usize {
        self.positions
            .iter()
            .filter(|p| matches!(p.status, PositionStatus::Open | PositionStatus::Closing))
            .count()
    }

    /// Run until `shutdown` flips to `true`. On shutdown, open positions are
    /// left as-is (not dumped), so a restart doesn't panic-sell.
    pub async fn run(self: Arc<Self>, mut shutdown: watch::Receiver<bool>) {
        let mut tick = tokio::time::interval(Duration::from_millis(self.cfg.poll_interval_ms));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        info!(
            tp = self.cfg.take_profit_pct,
            sl = self.cfg.stop_loss_pct,
            trail = self.cfg.trailing_stop_pct,
            max_hold = self.cfg.max_hold_secs,
            "Position manager started"
        );
        loop {
            tokio::select! {
                _ = tick.tick() => self.tick().await,
                r = shutdown.changed() => {
                    if r.is_err() || *shutdown.borrow() {
                        info!(open = self.open_count(), "Position manager stopping");
                        return;
                    }
                }
            }
        }
    }

    /// Evaluate every open position once. Public for tests and manual driving.
    pub async fn tick(&self) {
        let open: Vec<Pubkey> = self
            .positions
            .iter()
            .filter(|p| p.status == PositionStatus::Open)
            .map(|p| *p.key())
            .collect();

        // Re-price concurrently; each position is independent.
        let futs = open.into_iter().map(|mint| self.check(mint));
        futures::future::join_all(futs).await;
    }

    async fn check(&self, mint: Pubkey) {
        let Some(pos) = self.get(&mint) else { return };

        let curve = match self.executor.curve_state(&pos.pool).await {
            Ok(c) => c,
            Err(e) => {
                debug!(mint = %mint, error = %e, "Failed to price position, will retry");
                return;
            }
        };

        if curve.complete {
            warn!(mint = %mint, "Bonding curve completed; position must be sold on the AMM");
            self.finish(&mint, ExitReason::CurveComplete, None, PositionStatus::Failed);
            return;
        }

        let value = curve.sell_quote(pos.token_amount, curve::DEFAULT_FEE_BPS);
        let peak = pos.peak_value_lamports.max(value);
        let held_secs = pos.opened_at.elapsed().as_secs();

        if let Some(mut p) = self.positions.get_mut(&mint) {
            p.last_value_lamports = value;
            p.peak_value_lamports = peak;
        }

        let mark = Mark {
            cost_lamports: pos.cost_lamports,
            value_lamports: value,
            peak_value_lamports: peak,
            held_secs,
        };
        debug!(
            mint = %mint,
            value,
            pnl_pct = rules::pct_change(pos.cost_lamports, value),
            held_secs,
            "Position marked"
        );

        if let Some(reason) = rules::evaluate(&self.cfg, &mark) {
            self.sell(&mint, reason).await;
        }
    }

    /// Close a position now, regardless of rules.
    pub async fn close(&self, mint: &Pubkey) -> Result<()> {
        if self.get(mint).is_some() {
            self.sell(mint, ExitReason::Manual).await;
        }
        Ok(())
    }

    async fn sell(&self, mint: &Pubkey, reason: ExitReason) {
        // Claim the position so concurrent ticks don't double-sell.
        let pos = {
            let Some(mut p) = self.positions.get_mut(mint) else { return };
            if p.status != PositionStatus::Open {
                return;
            }
            p.status = PositionStatus::Closing;
            p.sell_attempts += 1;
            p.clone()
        };

        info!(mint = %mint, %reason, attempt = pos.sell_attempts, "Selling position");
        let result = self
            .executor
            .execute_sell(&pos.pool, pos.token_amount, self.cfg.sell_slippage)
            .await;

        let err = match result {
            Ok(r) if r.success => {
                self.finish(
                    mint,
                    reason,
                    Some(r.sol_spent_lamports),
                    PositionStatus::Closed,
                );
                return;
            }
            Ok(r) => r.error.unwrap_or_else(|| "unknown sell failure".to_string()),
            Err(e) => e.to_string(),
        };

        let gave_up = pos.sell_attempts >= self.cfg.max_sell_retries.max(1);
        error!(mint = %mint, attempt = pos.sell_attempts, gave_up, error = %err, "Sell failed");
        if let Some(mut p) = self.positions.get_mut(mint) {
            p.status = if gave_up {
                PositionStatus::Failed
            } else {
                PositionStatus::Open // retry on the next tick
            };
        }
        self.emit(PositionEvent::SellFailed {
            mint: *mint,
            attempt: pos.sell_attempts,
            error: err,
            gave_up,
        });
    }

    fn finish(
        &self,
        mint: &Pubkey,
        reason: ExitReason,
        proceeds: Option<u64>,
        status: PositionStatus,
    ) {
        let cost = {
            let Some(mut p) = self.positions.get_mut(mint) else { return };
            p.status = status;
            p.exit_reason = Some(reason);
            p.proceeds_lamports = proceeds;
            p.cost_lamports
        };
        if let Some(proceeds) = proceeds {
            info!(
                mint = %mint,
                %reason,
                cost_lamports = cost,
                proceeds_lamports = proceeds,
                pnl_pct = rules::pct_change(cost, proceeds),
                "Position closed"
            );
            self.emit(PositionEvent::Closed {
                mint: *mint,
                reason,
                cost_lamports: cost,
                proceeds_lamports: proceeds,
            });
        }
    }
}
