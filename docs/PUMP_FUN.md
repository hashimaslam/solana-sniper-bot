# pump.fun Protocol

## Overview

pump.fun is a token launchpad on Solana that uses bonding curves for price discovery. Tokens are created with a fixed supply and traded against a virtual AMM until reaching a market cap threshold, at which point liquidity migrates to Raydium.

## Program ID

```
6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P
```

## Instruction Discriminators

| Instruction | Discriminator (hex) | Description |
|-------------|---------------------|-------------|
| Create | `181ec828051c0777` | Create new bonding curve |
| Buy | `66063d1201daebea` | Buy tokens from curve |
| Sell | `33e685a4017f83ad` | Sell tokens to curve |
| Withdraw | `b712469c946da122` | Migrate to Raydium |

## Account Layout

### Create Instruction

| Index | Account | Writable | Signer | Description |
|-------|---------|----------|--------|-------------|
| 0 | Mint | ✓ | ✗ | Token mint |
| 1 | Mint Authority | ✗ | ✗ | PDA |
| 2 | Bonding Curve | ✓ | ✗ | Bonding curve PDA |
| 3 | Associated Bonding Curve | ✓ | ✗ | Token account for curve |
| 4 | Global | ✗ | ✗ | Global state PDA |
| 5 | MPL Token Metadata | ✗ | ✗ | Metaplex program |
| 6 | Metadata | ✓ | ✗ | Token metadata account |
| 7 | User | ✓ | ✓ | Creator wallet |
| 8 | System Program | ✗ | ✗ | |
| 9 | Token Program | ✗ | ✗ | |
| 10 | ATA Program | ✗ | ✗ | |
| 11 | Rent | ✗ | ✗ | |

### Buy Instruction

| Index | Account | Writable | Signer | Description |
|-------|---------|----------|--------|-------------|
| 0 | Global | ✗ | ✗ | Global state |
| 1 | Fee Recipient | ✓ | ✗ | pump.fun treasury |
| 2 | Mint | ✗ | ✗ | Token mint |
| 3 | Bonding Curve | ✓ | ✗ | Bonding curve PDA |
| 4 | Associated Bonding Curve | ✓ | ✗ | Curve's token account |
| 5 | Associated User | ✓ | ✗ | User's token account |
| 6 | User | ✓ | ✓ | Buyer wallet |
| 7 | System Program | ✗ | ✗ | |
| 8 | Token Program | ✗ | ✗ | |
| 9 | Rent | ✗ | ✗ | |
| 10 | Event Authority | ✗ | ✗ | PDA for events |
| 11 | Program | ✗ | ✗ | pump.fun program |

## PDA Derivation

### Bonding Curve

```rust
Pubkey::find_program_address(
    &[b"bonding-curve", mint.as_ref()],
    &PUMP_FUN_PROGRAM
)
```

### Global State

```rust
Pubkey::find_program_address(
    &[b"global"],
    &PUMP_FUN_PROGRAM
)
```

### Event Authority

```rust
Pubkey::find_program_address(
    &[b"__event_authority"],
    &PUMP_FUN_PROGRAM
)
```

## Bonding Curve Data

```rust
struct BondingCurveData {
    discriminator: [u8; 8],
    virtual_token_reserves: u64,
    virtual_sol_reserves: u64,
    real_token_reserves: u64,
    real_sol_reserves: u64,
    token_total_supply: u64,
    complete: bool,
}
```

## Price Calculation

The bonding curve uses constant product formula:

```
token_out = (virtual_token_reserves * sol_in) / (virtual_sol_reserves + sol_in)
```

Initial state:
- Virtual SOL reserves: ~30 SOL
- Virtual token reserves: 1B tokens
- Real reserves: 0

## Fee Structure

- 1% fee on all trades
- Fee recipient: `CebN5WGQ4jvEPvsVU4EoHEpgzq1VV7AbicfhtW4xC9iM`

## Migration

When bonding curve reaches ~$69k market cap (~85 SOL raised):
- `complete` flag set to true
- Liquidity migrates to Raydium
- LP tokens burned (locked forever)

## Snipe Strategy

1. **Detection**: Subscribe to pump.fun program logs
2. **Filter**: Look for `Create` instruction discriminator
3. **Extract**: Parse mint, bonding curve, creator from accounts
4. **Analyze**: Check creator history, metadata, token authorities
5. **Execute**: Build and send `Buy` instruction immediately
6. **Timing**: First few seconds critical for best entry
