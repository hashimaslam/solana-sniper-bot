# Solana Sniper Bot

High-performance token sniping bot for Solana, specializing in pump.fun pool detection and instant execution.

## Architecture

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                              Solana Sniper Bot                               │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│  ┌──────────────┐    ┌──────────────┐    ┌──────────────┐    ┌───────────┐ │
│  │   Listener   │───▶│   Decoder    │───▶│   Analyzer   │───▶│  Executor │ │
│  │  (Geyser/RPC)│    │ (SPL/Pump)   │    │  (Strategy)  │    │  (Snipe)  │ │
│  └──────────────┘    └──────────────┘    └──────────────┘    └───────────┘ │
│         │                   │                   │                   │       │
│         ▼                   ▼                   ▼                   ▼       │
│  ┌─────────────────────────────────────────────────────────────────────┐   │
│  │                         Shared State (DashMap)                       │   │
│  └─────────────────────────────────────────────────────────────────────┘   │
│                                      │                                       │
│                                      ▼                                       │
│  ┌─────────────────────────────────────────────────────────────────────┐   │
│  │                    Configuration & Wallet Manager                    │   │
│  └─────────────────────────────────────────────────────────────────────┘   │
│                                                                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

## Components

| Crate | Purpose |
|-------|---------|
| `sniper-core` | Shared types, configuration, error handling |
| `sniper-listener` | Geyser gRPC / RPC WebSocket transaction streaming |
| `sniper-decoder` | SPL Token & pump.fun instruction parsing |
| `sniper-analyzer` | Strategy engine, pool scoring, buy decision |
| `sniper-executor` | Transaction building, signing, submission (RPC / Jito bundles) |
| `sniper-position` | Position tracking and auto-sell (TP / SL / trailing / timeout) |
| `sniper-cli` | CLI interface and orchestration |

## Features

- **Low latency detection**: Yellowstone Geyser gRPC stream with full transactions, so decoding needs no extra RPC round-trip (falls back to RPC `logsSubscribe`)
- **Pump.fun native**: Decodes bonding curve creation, buy, and sell, including v0 transactions with address lookup tables
- **Curve-accurate pricing**: Buy and sell sizes are quoted from live bonding curve reserves
- **Jito bundles**: Tip to a random Jito tip account; optionally race a normal RPC send
- **Auto-sell**: Take-profit, stop-loss, trailing stop, and max hold time, measured on what the position would actually sell for
- **Configurable strategies**: Token age, liquidity thresholds, creator analysis

## Quick Start

```bash
# Build
cargo build --release

# Configure
cp config.example.toml config.toml
# Edit config.toml with your RPC, wallet, and strategy settings

# Run
./target/release/sniper validate     # check config, shows listener/sender/auto-sell
./target/release/sniper listen -d 60 # detect pools only
./target/release/sniper run --dry-run
./target/release/sniper run
```

Building compiles the Geyser protobufs with a vendored `protoc`, so you don't need `protoc` installed.

## Configuration

See `config.example.toml` for all options. Key settings:

- `rpc.endpoint`: Solana RPC URL (mainnet-beta)
- `geyser.endpoint`: Geyser gRPC endpoint (optional, faster)
- `wallet.keypair_path`: Path to wallet keypair JSON
- `strategy.max_buy_sol`: Maximum SOL per snipe
- `strategy.min_liquidity_sol`: Minimum pool liquidity to trigger
- `execution.jito_enabled` / `jito_tip_lamports`: Send buys and sells as Jito bundles
- `position.take_profit_pct` / `stop_loss_pct` / `trailing_stop_pct` / `max_hold_secs`: Auto-sell rules

### Auto-sell

Every tick (`position.poll_interval_ms`), each open position is re-priced as *what selling the whole bag into the curve returns right now*, so fees and price impact are already counted. Rules are checked in this order:

1. **Stop-loss**: value ≤ cost × (1 − `stop_loss_pct`/100)
2. **Trailing stop**: once the position has been in profit, value falls `trailing_stop_pct`% from its peak
3. **Take-profit**: value ≥ cost × (1 + `take_profit_pct`/100)
4. **Timeout**: held for `max_hold_secs`

Failed sells are retried up to `max_sell_retries` times. If the bonding curve completes (the token migrates), the position is flagged for manual handling. Open positions are **not** sold on shutdown.

## Safety

⚠️ **This is experimental software for educational purposes.**

- Never use with funds you cannot afford to lose
- Always test on devnet first
- Sniping carries high risk of rug pulls and failed trades
- pump.fun has added instruction accounts over time (creator vault, volume accumulators). Check the buy/sell account layouts in `sniper-executor/src/pump_fun.rs` against the current IDL before trading real funds
- Positions are held in memory only: if the process restarts, open positions stop being managed

## License

MIT
