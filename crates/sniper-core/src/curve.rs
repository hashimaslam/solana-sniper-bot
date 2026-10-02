//! pump.fun bonding curve math.
//!
//! pump.fun prices tokens with a constant-product curve over *virtual*
//! reserves (`x * y = k`). These helpers are pure functions so the executor
//! (buy sizing) and the position manager (mark-to-market for auto-sell) share
//! exactly the same pricing logic.

use crate::error::{Error, Result};

/// Basis-point denominator.
pub const BPS_DENOMINATOR: u64 = 10_000;

/// Default pump.fun trade fee in basis points (1%).
///
/// pump.fun has moved to tiered fees; treat this as an estimate and keep
/// slippage tolerance wide enough to absorb the difference.
pub const DEFAULT_FEE_BPS: u64 = 100;

/// Virtual token reserves of a freshly created pump.fun curve (6 decimals).
pub const INITIAL_VIRTUAL_TOKEN_RESERVES: u64 = 1_073_000_000_000_000;

/// Virtual SOL reserves of a freshly created pump.fun curve (lamports).
pub const INITIAL_VIRTUAL_SOL_RESERVES: u64 = 30_000_000_000;

/// Real token reserves of a freshly created pump.fun curve.
pub const INITIAL_REAL_TOKEN_RESERVES: u64 = 793_100_000_000_000;

/// Minimum length of a bonding curve account we can parse
/// (8 discriminator + 5 * u64 + bool).
pub const BONDING_CURVE_MIN_LEN: usize = 8 + 5 * 8 + 1;

/// Snapshot of a pump.fun bonding curve account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BondingCurveState {
    pub virtual_token_reserves: u64,
    pub virtual_sol_reserves: u64,
    pub real_token_reserves: u64,
    pub real_sol_reserves: u64,
    pub token_total_supply: u64,
    /// `true` once the curve has completed and liquidity migrated.
    pub complete: bool,
}

impl BondingCurveState {
    /// Curve state at token launch, used when the account isn't readable yet
    /// (e.g. we saw the create at `processed` before the RPC node has it).
    pub fn initial() -> Self {
        Self {
            virtual_token_reserves: INITIAL_VIRTUAL_TOKEN_RESERVES,
            virtual_sol_reserves: INITIAL_VIRTUAL_SOL_RESERVES,
            real_token_reserves: INITIAL_REAL_TOKEN_RESERVES,
            real_sol_reserves: 0,
            token_total_supply: 1_000_000_000_000_000,
            complete: false,
        }
    }

    /// Parse raw account data.
    ///
    /// Reads fixed offsets and ignores trailing bytes, because newer versions
    /// of the account append fields (e.g. `creator`) after `complete`.
    pub fn from_account_data(data: &[u8]) -> Result<Self> {
        if data.len() < BONDING_CURVE_MIN_LEN {
            return Err(Error::Decode(format!(
                "bonding curve account too short: {} bytes",
                data.len()
            )));
        }
        let u64_at = |i: usize| {
            let off = 8 + i * 8;
            u64::from_le_bytes(data[off..off + 8].try_into().expect("slice is 8 bytes"))
        };
        Ok(Self {
            virtual_token_reserves: u64_at(0),
            virtual_sol_reserves: u64_at(1),
            real_token_reserves: u64_at(2),
            real_sol_reserves: u64_at(3),
            token_total_supply: u64_at(4),
            complete: data[8 + 5 * 8] != 0,
        })
    }

    /// Tokens received for spending `sol_in` lamports (fee included in `sol_in`).
    pub fn buy_quote(&self, sol_in: u64, fee_bps: u64) -> u64 {
        if sol_in == 0 || self.complete {
            return 0;
        }
        // Fee is charged on top of the SOL that enters the curve.
        let net_sol = (sol_in as u128 * BPS_DENOMINATOR as u128)
            / (BPS_DENOMINATOR as u128 + fee_bps as u128);
        let vsr = self.virtual_sol_reserves as u128;
        let vtr = self.virtual_token_reserves as u128;
        let out = (vtr * net_sol) / (vsr + net_sol);
        // The curve can't hand out more than it actually holds.
        (out as u64).min(self.real_token_reserves)
    }

    /// Lamports received (after fee) for selling `tokens_in`.
    pub fn sell_quote(&self, tokens_in: u64, fee_bps: u64) -> u64 {
        if tokens_in == 0 || self.complete {
            return 0;
        }
        let vsr = self.virtual_sol_reserves as u128;
        let vtr = self.virtual_token_reserves as u128;
        let gross = (vsr * tokens_in as u128) / (vtr + tokens_in as u128);
        let fee = gross * fee_bps as u128 / BPS_DENOMINATOR as u128;
        let net = (gross - fee) as u64;
        net.min(self.real_sol_reserves)
    }

    /// Spot price in lamports per raw token unit.
    pub fn spot_price(&self) -> f64 {
        if self.virtual_token_reserves == 0 {
            return 0.0;
        }
        self.virtual_sol_reserves as f64 / self.virtual_token_reserves as f64
    }
}

/// Apply slippage upward (max cost we accept).
pub fn with_slippage_up(amount: u64, slippage: f64) -> u64 {
    (amount as f64 * (1.0 + slippage.max(0.0))).round() as u64
}

/// Apply slippage downward (minimum output we accept).
pub fn with_slippage_down(amount: u64, slippage: f64) -> u64 {
    (amount as f64 * (1.0 - slippage.clamp(0.0, 1.0))).floor() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account_bytes(state: &BondingCurveState, extra: usize) -> Vec<u8> {
        let mut v = vec![0xAAu8; 8];
        for x in [
            state.virtual_token_reserves,
            state.virtual_sol_reserves,
            state.real_token_reserves,
            state.real_sol_reserves,
            state.token_total_supply,
        ] {
            v.extend_from_slice(&x.to_le_bytes());
        }
        v.push(state.complete as u8);
        v.extend(std::iter::repeat_n(7u8, extra));
        v
    }

    #[test]
    fn parses_account_with_trailing_fields() {
        let s = BondingCurveState::initial();
        let parsed = BondingCurveState::from_account_data(&account_bytes(&s, 32)).unwrap();
        assert_eq!(parsed, s);
    }

    #[test]
    fn rejects_short_account() {
        assert!(BondingCurveState::from_account_data(&[0u8; 20]).is_err());
    }

    #[test]
    fn initial_buy_of_one_sol() {
        // 1 SOL into a fresh curve at 0% fee: 1.073e15 * 1e9 / 31e9 ≈ 34.6M tokens.
        let s = BondingCurveState::initial();
        let out = s.buy_quote(1_000_000_000, 0);
        assert_eq!(out, 34_612_903_225_806);
        // With a 1% fee you get strictly less.
        assert!(s.buy_quote(1_000_000_000, 100) < out);
    }

    #[test]
    fn round_trip_loses_only_fees() {
        let mut s = BondingCurveState::initial();
        let sol_in = 500_000_000;
        let tokens = s.buy_quote(sol_in, 0);
        // Apply the buy to the curve.
        s.virtual_sol_reserves += sol_in;
        s.virtual_token_reserves -= tokens;
        s.real_sol_reserves += sol_in;
        s.real_token_reserves -= tokens;
        let back = s.sell_quote(tokens, 0);
        // Integer rounding only.
        assert!(sol_in - back < 10, "lost {} lamports", sol_in - back);
        let back_with_fee = s.sell_quote(tokens, 100);
        assert!(back_with_fee < back);
    }

    #[test]
    fn complete_curve_quotes_zero() {
        let mut s = BondingCurveState::initial();
        s.complete = true;
        assert_eq!(s.buy_quote(1_000_000_000, 100), 0);
        assert_eq!(s.sell_quote(1_000_000, 100), 0);
    }

    #[test]
    fn sell_capped_by_real_sol() {
        let s = BondingCurveState::initial(); // real_sol_reserves == 0
        assert_eq!(s.sell_quote(1_000_000_000, 0), 0);
    }

    #[test]
    fn slippage_helpers() {
        assert_eq!(with_slippage_up(1_000, 0.15), 1_150);
        assert_eq!(with_slippage_down(1_000, 0.15), 850);
        assert_eq!(with_slippage_down(1_000, 2.0), 0);
    }
}
