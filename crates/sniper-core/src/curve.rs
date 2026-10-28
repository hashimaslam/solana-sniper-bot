//! pump.fun bonding curve math.
//!
//! pump.fun prices tokens with a constant-product curve over *virtual*
//! reserves (`x * y = k`). These helpers are pure functions so the executor
//! (buy sizing) and the position manager (mark-to-market for auto-sell) share
//! exactly the same pricing logic.

use crate::error::{Error, Result};

/// Basis-point denominator.
pub const BPS_DENOMINATOR: u64 = 10_000;

/// pump.fun trade fee in basis points: protocol 95 + creator 30.
///
/// Read from the live Pump Fees `fee_config` account (single tier) and
/// confirmed against mainnet `TradeEvent`s. Fees are tiered by market cap in
/// principle, so keep slippage wide enough to absorb a tier change.
pub const DEFAULT_FEE_BPS: u64 = 125;

/// Virtual token reserves of a freshly created pump.fun curve (6 decimals).
pub const INITIAL_VIRTUAL_TOKEN_RESERVES: u64 = 1_073_000_000_000_000;

/// Virtual SOL reserves of a freshly created pump.fun curve (lamports).
pub const INITIAL_VIRTUAL_SOL_RESERVES: u64 = 30_000_000_000;

/// Real token reserves of a freshly created pump.fun curve.
pub const INITIAL_REAL_TOKEN_RESERVES: u64 = 793_100_000_000_000;

/// Minimum length of a bonding curve account we can parse
/// (8 discriminator + 5 * u64 + bool).
pub const BONDING_CURVE_MIN_LEN: usize = 8 + 5 * 8 + 1;

/// Offsets of the fields appended after `complete` in the current layout.
const CREATOR_OFFSET: usize = BONDING_CURVE_MIN_LEN; // 49
const MAYHEM_OFFSET: usize = CREATOR_OFFSET + 32; // 81
const CASHBACK_OFFSET: usize = MAYHEM_OFFSET + 1; // 82
const QUOTE_MINT_OFFSET: usize = CASHBACK_OFFSET + 1; // 83

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
    /// Coin creator (zero on very old accounts).
    pub creator: [u8; 32],
    /// Mayhem mode coin (different fees, no synthetic migration).
    pub is_mayhem_mode: bool,
    /// Cashback coin (v3 trades are rejected for these).
    pub is_cashback_coin: bool,
    /// Quote mint; all-zero means SOL.
    pub quote_mint: [u8; 32],
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
            creator: [0; 32],
            is_mayhem_mode: false,
            is_cashback_coin: false,
            quote_mint: [0; 32],
        }
    }

    /// `true` if the curve is priced in SOL (the only quote we trade).
    pub fn is_sol_quoted(&self) -> bool {
        self.quote_mint == [0; 32]
    }

    /// Parse raw account data.
    ///
    /// Reads fixed offsets. Fields appended after `complete` are read when
    /// present and default to zero/false on older, shorter accounts.
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
        let bytes32 = |off: usize| -> [u8; 32] {
            data.get(off..off + 32)
                .map(|s| s.try_into().expect("slice is 32 bytes"))
                .unwrap_or([0; 32])
        };
        let flag = |off: usize| data.get(off).is_some_and(|b| *b != 0);
        Ok(Self {
            virtual_token_reserves: u64_at(0),
            virtual_sol_reserves: u64_at(1),
            real_token_reserves: u64_at(2),
            real_sol_reserves: u64_at(3),
            token_total_supply: u64_at(4),
            complete: data[8 + 5 * 8] != 0,
            creator: bytes32(CREATOR_OFFSET),
            is_mayhem_mode: flag(MAYHEM_OFFSET),
            is_cashback_coin: flag(CASHBACK_OFFSET),
            quote_mint: bytes32(QUOTE_MINT_OFFSET),
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
        v.extend(std::iter::repeat_n(0u8, extra));
        v
    }

    #[test]
    fn parses_short_legacy_account() {
        let s = BondingCurveState::initial();
        let parsed = BondingCurveState::from_account_data(&account_bytes(&s, 0)).unwrap();
        assert_eq!(parsed, s);
        assert!(parsed.is_sol_quoted());
    }

    /// Real mainnet bonding curve account (166 bytes), captured 2026-10-02.
    /// Curve FjzSWshoB8msrGDpYnREYKYSpxoreQXTy5WvSCu8wq84 (SOL-quoted).
    #[test]
    fn parses_live_mainnet_account() {
        let data = include_bytes!("../tests/fixtures/curve_sol.bin");
        let c = BondingCurveState::from_account_data(data).unwrap();
        assert_eq!(c.virtual_token_reserves, 1_006_145_946_337_782);
        assert_eq!(c.virtual_sol_reserves, 31_993_370_469);
        assert_eq!(c.real_token_reserves, 726_245_946_337_782);
        assert_eq!(c.real_sol_reserves, 1_993_370_469);
        assert!(!c.complete && !c.is_mayhem_mode && !c.is_cashback_coin);
        assert!(c.is_sol_quoted());
        assert_ne!(c.creator, [0; 32]);
        // Virtual SOL = 30 SOL launch + real SOL raised.
        assert_eq!(c.virtual_sol_reserves - c.real_sol_reserves, INITIAL_VIRTUAL_SOL_RESERVES);
    }

    /// Mainnet curve quoted in a pump coin, not SOL.
    #[test]
    fn detects_non_sol_quote() {
        let data = include_bytes!("../tests/fixtures/curve_token_quoted.bin");
        let c = BondingCurveState::from_account_data(data).unwrap();
        assert!(!c.is_sol_quoted());
    }

    /// The fee constant must match what mainnet charged:
    /// sell of 47_501_394 lamports gross paid fee 451_264 + creator 142_505.
    #[test]
    fn fee_matches_mainnet_trade_event() {
        let gross: u64 = 47_501_394;
        let fee = gross * DEFAULT_FEE_BPS / BPS_DENOMINATOR;
        assert!(fee.abs_diff(451_264 + 142_505) <= 2, "{}", fee);
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
