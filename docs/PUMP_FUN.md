# pump.fun Protocol

Reference for how this bot talks to pump.fun. Source of truth: the official IDL and docs at
<https://github.com/pump-fun/pump-public-docs> (`idl/pump.json`, `docs/instructions/*`).
Everything below was also checked against mainnet on 2026-10-02 (see the referenced transactions and tests).

## Overview

pump.fun launches tokens on a constant-product bonding curve over virtual reserves. When the curve's real token reserves run out, the curve is `complete` and liquidity migrates to **PumpSwap** (pump.fun's own AMM), not Raydium.

## Program IDs

| Program | Address |
|---|---|
| pump | `6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P` |
| Pump Fees | `pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ` |

## Instructions the bot uses

| Instruction | Discriminator | Used for |
|---|---|---|
| `create` | `[24,30,200,40,5,28,7,119]` | Detect (legacy SPL Token launches) |
| `create_v2` | `[214,144,76,236,95,139,49,180]` | Detect (Token-2022 launches, all new coins) |
| `buy_exact_quote_in_v3` | `[225,247,80,30,213,179,132,136]` | **Our buys**: spend exact SOL, require min tokens |
| `sell_v3` | `[28,146,222,119,38,196,105,213]` | **Our sells** |

The decoder also recognises `buy`, `sell`, `buy_exact_sol_in`, `buy_v2`, `sell_v2` and `buy_v3` from other traders.

### Why v3

`buy_v3` / `buy_exact_quote_in_v3` / `sell_v3` take 17 accounts instead of 26–27. They charge the same prices and fees; the protocol and creator fees just stay on the curve until swept. They fail with `CashbackCoinNotSupported` (6094) on cashback coins, so the executor skips those.

### v3 accounts (SOL-paired coin)

| # | Account | W | S | Derivation |
|---|---|---|---|---|
| 0 | global | | | PDA `["global"]` = `4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf` |
| 1 | base_mint | | | coin mint |
| 2 | quote_mint | | | WSOL `So111…112` |
| 3 | base_token_program | | | mint owner: Token-2022 for `create_v2` coins, SPL Token for older ones |
| 4 | quote_token_program | | | SPL Token |
| 5 | bonding_curve | ✓ | | PDA `["bonding-curve", mint]` |
| 6 | associated_base_bonding_curve | ✓ | | ATA(curve, mint, base_token_program) |
| 7 | associated_quote_bonding_curve | ✓ | | ATA(curve, WSOL, SPL Token); unused for SOL coins but must be the derived address |
| 8 | user | ✓ | ✓ | trader |
| 9 | associated_base_user | ✓ | | ATA(user, mint, base_token_program). **Must exist**: the bot creates it idempotently before buying |
| 10 | associated_quote_user | ✓ | | ATA(user, WSOL, SPL Token); unused for SOL coins |
| 11 | user_volume_accumulator | ✓ | | PDA `["user_volume_accumulator", user]`, created on first trade (~0.0018 SOL) |
| 12 | fee_config | | | Pump Fees PDA `["fee_config", pump_program_id]` = `8Wf5TiAheLUqBrKXeYg2JtAFFMWtKdG2BSFgqUcPVwTt` |
| 13 | buyback_fee_recipient | ✓ | | one of 8 buyback wallets (picked at random) |
| 14 | system_program | | | |
| 15 | event_authority | | | PDA `["__event_authority"]` |
| 16 | program | | | pump |

Verified by `matches_mainnet_sell_v3_accounts` against mainnet tx `2eyyahVtB6SP8dLgjvmePYXo5H9xCgmW2DamMJ4Lo73uf3QMRtuLGmjd4sZnzgWp9PXyMZ7QgDA8o43WTw9yF4LW`.

### Instruction data

- `buy_exact_quote_in_v3`: `disc | spendable_quote_in: u64 | min_tokens_out: u64 | partial_fill: OptionBool` (we send `[1, 0]` = Some(false))
- `sell_v3`: `disc | amount: u64 | min_sol_output: u64`

pump.fun keeps appending optional trailing arguments, so the decoder reads arguments as a prefix and ignores trailing bytes.

### create / create_v2 accounts used for detection

| | mint | bonding_curve | token_program | creator |
|---|---|---|---|---|
| `create` | 0 | 2 | 9 | arg |
| `create_v2` | 0 | 2 | 7 | arg |

Both start with `name, symbol, uri: String, creator: Pubkey`. `create_v2` follows with `is_mayhem_mode: bool`. Optional remaining accounts 16–18 on `create_v2` set a non-SOL quote mint.

## Bonding curve account

166 bytes today (older accounts are shorter; missing fields read as zero):

| Offset | Field | Type |
|---|---|---|
| 0 | discriminator | `[u8; 8]` |
| 8 | virtual_token_reserves | u64 |
| 16 | virtual_quote_reserves | u64 |
| 24 | real_token_reserves | u64 |
| 32 | real_quote_reserves | u64 |
| 40 | token_total_supply | u64 |
| 48 | complete | bool |
| 49 | creator | Pubkey |
| 81 | is_mayhem_mode | bool |
| 82 | is_cashback_coin | bool |
| 83 | quote_mint | Pubkey (all-zero = SOL) |
| 115.. | creator_fee_bps, fee accumulators, depth, … | (not read) |

Launch reserves: 30 SOL virtual, 1.073B virtual tokens, 793.1M real tokens, 1B supply (6 decimals).

## Pricing and fees

Constant product over virtual reserves:

```
tokens_out = vtr * sol_net / (vsr + sol_net)        sol_net = sol_in / (1 + fee)
sol_out    = vsr * tokens_in / (vtr + tokens_in) * (1 - fee)
```

Fees come from the Pump Fees `fee_config` account, tiered by market cap. Currently there is a **single tier: 95 bps protocol + 30 bps creator = 1.25%**. Confirmed against mainnet `TradeEvent`s (`fee_basis_points = 95`, `creator_fee_basis_points = 30`). Half of the protocol fee goes to the buyback recipient.

## Transaction format

pump.fun launches are often sent as **SIMD-0385 v1 transactions** (version byte `129`), which `solana-sdk` 2.x can't deserialize. `sniper_core::txv1` parses them, and RPC fetches request `maxSupportedTransactionVersion: 1`.

## Mainnet simulation

`cargo run -p sniper-executor --example simulate_mainnet -- <MINT> <WALLET> [FEE_PAYER]` builds our buy and sell for a live curve and runs `simulateTransaction` (no signing, nothing sent). On 2026-10-02:

- Coin `BFBv71Hsgd4Zp5kAVzVExyrRQqqXvWK4rPTwWvLopump` (Token-2022), as holder `GPnucVh9QnWGHyNBS1uFFJhRKVMXJGignRySoWrihokP`: **both `buy_exact_quote_in_v3` (60.6k CU) and `sell_v3` (56.6k CU) succeeded**.
- The simulated sell's `TradeEvent` gave a net of 42,624 lamports; our curve model predicted 42,626 (the program rounds protocol and creator fees separately).

## Snipe flow

1. **Detect** `create` / `create_v2` (Geyser, or `logsSubscribe` + fetch)
2. **Extract** mint, curve, creator, token program, mayhem flag
3. **Analyze**: token authorities (SPL Token or Token-2022), age, blacklist
4. **Buy** with `buy_exact_quote_in_v3`, after an idempotent ATA create
5. **Manage**: re-price from the curve, exit with `sell_v3`
