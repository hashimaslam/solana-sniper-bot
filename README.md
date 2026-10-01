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
| `sniper-executor` | Transaction building, signing, submission |
| `sniper-cli` | CLI interface and orchestration |

## Features

- **Ultra-low latency**: Direct Geyser gRPC connection (sub-100ms detection)
- **Pump.fun native**: Decodes bonding curve creation and migration events
- **Configurable strategies**: Token age, liquidity thresholds, creator analysis
- **Multi-wallet support**: Parallel execution across wallets
- **Jito integration**: MEV-protected bundle submission

## Quick Start

```bash
# Build
cargo build --release

# Configure
cp config.example.toml config.toml
# Edit config.toml with your RPC, wallet, and strategy settings

# Run
./target/release/sniper-cli run
```

## Configuration

See `config.example.toml` for all options. Key settings:

- `rpc.endpoint`: Solana RPC URL (mainnet-beta)
- `geyser.endpoint`: Geyser gRPC endpoint (optional, faster)
- `wallet.keypair_path`: Path to wallet keypair JSON
- `strategy.max_buy_sol`: Maximum SOL per snipe
- `strategy.min_liquidity`: Minimum pool liquidity to trigger

## Safety

⚠️ **This is experimental software for educational purposes.**

- Never use with funds you cannot afford to lose
- Always test on devnet first
- Sniping carries high risk of rug pulls and failed trades

## License

MIT
