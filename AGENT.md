# Solana Sniper Bot

## Project Agent

This project is managed with AI assistance. The agent maintains project documentation, follows established patterns, and ensures code quality.

### Development Workflow

1. **Changes**: All code changes are made through atomic commits with clear messages
2. **Testing**: Run `cargo test` before committing
3. **Documentation**: Update docs when adding features
4. **Architecture**: Follow the crate separation pattern

### Code Standards

- Rust 2021 edition
- Use `tracing` for logging
- Use `thiserror` for error types
- Async with `tokio`
- Configuration via TOML

### Crate Structure

| Crate | Purpose |
|-------|---------|
| `sniper-core` | Shared types, config, errors |
| `sniper-listener` | Transaction streaming |
| `sniper-decoder` | Instruction decoding |
| `sniper-analyzer` | Strategy engine |
| `sniper-executor` | Transaction execution |
| `sniper-cli` | CLI interface |

### Quick Commands

```bash
# Build
cargo build --release

# Test
cargo test

# Run
./target/release/sniper run

# Check balance
./target/release/sniper balance

# Listen only (no execution)
./target/release/sniper listen -d 120
```
