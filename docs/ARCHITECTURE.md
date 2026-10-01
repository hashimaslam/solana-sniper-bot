# Architecture

## Overview

The Solana Sniper Bot is designed as a modular, high-performance system for detecting and executing token snipes on Solana. The architecture prioritizes:

- **Low latency**: Direct WebSocket/Geyser connections for real-time data
- **Modularity**: Separate crates for each concern
- **Extensibility**: Trait-based design for adding new pool types
- **Safety**: Simulation before execution, configurable risk filters

## System Flow

```
┌─────────────┐     ┌─────────────┐     ┌─────────────┐     ┌─────────────┐
│   Listener  │────▶│   Decoder   │────▶│  Analyzer   │────▶│  Executor   │
│             │     │             │     │             │     │             │
│ • RPC WS    │     │ • pump.fun  │     │ • Blacklist │     │ • Build TX  │
│ • Geyser    │     │ • Raydium   │     │ • Liquidity │     │ • Sign      │
│             │     │ • SPL Token │     │ • Age check │     │ • Submit    │
└─────────────┘     └─────────────┘     └─────────────┘     └─────────────┘
       │                   │                   │                   │
       └───────────────────┴───────────────────┴───────────────────┘
                                    │
                            ┌───────▼───────┐
                            │  Shared State │
                            │   (DashMap)   │
                            └───────────────┘
```

## Crate Dependencies

```
sniper-cli
    ├── sniper-core
    ├── sniper-listener ─── sniper-core
    ├── sniper-decoder ──── sniper-core
    ├── sniper-analyzer ─── sniper-core
    └── sniper-executor ─── sniper-core
```

## Component Details

### sniper-core

Foundation crate providing:
- `Config`: TOML configuration loading and validation
- `Error`/`Result`: Error types with `thiserror`
- Types: `Pool`, `ParsedTransaction`, `AnalysisResult`, `ExecutionResult`
- Program IDs: pump.fun, Raydium, SPL Token, etc.

### sniper-listener

Transaction streaming via:
- **RPC WebSocket**: Standard `logsSubscribe` with program filters
- **Geyser gRPC** (future): Direct validator connection for <50ms latency

The `Listener` trait allows swapping backends:

```rust
#[async_trait]
pub trait Listener: Send + Sync {
    async fn start(&self) -> Result<mpsc::Receiver<TransactionEvent>>;
    async fn stop(&self) -> Result<()>;
    fn is_running(&self) -> bool;
}
```

### sniper-decoder

Instruction decoding for supported DEXes:
- **pump.fun**: Create, Buy, Sell instructions
- **Raydium** (planned): Initialize, Swap instructions

The `Decoder` trait enables adding new DEX support:

```rust
#[async_trait]
pub trait Decoder: Send + Sync {
    fn program_id(&self) -> Pubkey;
    async fn decode(&self, tx: &ParsedTransaction) -> Result<Vec<DecodedInstruction>>;
    async fn fetch_pool(&self, address: &Pubkey) -> Result<Pool>;
}
```

### sniper-analyzer

Strategy engine for snipe decisions:
- Blacklist/whitelist filtering
- Liquidity thresholds
- Pool age limits
- Token risk analysis (mint/freeze authority)
- Confidence scoring

Configuration-driven rules:

```toml
[strategy]
max_buy_sol = 0.1
min_liquidity_sol = 1.0
max_pool_age_secs = 60
slippage = 0.15
```

### sniper-executor

Transaction building and submission:
- Compute budget instructions
- Priority fees
- ATA creation
- Transaction signing
- Simulation before execution
- Confirmation polling

The `Executor` trait:

```rust
#[async_trait]
pub trait Executor: Send + Sync {
    async fn execute_buy(
        &self,
        pool: &Pool,
        amount_sol: f64,
        slippage: f64,
    ) -> Result<ExecutionResult>;
    async fn simulate(&self, pool: &Pool, amount_sol: f64, slippage: f64) -> Result<()>;
}
```

### sniper-cli

Orchestrates all components:
- `run`: Main sniper loop
- `balance`: Check wallet balance
- `listen`: Pool detection without execution
- `validate`: Configuration validation

## Latency Optimization

| Stage | Target | Method |
|-------|--------|--------|
| Detection | <100ms | Geyser gRPC |
| Decode | <5ms | Borsh deserialize |
| Analysis | <10ms | In-memory checks |
| Execution | <200ms | Priority fees, skip simulation |

## Future Enhancements

1. **Jito Integration**: MEV-protected bundle submission
2. **Raydium Support**: V4 and CPMM pool detection
3. **Multi-wallet**: Parallel execution across wallets
4. **Metrics**: Prometheus/Grafana dashboards
5. **Telegram Alerts**: Real-time notifications
6. **Position Management**: Auto-sell on profit targets
