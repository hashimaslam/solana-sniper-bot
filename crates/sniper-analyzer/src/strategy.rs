//! Strategy analyzer implementation.
//!
//! Analyzes pools based on configurable strategy parameters and returns
//! a decision on whether to snipe, along with risk assessment.

use dashmap::DashSet;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use std::sync::Arc;
use tracing::{debug, info, warn};

use sniper_core::{
    config::StrategyConfig, AnalysisResult, Pool, PoolType, Result, RiskFactor, RiskType,
};

/// Strategy analyzer for evaluating pools.
pub struct StrategyAnalyzer {
    config: StrategyConfig,
    rpc_client: Arc<RpcClient>,
    /// Blacklisted mints (parsed from config)
    blacklist: DashSet<Pubkey>,
    /// Whitelisted creators (parsed from config)
    creator_whitelist: DashSet<Pubkey>,
    /// Recently seen pools (deduplication)
    seen_pools: DashSet<Pubkey>,
}

impl StrategyAnalyzer {
    /// Create a new strategy analyzer.
    pub fn new(config: StrategyConfig, rpc_client: Arc<RpcClient>) -> Self {
        let blacklist = DashSet::new();
        for mint_str in &config.blacklist {
            if let Ok(mint) = Pubkey::from_str(mint_str) {
                blacklist.insert(mint);
            }
        }

        let creator_whitelist = DashSet::new();
        for creator_str in &config.creator_whitelist {
            if let Ok(creator) = Pubkey::from_str(creator_str) {
                creator_whitelist.insert(creator);
            }
        }

        Self {
            config,
            rpc_client,
            blacklist,
            creator_whitelist,
            seen_pools: DashSet::new(),
        }
    }

    /// Analyze a pool and determine if it should be sniped.
    pub async fn analyze(&self, pool: &Pool) -> Result<AnalysisResult> {
        let mut reasons = Vec::new();
        let mut risk_factors = Vec::new();
        let mut confidence = 1.0;

        // Check if we've already seen this pool
        if self.seen_pools.contains(&pool.address) {
            return Ok(AnalysisResult {
                should_snipe: false,
                confidence: 0.0,
                recommended_amount_sol: 0.0,
                reasons: vec!["Pool already processed".to_string()],
                risk_factors: vec![],
            });
        }

        // Mark as seen
        self.seen_pools.insert(pool.address);

        // Check pool type is enabled
        match pool.pool_type {
            PoolType::PumpFun if !self.config.pump_fun_enabled => {
                return Ok(AnalysisResult {
                    should_snipe: false,
                    confidence: 0.0,
                    recommended_amount_sol: 0.0,
                    reasons: vec!["pump.fun pools disabled".to_string()],
                    risk_factors: vec![],
                });
            }
            PoolType::RaydiumV4 | PoolType::RaydiumCpmm if !self.config.raydium_enabled => {
                return Ok(AnalysisResult {
                    should_snipe: false,
                    confidence: 0.0,
                    recommended_amount_sol: 0.0,
                    reasons: vec!["Raydium pools disabled".to_string()],
                    risk_factors: vec![],
                });
            }
            _ => {}
        }

        // Check blacklist
        if self.blacklist.contains(&pool.token_mint) {
            warn!(mint = %pool.token_mint, "Token is blacklisted");
            return Ok(AnalysisResult {
                should_snipe: false,
                confidence: 0.0,
                recommended_amount_sol: 0.0,
                reasons: vec!["Token is blacklisted".to_string()],
                risk_factors: vec![RiskFactor {
                    risk_type: RiskType::BlacklistedCreator,
                    severity: 1.0,
                    description: "Token mint is in blacklist".to_string(),
                }],
            });
        }

        // Check creator whitelist (if enabled)
        if !self.creator_whitelist.is_empty() {
            if let Some(creator) = &pool.creator {
                if !self.creator_whitelist.contains(creator) {
                    debug!(creator = %creator, "Creator not in whitelist");
                    return Ok(AnalysisResult {
                        should_snipe: false,
                        confidence: 0.0,
                        recommended_amount_sol: 0.0,
                        reasons: vec!["Creator not in whitelist".to_string()],
                        risk_factors: vec![],
                    });
                }
                reasons.push(format!("Creator {} is whitelisted", creator));
            }
        }

        // Check pool age
        let age_secs = pool.age_secs();
        if age_secs > self.config.max_pool_age_secs as i64 {
            debug!(age = age_secs, max = self.config.max_pool_age_secs, "Pool too old");
            return Ok(AnalysisResult {
                should_snipe: false,
                confidence: 0.0,
                recommended_amount_sol: 0.0,
                reasons: vec![format!(
                    "Pool age {}s exceeds max {}s",
                    age_secs, self.config.max_pool_age_secs
                )],
                risk_factors: vec![RiskFactor {
                    risk_type: RiskType::PoolTooOld,
                    severity: 0.5,
                    description: format!("Pool is {}s old", age_secs),
                }],
            });
        }
        reasons.push(format!("Pool age: {}s (within limit)", age_secs));

        // Check liquidity
        let liquidity_sol = pool.liquidity_sol();
        if liquidity_sol < self.config.min_liquidity_sol {
            debug!(
                liquidity = liquidity_sol,
                min = self.config.min_liquidity_sol,
                "Insufficient liquidity"
            );
            risk_factors.push(RiskFactor {
                risk_type: RiskType::LowLiquidity,
                severity: 0.8,
                description: format!(
                    "Liquidity {:.4} SOL below minimum {:.4} SOL",
                    liquidity_sol, self.config.min_liquidity_sol
                ),
            });
            confidence *= 0.3;
        } else {
            reasons.push(format!("Liquidity: {:.4} SOL", liquidity_sol));
        }

        // Fetch additional on-chain data for risk assessment
        let token_risks = self.analyze_token_risks(&pool.token_mint).await?;
        risk_factors.extend(token_risks.clone());

        // Adjust confidence based on risks
        for risk in &token_risks {
            confidence *= 1.0 - (risk.severity * 0.5);
        }

        // Calculate recommended amount
        let base_amount = self.config.max_buy_sol;
        let adjusted_amount = base_amount * confidence;
        let recommended_amount = adjusted_amount.max(0.01).min(self.config.max_buy_sol);

        // Final decision
        let should_snipe = confidence > 0.5 && risk_factors.iter().all(|r| r.severity < 0.9);

        if should_snipe {
            info!(
                pool = %pool.address,
                mint = %pool.token_mint,
                confidence = confidence,
                amount = recommended_amount,
                "Pool approved for sniping"
            );
        } else {
            debug!(
                pool = %pool.address,
                confidence = confidence,
                risks = ?risk_factors,
                "Pool rejected"
            );
        }

        Ok(AnalysisResult {
            should_snipe,
            confidence,
            recommended_amount_sol: recommended_amount,
            reasons,
            risk_factors,
        })
    }

    /// Analyze token-specific risks.
    async fn analyze_token_risks(&self, mint: &Pubkey) -> Result<Vec<RiskFactor>> {
        let mut risks = Vec::new();

        // Fetch mint account
        match self.rpc_client.get_account(mint).await {
            Ok(account) => {
                // Check if it's a valid SPL token
                if account.owner != spl_token::id() {
                    risks.push(RiskFactor {
                        risk_type: RiskType::SuspiciousMetadata,
                        severity: 1.0,
                        description: "Not owned by SPL Token program".to_string(),
                    });
                    return Ok(risks);
                }

                // Parse mint data
                if account.data.len() >= 82 {
                    // SPL Token Mint layout:
                    // 0-4: mint_authority_option
                    // 4-36: mint_authority
                    // 36-44: supply
                    // 44: decimals
                    // 45: is_initialized
                    // 46-50: freeze_authority_option
                    // 50-82: freeze_authority

                    let mint_authority_option = u32::from_le_bytes(
                        account.data[0..4].try_into().unwrap_or([0; 4])
                    );

                    let freeze_authority_option = u32::from_le_bytes(
                        account.data[46..50].try_into().unwrap_or([0; 4])
                    );

                    // Check mint authority
                    if mint_authority_option == 1 {
                        risks.push(RiskFactor {
                            risk_type: RiskType::MintAuthority,
                            severity: 0.7,
                            description: "Mint authority is still enabled (can mint more tokens)".to_string(),
                        });
                    }

                    // Check freeze authority
                    if freeze_authority_option == 1 {
                        risks.push(RiskFactor {
                            risk_type: RiskType::FreezeAuthority,
                            severity: 0.8,
                            description: "Freeze authority is enabled (can freeze accounts)".to_string(),
                        });
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "Failed to fetch mint account");
                risks.push(RiskFactor {
                    risk_type: RiskType::SuspiciousMetadata,
                    severity: 0.5,
                    description: format!("Could not verify token: {}", e),
                });
            }
        }

        Ok(risks)
    }

    /// Add a mint to the blacklist.
    pub fn add_to_blacklist(&self, mint: Pubkey) {
        self.blacklist.insert(mint);
    }

    /// Clear the seen pools cache.
    pub fn clear_seen(&self) {
        self.seen_pools.clear();
    }

    /// Get the number of pools analyzed.
    pub fn pools_analyzed(&self) -> usize {
        self.seen_pools.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn mock_config() -> StrategyConfig {
        StrategyConfig {
            max_buy_sol: 0.1,
            min_liquidity_sol: 1.0,
            max_pool_age_secs: 60,
            slippage: 0.15,
            pump_fun_enabled: true,
            raydium_enabled: false,
            blacklist: vec![],
            creator_whitelist: vec![],
        }
    }

    #[test]
    fn test_analyzer_creation() {
        let config = mock_config();
        let rpc = Arc::new(RpcClient::new("https://api.mainnet-beta.solana.com".to_string()));
        let analyzer = StrategyAnalyzer::new(config, rpc);
        assert_eq!(analyzer.pools_analyzed(), 0);
    }
}
