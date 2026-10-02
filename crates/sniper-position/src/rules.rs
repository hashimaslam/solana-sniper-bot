//! Exit rules for open positions.
//!
//! Kept as pure functions over plain numbers so every rule is unit-testable
//! without a chain, an RPC, or a clock.

use serde::{Deserialize, Serialize};
use sniper_core::config::PositionConfig;

/// Why a position is being closed.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    TakeProfit { pnl_pct: f64 },
    StopLoss { pnl_pct: f64 },
    TrailingStop { drawdown_pct: f64 },
    Timeout { held_secs: u64 },
    /// Bonding curve completed (migrated); can no longer sell into it.
    CurveComplete,
    Manual,
}

impl std::fmt::Display for ExitReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExitReason::TakeProfit { pnl_pct } => write!(f, "take-profit ({:+.1}%)", pnl_pct),
            ExitReason::StopLoss { pnl_pct } => write!(f, "stop-loss ({:+.1}%)", pnl_pct),
            ExitReason::TrailingStop { drawdown_pct } => {
                write!(f, "trailing-stop (-{:.1}% from peak)", drawdown_pct)
            }
            ExitReason::Timeout { held_secs } => write!(f, "timeout ({}s)", held_secs),
            ExitReason::CurveComplete => write!(f, "curve complete"),
            ExitReason::Manual => write!(f, "manual"),
        }
    }
}

/// Inputs for one evaluation.
#[derive(Debug, Clone, Copy)]
pub struct Mark {
    /// Lamports spent to open the position
    pub cost_lamports: u64,
    /// Lamports we'd receive selling everything now (after fees/impact)
    pub value_lamports: u64,
    /// Highest `value_lamports` seen since open (including this mark)
    pub peak_value_lamports: u64,
    /// Seconds since the position opened
    pub held_secs: u64,
}

/// % change of `value` over `base`.
pub fn pct_change(base: u64, value: u64) -> f64 {
    if base == 0 {
        return 0.0;
    }
    (value as f64 - base as f64) / base as f64 * 100.0
}

/// Decide whether to exit. Order matters: protective exits (stop-loss,
/// trailing) take precedence over take-profit, and timeout is last so a
/// position at TP on its final tick is still reported as a TP.
pub fn evaluate(cfg: &PositionConfig, m: &Mark) -> Option<ExitReason> {
    let pnl_pct = pct_change(m.cost_lamports, m.value_lamports);

    if pnl_pct <= -cfg.stop_loss_pct {
        return Some(ExitReason::StopLoss { pnl_pct });
    }

    if cfg.trailing_stop_pct > 0.0 && m.peak_value_lamports > m.cost_lamports {
        let drawdown_pct = -pct_change(m.peak_value_lamports, m.value_lamports);
        if drawdown_pct >= cfg.trailing_stop_pct {
            return Some(ExitReason::TrailingStop { drawdown_pct });
        }
    }

    if pnl_pct >= cfg.take_profit_pct {
        return Some(ExitReason::TakeProfit { pnl_pct });
    }

    if cfg.max_hold_secs > 0 && m.held_secs >= cfg.max_hold_secs {
        return Some(ExitReason::Timeout {
            held_secs: m.held_secs,
        });
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> PositionConfig {
        PositionConfig {
            take_profit_pct: 100.0,
            stop_loss_pct: 30.0,
            trailing_stop_pct: 0.0,
            max_hold_secs: 300,
            ..Default::default()
        }
    }

    fn mark(cost: u64, value: u64, peak: u64, held: u64) -> Mark {
        Mark {
            cost_lamports: cost,
            value_lamports: value,
            peak_value_lamports: peak,
            held_secs: held,
        }
    }

    #[test]
    fn holds_inside_band() {
        assert_eq!(evaluate(&cfg(), &mark(100, 150, 150, 10)), None);
        assert_eq!(evaluate(&cfg(), &mark(100, 71, 100, 10)), None);
    }

    #[test]
    fn take_profit_at_threshold() {
        assert!(matches!(
            evaluate(&cfg(), &mark(100, 200, 200, 10)),
            Some(ExitReason::TakeProfit { pnl_pct }) if (pnl_pct - 100.0).abs() < 1e-9
        ));
    }

    #[test]
    fn stop_loss_at_threshold() {
        assert!(matches!(
            evaluate(&cfg(), &mark(100, 70, 100, 10)),
            Some(ExitReason::StopLoss { .. })
        ));
    }

    #[test]
    fn timeout_when_flat() {
        assert_eq!(
            evaluate(&cfg(), &mark(100, 100, 100, 300)),
            Some(ExitReason::Timeout { held_secs: 300 })
        );
        let mut c = cfg();
        c.max_hold_secs = 0;
        assert_eq!(evaluate(&c, &mark(100, 100, 100, 10_000)), None);
    }

    #[test]
    fn take_profit_beats_timeout() {
        assert!(matches!(
            evaluate(&cfg(), &mark(100, 250, 250, 999)),
            Some(ExitReason::TakeProfit { .. })
        ));
    }

    #[test]
    fn trailing_stop_only_once_in_profit() {
        let mut c = cfg();
        c.trailing_stop_pct = 20.0;
        // Peak 180 (+80%), now 140: 22% off peak -> trail.
        assert!(matches!(
            evaluate(&c, &mark(100, 140, 180, 10)),
            Some(ExitReason::TrailingStop { .. })
        ));
        // Never went above cost: trailing doesn't apply, stop-loss governs.
        assert_eq!(evaluate(&c, &mark(100, 80, 100, 10)), None);
        // 10% off peak: hold.
        assert_eq!(evaluate(&c, &mark(100, 162, 180, 10)), None);
    }

    #[test]
    fn zero_cost_never_panics() {
        assert_eq!(pct_change(0, 50), 0.0);
        assert_eq!(evaluate(&cfg(), &mark(0, 0, 0, 1)), None);
    }
}
