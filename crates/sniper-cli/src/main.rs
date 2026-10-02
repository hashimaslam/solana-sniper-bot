//! # Sniper CLI
//!
//! Command-line interface for the Solana sniper bot.

use clap::{Parser, Subcommand};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::signature::{read_keypair_file, Keypair, Signer};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::watch;
use tracing::{debug, error, info, warn, Level};
use tracing_subscriber::EnvFilter;

use sniper_analyzer::StrategyAnalyzer;
use sniper_core::{config::parse_commitment, Config};
use sniper_decoder::{DecodedInstruction, Decoder, PumpFunDecoder};
use sniper_executor::{
    Executor, JitoClient, JitoSender, PumpFunExecutor, RaceSender, RpcSender, TxSender,
};
use sniper_listener::{GeyserListener, Listener, RpcListener, TransactionEvent};
use sniper_position::{PositionEvent, PositionManager};

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

/// Pick the fastest configured listener: Geyser if set, else RPC WebSocket.
fn make_listener(config: &Config) -> Box<dyn Listener> {
    match &config.geyser {
        Some(g) => {
            info!(endpoint = %g.endpoint, commitment = %g.commitment, "Using Geyser gRPC listener");
            Box::new(GeyserListener::new(g.clone()))
        }
        None => {
            info!("Using RPC WebSocket listener (configure [geyser] for lower latency)");
            Box::new(RpcListener::new(config.rpc.clone()))
        }
    }
}

/// Build the executor with the configured transaction sender.
fn make_executor(
    config: &Config,
    rpc_client: Arc<RpcClient>,
    wallet: Arc<Keypair>,
) -> anyhow::Result<PumpFunExecutor> {
    let exec = PumpFunExecutor::new(rpc_client.clone(), wallet, config.execution.clone());
    let ex = &config.execution;
    if !ex.jito_enabled {
        return Ok(exec);
    }
    let jito = JitoClient::new(ex.jito_endpoint.as_deref(), ex.jito_auth_uuid.clone())?;
    let jito: Arc<dyn TxSender> = Arc::new(JitoSender::new(jito, ex.jito_tip_lamports));
    info!(tip_lamports = ex.jito_tip_lamports, race_rpc = ex.jito_also_send_rpc, "Jito bundles enabled");
    let sender: Arc<dyn TxSender> = if ex.jito_also_send_rpc {
        Arc::new(RaceSender::new(jito, vec![Arc::new(RpcSender::new(rpc_client))]))
    } else {
        jito
    };
    Ok(exec.with_sender(sender))
}

/// Log position events (hook point for alerts).
fn spawn_position_logger(mut rx: tokio::sync::mpsc::UnboundedReceiver<PositionEvent>) {
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            match ev {
                PositionEvent::Opened { mint, tokens, cost_lamports } => {
                    info!(%mint, tokens, cost_sol = lamports_to_sol(cost_lamports), "[position] opened");
                }
                PositionEvent::Closed { mint, reason, cost_lamports, proceeds_lamports } => {
                    let pnl = proceeds_lamports as i128 - cost_lamports as i128;
                    info!(
                        %mint,
                        %reason,
                        pnl_sol = pnl as f64 / 1e9,
                        "[position] closed"
                    );
                }
                PositionEvent::SellFailed { mint, attempt, error, gave_up } => {
                    error!(%mint, attempt, gave_up, error, "[position] sell failed");
                }
            }
        }
    });
}

fn lamports_to_sol(l: u64) -> f64 {
    l as f64 / 1e9
}

/// Run the sniper bot.
async fn run(config: Config, dry_run: bool) -> anyhow::Result<()> {
    info!(dry_run, "Starting sniper bot");

    let rpc_client = Arc::new(RpcClient::new_with_commitment(
        config.rpc.endpoint.clone(),
        CommitmentConfig {
            commitment: parse_commitment(&config.rpc.commitment)?,
        },
    ));

    let wallet = Arc::new(load_keypair(&config.wallet.keypair_path)?);
    info!(wallet = %wallet.pubkey(), "Wallet loaded");

    let balance_sol = lamports_to_sol(rpc_client.get_balance(&wallet.pubkey()).await?);
    info!(balance_sol, "Wallet balance");
    if !dry_run && balance_sol < config.strategy.max_buy_sol {
        error!(balance = balance_sol, required = config.strategy.max_buy_sol, "Insufficient balance");
        return Err(anyhow::anyhow!("Insufficient balance"));
    }

    let listener = make_listener(&config);
    let decoder = Arc::new(PumpFunDecoder::new(rpc_client.clone()));
    let analyzer = Arc::new(StrategyAnalyzer::new(config.strategy.clone(), rpc_client.clone()));
    let executor: Arc<dyn Executor> = Arc::new(make_executor(&config, rpc_client.clone(), wallet.clone())?);

    // Position manager runs on its own task.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let positions = if config.position.enabled {
        let mut mgr = PositionManager::new(config.position.clone(), executor.clone());
        spawn_position_logger(mgr.subscribe());
        let mgr = Arc::new(mgr);
        tokio::spawn(mgr.clone().run(shutdown_rx));
        Some(mgr)
    } else {
        warn!("Position management disabled: bought tokens will NOT be sold automatically");
        None
    };

    let mut rx = listener.start().await?;
    info!(listener = listener.name(), "Listener started, waiting for transactions...");

    loop {
        let event = tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl-C received, shutting down");
                break;
            }
            ev = rx.recv() => match ev {
                Some(ev) => ev,
                None => break,
            },
        };

        let tx = match event {
            TransactionEvent::Connected => {
                info!("Listener connected");
                continue;
            }
            TransactionEvent::Disconnected(reason) => {
                warn!(reason, "Listener disconnected, will reconnect");
                continue;
            }
            TransactionEvent::Stopped => {
                info!("Listener stopped");
                break;
            }
            TransactionEvent::Transaction(tx) => tx,
        };

        let instructions = match decoder.decode(&tx).await {
            Ok(ixs) => ixs,
            Err(e) => {
                debug!(error = %e, "Failed to decode transaction");
                continue;
            }
        };

        for ix in instructions {
            let DecodedInstruction::PoolCreation(event) = ix else { continue };
            let pool = event.pool;
            info!(pool = %pool.address, mint = %pool.token_mint, pool_type = %pool.pool_type, "New pool detected");

            let analysis = match analyzer.analyze(&pool).await {
                Ok(a) => a,
                Err(e) => {
                    error!(error = %e, "Analysis failed");
                    continue;
                }
            };
            if !analysis.should_snipe {
                info!(reasons = ?analysis.reasons, "Pool rejected by strategy");
                continue;
            }
            info!(confidence = analysis.confidence, amount = analysis.recommended_amount_sol, "Pool approved");

            if dry_run {
                info!("Dry run mode, skipping execution");
                continue;
            }

            // Execute off the event loop so detection keeps flowing.
            let executor = executor.clone();
            let positions = positions.clone();
            let slippage = config.strategy.slippage;
            tokio::spawn(async move {
                match executor.execute_buy(&pool, analysis.recommended_amount_sol, slippage).await {
                    Ok(r) if r.success => {
                        info!(signature = %r.signature, tokens = r.tokens_received, latency_ms = r.latency_ms, "Snipe successful");
                        if let Some(p) = positions {
                            p.open(pool, &r);
                        }
                    }
                    Ok(r) => error!(error = ?r.error, latency_ms = r.latency_ms, "Snipe failed"),
                    Err(e) => error!(error = %e, "Snipe errored"),
                }
            });
        }
    }

    listener.stop().await?;
    let _ = shutdown_tx.send(true);
    if let Some(p) = &positions {
        let open = p.open_count();
        if open > 0 {
            warn!(open, "Exiting with open positions; they are NOT sold on shutdown");
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
    println!(
        "Listener: {}",
        config.geyser.as_ref().map(|g| format!("Geyser ({})", g.endpoint)).unwrap_or_else(|| "RPC WebSocket".into())
    );
    if config.execution.jito_enabled {
        println!("Sender: Jito (tip {} lamports)", config.execution.jito_tip_lamports);
    } else {
        println!("Sender: RPC (priority fee {} µlamports/CU)", config.execution.priority_fee_microlamports);
    }
    let p = &config.position;
    if p.enabled {
        println!(
            "Auto-sell: TP +{}% | SL -{}% | trailing {} | max hold {}",
            p.take_profit_pct,
            p.stop_loss_pct,
            if p.trailing_stop_pct > 0.0 { format!("{}%", p.trailing_stop_pct) } else { "off".into() },
            if p.max_hold_secs > 0 { format!("{}s", p.max_hold_secs) } else { "off".into() },
        );
    } else {
        println!("Auto-sell: disabled");
    }

    Ok(())
}

/// Listen for pools without executing.
async fn listen(config: Config, duration: u64) -> anyhow::Result<()> {
    info!(duration_secs = duration, "Starting listen-only mode");

    let rpc_client = Arc::new(RpcClient::new(config.rpc.endpoint.clone()));
    let listener = make_listener(&config);
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
                        println!("Connected ({})", listener.name());
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
