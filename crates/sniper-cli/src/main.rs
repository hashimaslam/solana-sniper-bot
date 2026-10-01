//! # Sniper CLI
//!
//! Command-line interface for the Solana sniper bot.

use clap::{Parser, Subcommand};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::signature::{read_keypair_file, Keypair, Signer};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{error, info, Level};
use tracing_subscriber::EnvFilter;

use sniper_analyzer::StrategyAnalyzer;
use sniper_core::Config;
use sniper_decoder::{DecodedInstruction, Decoder, PumpFunDecoder};
use sniper_executor::{PumpFunExecutor, transaction::Executor};
use sniper_listener::{Listener, RpcListener, TransactionEvent};

#[derive(Parser)]
#[command(name = "sniper")]
#[command(about = "High-performance Solana token sniping bot", long_about = None)]
struct Cli {
    /// Path to configuration file
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,

    /// Verbosity level
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the sniper bot
    Run {
        /// Dry run mode (simulate only, don't execute)
        #[arg(long)]
        dry_run: bool,
    },
    /// Show wallet balance and info
    Balance,
    /// Validate configuration file
    Validate,
    /// Show detected pools (listen-only mode)
    Listen {
        /// Duration to listen in seconds
        #[arg(short, long, default_value = "60")]
        duration: u64,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Set up logging
    let log_level = match cli.verbose {
        0 => Level::INFO,
        1 => Level::DEBUG,
        _ => Level::TRACE,
    };

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive(format!("sniper={}", log_level).parse().unwrap())
                .add_directive("hyper=warn".parse().unwrap())
                .add_directive("reqwest=warn".parse().unwrap()),
        )
        .init();

    // Load configuration
    let config = Config::load(&cli.config)?;
    info!(config_path = ?cli.config, "Configuration loaded");

    match cli.command {
        Commands::Run { dry_run } => run(config, dry_run).await,
        Commands::Balance => balance(config).await,
        Commands::Validate => validate(config),
        Commands::Listen { duration } => listen(config, duration).await,
    }
}

/// Run the sniper bot.
async fn run(config: Config, dry_run: bool) -> anyhow::Result<()> {
    info!(dry_run, "Starting sniper bot");

    // Initialize RPC client
    let rpc_client = Arc::new(RpcClient::new(config.rpc.endpoint.clone()));

    // Load wallet
    let wallet = Arc::new(load_keypair(&config.wallet.keypair_path)?);
    info!(wallet = %wallet.pubkey(), "Wallet loaded");

    // Check balance
    let balance = rpc_client.get_balance(&wallet.pubkey()).await?;
    let balance_sol = balance as f64 / 1_000_000_000.0;
    info!(balance_sol, "Wallet balance");

    if balance_sol < config.strategy.max_buy_sol {
        error!(
            balance = balance_sol,
            required = config.strategy.max_buy_sol,
            "Insufficient balance"
        );
        return Err(anyhow::anyhow!("Insufficient balance"));
    }

    // Initialize components
    let listener = RpcListener::new(config.rpc.clone());
    let decoder = Arc::new(PumpFunDecoder::new(rpc_client.clone()));
    let analyzer = Arc::new(StrategyAnalyzer::new(config.strategy.clone(), rpc_client.clone()));
    let executor = Arc::new(PumpFunExecutor::new(
        rpc_client.clone(),
        wallet.clone(),
        config.execution.clone(),
    ));

    // Start listener
    let mut rx = listener.start().await?;
    info!("Listener started, waiting for transactions...");

    // Main event loop
    while let Some(event) = rx.recv().await {
        match event {
            TransactionEvent::Connected => {
                info!("Connected to RPC WebSocket");
            }
            TransactionEvent::Disconnected(reason) => {
                info!(reason, "Disconnected, will reconnect");
            }
            TransactionEvent::Transaction(tx) => {
                // Decode transaction
                let instructions = match decoder.decode(&tx).await {
                    Ok(ixs) => ixs,
                    Err(e) => {
                        tracing::debug!(error = %e, "Failed to decode transaction");
                        continue;
                    }
                };

                // Process pool creations
                for ix in instructions {
                    if let DecodedInstruction::PoolCreation(event) = ix {
                        let pool = event.pool;
                        info!(
                            pool = %pool.address,
                            mint = %pool.token_mint,
                            pool_type = %pool.pool_type,
                            "New pool detected"
                        );

                        // Analyze
                        let analysis = match analyzer.analyze(&pool).await {
                            Ok(a) => a,
                            Err(e) => {
                                error!(error = %e, "Analysis failed");
                                continue;
                            }
                        };

                        if !analysis.should_snipe {
                            info!(
                                reasons = ?analysis.reasons,
                                "Pool rejected by strategy"
                            );
                            continue;
                        }

                        info!(
                            confidence = analysis.confidence,
                            amount = analysis.recommended_amount_sol,
                            "Pool approved, executing snipe"
                        );

                        if dry_run {
                            info!("Dry run mode, skipping execution");
                            continue;
                        }

                        // Execute
                        let result = executor
                            .execute_buy(
                                &pool,
                                analysis.recommended_amount_sol,
                                config.strategy.slippage,
                            )
                            .await?;

                        if result.success {
                            info!(
                                signature = %result.signature,
                                tokens = result.tokens_received,
                                latency_ms = result.latency_ms,
                                "Snipe successful!"
                            );
                        } else {
                            error!(
                                error = ?result.error,
                                latency_ms = result.latency_ms,
                                "Snipe failed"
                            );
                        }
                    }
                }
            }
            TransactionEvent::Stopped => {
                info!("Listener stopped");
                break;
            }
        }
    }

    Ok(())
}

/// Show wallet balance.
async fn balance(config: Config) -> anyhow::Result<()> {
    let rpc_client = RpcClient::new(config.rpc.endpoint.clone());
    let wallet = load_keypair(&config.wallet.keypair_path)?;

    let balance = rpc_client.get_balance(&wallet.pubkey()).await?;
    let balance_sol = balance as f64 / 1_000_000_000.0;

    println!("Wallet: {}", wallet.pubkey());
    println!("Balance: {:.9} SOL", balance_sol);

    Ok(())
}

/// Validate configuration.
fn validate(config: Config) -> anyhow::Result<()> {
    config.validate()?;
    println!("Configuration is valid!");
    println!();
    println!("RPC Endpoint: {}", config.rpc.endpoint);
    println!("Wallet: {}", config.wallet.keypair_path);
    println!("Max Buy: {} SOL", config.strategy.max_buy_sol);
    println!("Min Liquidity: {} SOL", config.strategy.min_liquidity_sol);
    println!("Slippage: {}%", config.strategy.slippage * 100.0);
    println!("pump.fun: {}", if config.strategy.pump_fun_enabled { "enabled" } else { "disabled" });
    println!("Raydium: {}", if config.strategy.raydium_enabled { "enabled" } else { "disabled" });

    Ok(())
}

/// Listen for pools without executing.
async fn listen(config: Config, duration: u64) -> anyhow::Result<()> {
    info!(duration_secs = duration, "Starting listen-only mode");

    let rpc_client = Arc::new(RpcClient::new(config.rpc.endpoint.clone()));
    let listener = RpcListener::new(config.rpc.clone());
    let decoder = Arc::new(PumpFunDecoder::new(rpc_client.clone()));

    let mut rx = listener.start().await?;

    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(duration);
    let mut pool_count = 0;

    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                info!(pools_detected = pool_count, "Listen duration expired");
                break;
            }
            event = rx.recv() => {
                match event {
                    Some(TransactionEvent::Transaction(tx)) => {
                        if let Ok(instructions) = decoder.decode(&tx).await {
                            for ix in instructions {
                                if let DecodedInstruction::PoolCreation(event) = ix {
                                    pool_count += 1;
                                    println!(
                                        "[{}] Pool #{}: {} | Mint: {} | Type: {}",
                                        chrono::Utc::now().format("%H:%M:%S"),
                                        pool_count,
                                        event.pool.address,
                                        event.pool.token_mint,
                                        event.pool.pool_type
                                    );
                                }
                            }
                        }
                    }
                    Some(TransactionEvent::Connected) => {
                        println!("Connected to RPC WebSocket");
                    }
                    Some(TransactionEvent::Disconnected(reason)) => {
                        println!("Disconnected: {}", reason);
                    }
                    Some(TransactionEvent::Stopped) | None => {
                        break;
                    }
                }
            }
        }
    }

    listener.stop().await?;
    println!("\nTotal pools detected: {}", pool_count);

    Ok(())
}

/// Load a keypair from file.
fn load_keypair(path: &str) -> anyhow::Result<Keypair> {
    read_keypair_file(path).map_err(|e| anyhow::anyhow!("Failed to load keypair from {}: {}", path, e))
}
