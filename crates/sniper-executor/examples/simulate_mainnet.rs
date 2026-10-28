//! Read-only mainnet check of the pump.fun v3 instruction builders.
//!
//! Builds a buy (and sell) for a live bonding curve on behalf of any
//! existing wallet and runs `simulateTransaction` with signature
//! verification off. No private key is used and nothing is sent; the
//! wallet only needs SOL (and tokens, for the sell) to get past the
//! runtime's balance checks.
//!
//! A correct account layout gets past account validation and fails on
//! funds; a wrong one fails with an Anchor account error (e.g. 2006
//! ConstraintSeeds, 3012 AccountNotInitialized).
//!
//! ```text
//! cargo run -p sniper-executor --example simulate_mainnet -- <MINT> <WALLET> [FEE_PAYER] [RPC_URL]
//! ```
//!
//! `FEE_PAYER` (any SOL-funded address) pays fees, so `WALLET` can be a token
//! holder with no SOL. Defaults to `WALLET`.

use solana_client::{nonblocking::rpc_client::RpcClient, rpc_config::RpcSimulateTransactionConfig};
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction, message::Message, pubkey::Pubkey,
    transaction::Transaction,
};
use sniper_core::{programs, BondingCurveState, Pool, PoolType};
use sniper_executor::pump_fun::{self, build_buy_instruction, build_sell_instruction};
use std::str::FromStr;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: simulate_mainnet <MINT> <WALLET> [RPC]";
    let mint = Pubkey::from_str(&args.next().expect(usage))?;
    let user = Pubkey::from_str(&args.next().expect(usage))?;
    let payer = match args.next() {
        Some(p) => Pubkey::from_str(&p)?,
        None => user,
    };
    let rpc_url = args.next().unwrap_or_else(|| "https://api.mainnet-beta.solana.com".into());
    let rpc = RpcClient::new(rpc_url);

    let mint_acct = rpc.get_account(&mint).await?;
    let curve_addr =
        Pubkey::find_program_address(&[b"bonding-curve", mint.as_ref()], &programs::PUMP_FUN_PROGRAM).0;
    let curve = BondingCurveState::from_account_data(&rpc.get_account(&curve_addr).await?.data)?;
    println!("mint owner: {}  curve: {:?}", mint_acct.owner, curve);
    pump_fun::check_tradeable(&curve)?;

    let pool = Pool {
        address: curve_addr,
        token_mint: mint,
        quote_mint: *programs::WSOL_MINT,
        pool_type: PoolType::PumpFun,
        initial_liquidity_lamports: 0,
        token_reserve: 0,
        creation_slot: 0,
        created_at: chrono::Utc::now(),
        creator: None,
        bonding_curve: Some(curve_addr),
        token_program: Some(mint_acct.owner),
        is_mayhem_mode: curve.is_mayhem_mode,
    };

    let ata = spl_associated_token_account::get_associated_token_address_with_program_id(
        &user, &mint, &mint_acct.owner,
    );
    let held: u64 = match rpc.get_token_account_balance(&ata).await {
        Ok(b) => b.amount.parse()?,
        Err(_) => 0,
    };
    println!("wallet {} holds {} tokens", user, held);
    let buyback = Pubkey::from_str(pump_fun::BUYBACK_FEE_RECIPIENTS[0])?;
    let (quote_in, expected, min_out) = pump_fun::buy_params(&curve, 0.01, 0.2);
    println!("buy 0.01 SOL -> expect {} tokens (min {})", expected, min_out);

    let cases = [
        (
            "buy_exact_quote_in_v3",
            vec![
                ComputeBudgetInstruction::set_compute_unit_limit(300_000),
                spl_associated_token_account::instruction::create_associated_token_account_idempotent(
                    &user, &user, &mint, &mint_acct.owner,
                ),
                build_buy_instruction(&user, &pool, quote_in, min_out, buyback)?,
            ],
        ),
        (
            "sell_v3",
            vec![
                ComputeBudgetInstruction::set_compute_unit_limit(300_000),
                build_sell_instruction(&user, &pool, held.clamp(1, 1_000_000_000), 0, buyback)?,
            ],
        ),
    ];

    for (name, ixs) in cases {
        let bh = rpc.get_latest_blockhash().await?;
        // Unsigned: sig_verify is off, so placeholder signatures are fine.
        let mut tx = Transaction::new_unsigned(Message::new(&ixs, Some(&payer)));
        tx.message.recent_blockhash = bh;
        let sim = rpc
            .simulate_transaction_with_config(
                &tx,
                RpcSimulateTransactionConfig {
                    sig_verify: false,
                    replace_recent_blockhash: true,
                    ..Default::default()
                },
            )
            .await?;
        println!("\n== {} -> err: {:?}", name, sim.value.err);
        for l in sim.value.logs.unwrap_or_default().iter().take(25) {
            println!("  {}", l);
        }
    }
    Ok(())
}
