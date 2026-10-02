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
- **Geyser gRPC** (`GeyserListener`): Yellowstone `Subscribe` stream filtered on the watched programs (`account_include`, non-vote, non-failed). Each update carries the full transaction, which is turned into a bincode `VersionedTransaction` plus the resolved account keys (static, then ALT writable, then ALT readonly). Sends the `x-token` auth header, replies to server pings, and reconnects with exponential backoff (0.5s to 30s).
- **RPC WebSocket** (`RpcListener`): `logsSubscribe` fallback. Delivers only the signature, so the decoder fetches the transaction.

The CLI uses Geyser when `[geyser]` is configured. Protos are vendored in `crates/sniper-listener/proto` and compiled by `build.rs`.

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
- Buy/sell quotes from live bonding curve reserves (`sniper_core::curve`)
- Compute budget instructions and priority fees
- Idempotent ATA creation
- Simulation before execution (optional), confirmation polling
- Pluggable `TxSender`:
  - `RpcSender`: `sendTransaction` with preflight skipped
  - `JitoSender`: appends a tip transfer to a random Jito tip account, drops the priority fee, submits via `sendBundle`
  - `RaceSender`: primary sender plus best-effort parallel senders (Jito + RPC)

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
    async fn execute_sell(&self, pool: &Pool, token_amount: u64, slippage: f64) -> Result<ExecutionResult>;
    async fn curve_state(&self, pool: &Pool) -> Result<BondingCurveState>;
    async fn simulate(&self, pool: &Pool, amount_sol: f64, slippage: f64) -> Result<()>;
}
```

### sniper-position

`PositionManager` tracks filled buys and sells them automatically:
- Exit rules (`rules.rs`) are pure functions: stop-loss, trailing stop, take-profit, timeout
- Positions are valued at the full-bag sell quote, so fees and price impact are included
- Re-prices all positions concurrently every `poll_interval_ms`
- A `Closing` state prevents double-sells; failed sells are retried, then marked `Failed`
- Emits `PositionEvent`s (opened / closed / sell failed) for logging or alerts

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

Done in phase 2: Geyser gRPC listener, Jito bundles, position management.

1. **Raydium / PumpSwap Support**: Detect and sell after curve migration
2. **Position persistence**: Survive restarts
3. **Multi-wallet**: Parallel execution across wallets
4. **Metrics**: Prometheus/Grafana dashboards
5. **Telegram Alerts**: Hook into `PositionEvent`
6. **Geyser account subscriptions**: Push curve updates instead of polling RPC for position pricing
