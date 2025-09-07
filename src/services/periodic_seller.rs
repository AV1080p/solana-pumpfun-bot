use std::sync::Arc;
use std::time::Duration;
use std::str::FromStr;
use tokio::time::{interval, sleep};
use anyhow::Result;
use colored::Colorize;
use anchor_client::solana_sdk::signature::Signer;

use crate::common::{
    config::AppState,
    logger::Logger,
    cache::BOUGHT_TOKENS,
};
use crate::services::{jupiter::JupiterClient, balance_manager::BalanceManager};

/// Periodic selling service that automatically sells all holdings every 2 minutes
pub struct PeriodicSellerService {
    app_state: Arc<AppState>,
    jupiter_client: JupiterClient,
    balance_manager: BalanceManager,
    logger: Logger,
    enabled: bool,
}

impl PeriodicSellerService {
    /// Create a new periodic selling service
    pub fn new(app_state: Arc<AppState>) -> Self {
        let jupiter_client = JupiterClient::new(app_state.rpc_nonblocking_client.clone());
        let balance_manager = BalanceManager::new(app_state.clone());
        
        // Check if periodic selling is enabled via environment variable
        let enabled = std::env::var("PERIODIC_SELLING_ENABLED")
            .ok()
            .and_then(|v| v.parse::<bool>().ok())
            .unwrap_or(true); // Default to enabled
        
        Self {
            app_state,
            jupiter_client,
            balance_manager,
            logger: Logger::new("[PERIODIC-SELLER] => ".magenta().to_string()),
            enabled,
        }
    }
    
    /// Start the periodic selling service
    pub async fn start_periodic_selling(&self) -> Result<()> {
        if !self.enabled {
            self.logger.log("Periodic selling is disabled".yellow().to_string());
            return Ok(());
        }
        
        // Get selling interval from environment variable (default: 2 minutes)
        let selling_interval_seconds = std::env::var("PERIODIC_SELLING_INTERVAL_SECONDS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(120); // Default to 2 minutes
        
        self.logger.log(format!("Starting periodic selling service (every {} seconds)", selling_interval_seconds).green().to_string());
        
        // Create interval timer
        let mut interval = interval(Duration::from_secs(selling_interval_seconds));
        
        loop {
            // Wait for next tick
            interval.tick().await;
            
            // Perform periodic selling
            if let Err(e) = self.sell_all_holdings().await {
                self.logger.log(format!("Periodic selling failed: {}", e).red().to_string());
            }
        }
    }
    
    /// Sell all holdings from BOUGHT_TOKENS cache
    async fn sell_all_holdings(&self) -> Result<()> {
        // Get all bought tokens from cache
        let bought_tokens = BOUGHT_TOKENS.get_all_tokens();
        
        if bought_tokens.is_empty() {
            self.logger.log("No tokens to sell".cyan().to_string());
            return Ok(());
        }
        
        self.logger.log(format!("Starting periodic sale of {} holdings", bought_tokens.len()).blue().to_string());
        
        let mut successful_sales = 0;
        let mut failed_sales = 0;
        let mut total_sol_received = 0.0;
        
        for token_info in bought_tokens {
            match self.sell_single_token(&token_info).await {
                Ok(sol_received) => {
                    self.logger.log(format!(
                        "✅ Successfully sold {} ({:.6} SOL received)", 
                        token_info.mint, 
                        sol_received
                    ).green().to_string());
                    
                    successful_sales += 1;
                    total_sol_received += sol_received;
                    
                    // Remove sold token from cache
                    BOUGHT_TOKENS.remove_token(&token_info.mint);
                    
                    self.logger.log(format!("Removed {} from holdings cache", token_info.mint).cyan().to_string());
                },
                Err(e) => {
                    self.logger.log(format!(
                        "❌ Failed to sell {}: {}", 
                        token_info.mint, 
                        e
                    ).red().to_string());
                    
                    failed_sales += 1;
                    
                    // Check if token still exists, if not remove from cache
                    if e.to_string().contains("account not found") || e.to_string().contains("Invalid account") {
                        self.logger.log(format!("Token account no longer exists, removing {} from cache", token_info.mint).yellow().to_string());
                        BOUGHT_TOKENS.remove_token(&token_info.mint);
                    }
                }
            }
            
            // Add small delay between sales to avoid rate limiting
            sleep(Duration::from_millis(500)).await;
        }
        
        if successful_sales > 0 || failed_sales > 0 {
            self.logger.log(format!(
                "Periodic selling completed: ✅ {} successful, ❌ {} failed, {:.6} SOL total",
                successful_sales, 
                failed_sales, 
                total_sol_received
            ).blue().to_string());
            
            // Trigger SOL/WSOL balance management after selling
            if successful_sales > 0 {
                self.logger.log("Triggering SOL/WSOL balance management after successful sales...".cyan().to_string());
                if let Err(e) = self.balance_manager.manage_balances_after_selling().await {
                    self.logger.log(format!("Balance management failed: {}", e).red().to_string());
                } else {
                    self.logger.log("Balance management completed successfully".green().to_string());
                }
            }
        }
        
        Ok(())
    }
    
    /// Sell a single token using Jupiter API with retry logic
    async fn sell_single_token(&self, token_info: &crate::common::cache::BoughtTokenInfo) -> Result<f64> {
        let sol_mint = "So11111111111111111111111111111111111111112"; // SOL mint address
        
        // Skip if it's already SOL or WSOL
        if token_info.mint == sol_mint || token_info.mint == "So11111111111111111111111111111111111111112" {
            return Ok(0.0);
        }
        
        const MAX_RETRIES: u32 = 3;
        let mut retry_count = 0;
        
        while retry_count < MAX_RETRIES {
            retry_count += 1;
            
            // Verify current token balance before attempting to sell
            let current_balance = self.verify_token_balance(token_info).await?;
            
            if current_balance.amount_decimal < 0.000001 {
                self.logger.log(format!("Token {} has negligible balance ({:.9}), considering sold", token_info.mint, current_balance.amount_decimal).cyan().to_string());
                return Ok(0.0);
            }
            
            self.logger.log(format!(
                "Attempt {}/{}: Selling {} {} tokens (raw: {}, decimals: {})", 
                retry_count,
                MAX_RETRIES,
                current_balance.amount_decimal, 
                token_info.mint, 
                current_balance.amount_raw, 
                current_balance.decimals
            ).blue().to_string());
            
            match self.attempt_jupiter_sell(token_info, &current_balance).await {
                Ok(sol_received) => {
                    // Verify the sale was successful by checking balance again
                    sleep(Duration::from_secs(5)).await; // Wait for transaction to settle
                    
                    match self.verify_token_balance(token_info).await {
                        Ok(post_sale_balance) => {
                            if post_sale_balance.amount_decimal < current_balance.amount_decimal * 0.1 {
                                // Balance reduced significantly, consider successful
                                self.logger.log(format!(
                                    "✅ Sale verified: {} balance reduced from {:.9} to {:.9}",
                                    token_info.mint,
                                    current_balance.amount_decimal,
                                    post_sale_balance.amount_decimal
                                ).green().to_string());
                                return Ok(sol_received);
                            } else {
                                // Balance didn't reduce enough, transaction likely failed
                                self.logger.log(format!(
                                    "⚠️ Sale verification failed: {} balance only reduced from {:.9} to {:.9} (attempt {}/{})",
                                    token_info.mint,
                                    current_balance.amount_decimal,
                                    post_sale_balance.amount_decimal,
                                    retry_count,
                                    MAX_RETRIES
                                ).yellow().to_string());
                                
                                if retry_count >= MAX_RETRIES {
                                    return Err(anyhow::anyhow!(
                                        "Failed to sell after {} attempts - balance verification failed", 
                                        MAX_RETRIES
                                    ));
                                }
                                // Continue to next retry
                            }
                        },
                        Err(e) => {
                            self.logger.log(format!(
                                "Failed to verify post-sale balance for {}: {}",
                                token_info.mint, e
                            ).red().to_string());
                            
                            if retry_count >= MAX_RETRIES {
                                return Err(anyhow::anyhow!(
                                    "Failed to verify sale after {} attempts: {}", 
                                    MAX_RETRIES, e
                                ));
                            }
                        }
                    }
                },
                Err(e) => {
                    self.logger.log(format!(
                        "❌ Sell attempt {}/{} failed for {}: {}",
                        retry_count, MAX_RETRIES, token_info.mint, e
                    ).red().to_string());
                    
                    if retry_count >= MAX_RETRIES {
                        return Err(e);
                    }
                }
            }
            
            // Wait before retry
            sleep(Duration::from_secs(2)).await;
        }
        
        Err(anyhow::anyhow!("Maximum retry attempts exceeded"))
    }
    
    /// Verify current token balance
    async fn verify_token_balance(&self, token_info: &crate::common::cache::BoughtTokenInfo) -> Result<TokenBalance> {
        use anchor_client::solana_sdk::pubkey::Pubkey;
        use std::str::FromStr;
        
        // First try to get fresh balance from RPC
        match self.app_state.rpc_nonblocking_client.get_token_account(&token_info.token_account).await {
            Ok(Some(account_info)) => {
                let amount_raw = account_info.token_amount.amount.parse::<u64>()
                    .map_err(|e| anyhow::anyhow!("Failed to parse token amount: {}", e))?;
                let amount_decimal = account_info.token_amount.ui_amount.unwrap_or(0.0);
                let decimals = account_info.token_amount.decimals;
                
                self.logger.log(format!(
                    "Fresh balance for {}: {:.9} tokens ({} raw, {} decimals)",
                    token_info.mint, amount_decimal, amount_raw, decimals
                ).cyan().to_string());
                
                // Update cache with fresh balance
                BOUGHT_TOKENS.cache_verified_balance(&token_info.mint, amount_raw, amount_decimal, decimals);
                
                Ok(TokenBalance {
                    amount_raw,
                    amount_decimal,
                    decimals,
                })
            },
            Ok(None) => {
                Err(anyhow::anyhow!("Token account no longer exists"))
            },
            Err(e) => {
                self.logger.log(format!("Failed to get fresh balance for {}, using cached: {}", token_info.mint, e).yellow().to_string());
                
                // Fall back to cached balance
                match BOUGHT_TOKENS.get_cached_balance(&token_info.mint, 300) { // 5 minutes cache
                    Some((raw, decimal, decimals)) => {
                        Ok(TokenBalance {
                            amount_raw: raw,
                            amount_decimal: decimal,
                            decimals,
                        })
                    },
                    None => {
                        // Last resort: use stored values
                        let decimals = token_info.cached_decimals;
                        let amount_raw = (token_info.amount * 10f64.powi(decimals as i32)) as u64;
                        Ok(TokenBalance {
                            amount_raw,
                            amount_decimal: token_info.amount,
                            decimals,
                        })
                    }
                }
            }
        }
    }
    
    /// Attempt to sell using Jupiter API
    async fn attempt_jupiter_sell(&self, token_info: &crate::common::cache::BoughtTokenInfo, balance: &TokenBalance) -> Result<f64> {
        // Get wallet pubkey
        let wallet_pubkey = self.app_state.wallet.pubkey();
        
        // Use the high-level sell function
        let (signature, expected_sol) = self.jupiter_client.sell_token(
            &token_info.mint,
            balance.amount_raw,
            100, // 1% slippage
            &wallet_pubkey,
        ).await.map_err(|e| anyhow::anyhow!("Failed to sell token: {}", e))?;
        
        // Skip if it was a SOL token
        if signature == "skip" {
            return Ok(0.0);
        }
        
        self.logger.log(format!("Swap transaction sent: {}", signature).green().to_string());
        
        // Wait for confirmation with proper timeout
        let mut confirmation_attempts = 0;
        const MAX_CONFIRMATION_ATTEMPTS: u32 = 30; // 30 seconds max wait
        
        while confirmation_attempts < MAX_CONFIRMATION_ATTEMPTS {
            sleep(Duration::from_secs(1)).await;
            confirmation_attempts += 1;
            
            let sig = anchor_client::solana_sdk::signature::Signature::from_str(&signature)
                .map_err(|e| anyhow::anyhow!("Invalid signature: {}", e))?;
            
            match self.app_state.rpc_nonblocking_client.get_signature_status(&sig).await {
                Ok(Some(result)) if result.is_ok() => {
                    self.logger.log(format!("Transaction confirmed: {}", signature).green().to_string());
                    return Ok(expected_sol);
                },
                Ok(Some(result)) => {
                    return Err(anyhow::anyhow!("Transaction failed with error: {:?}", result));
                },
                Ok(None) => {
                    // Still processing, continue waiting
                    if confirmation_attempts % 10 == 0 {
                        self.logger.log(format!("Still waiting for confirmation... ({}/{}s)", confirmation_attempts, MAX_CONFIRMATION_ATTEMPTS).yellow().to_string());
                    }
                },
                Err(e) => {
                    return Err(anyhow::anyhow!("Failed to check transaction status: {}", e));
                }
            }
        }
        
        Err(anyhow::anyhow!("Transaction confirmation timeout after {}s", MAX_CONFIRMATION_ATTEMPTS))
    }
}

/// Helper struct for token balance information
#[derive(Debug, Clone)]
struct TokenBalance {
    amount_raw: u64,
    amount_decimal: f64,
    decimals: u8,
}

impl Clone for PeriodicSellerService {
    fn clone(&self) -> Self {
        Self {
            app_state: self.app_state.clone(),
            jupiter_client: JupiterClient::new(self.app_state.rpc_nonblocking_client.clone()),
            balance_manager: BalanceManager::new(self.app_state.clone()),
            logger: Logger::new("[PERIODIC-SELLER] => ".magenta().to_string()),
            enabled: self.enabled,
        }
    }
} 