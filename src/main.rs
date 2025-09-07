/*
 * Copy Trading Bot with PumpSwap Notification Mode, Jupiter Integration, and Risk Management
 * 
 * Features:
 * - Modified PumpSwap buy/sell logic to only send notifications without executing transactions
 * - Transaction processing now runs in separate tokio tasks to ensure main monitoring continues
 * - Added placeholder for future selling strategy implementation
 * - PumpFun protocol functionality remains unchanged
 * - Added caching and batch RPC calls for improved performance
 * - Added --sell command to sell all holding tokens using Jupiter API
 * - Added --balance command to check and manage SOL/WSOL balance
 * - NEW: Automated risk management system with target wallet monitoring
 * - NEW: Periodic selling system that sells all holdings every 2 minutes using Jupiter API
 * 
 * Usage Commands:
 * - cargo run -- --wrap       : Wrap SOL to WSOL (amount from WRAP_AMOUNT env var)
 * - cargo run -- --unwrap     : Unwrap WSOL back to SOL
 * - cargo run -- --close      : Close all empty token accounts
 * - cargo run -- --sell       : Sell all holding tokens using Jupiter API
 * - cargo run -- --balance    : Check and manage SOL/WSOL balance
 * - cargo run -- --check-tokens : Check token tracking status
 * - cargo run -- --risk-check : Run manual risk management check (NEW!)
 * - cargo run                  : Start copy trading bot with risk management
 * 
 * Jupiter API Integration:
 * The --sell command performs the following operations:
 * 1. Fetches all token accounts owned by the wallet
 * 2. Gets current prices for all tokens using Jupiter Price API
 * 3. Filters tokens worth more than $1 USD
 * 4. For each valuable token:
 *    - Gets a quote from Jupiter Quote API (1% slippage)
 *    - Gets swap transaction from Jupiter Swap API
 *    - Signs and executes the transaction via RPC
 *    - Waits for confirmation
 * 5. Automatically triggers SOL/WSOL balance management
 * 6. Reports successful sales and total SOL received
 * 
 * The --balance command performs SOL/WSOL balance management:
 * 1. Checks current SOL and WSOL balances
 * 2. Calculates if rebalancing is needed (default: 20% deviation from 50/50 split)
 * 3. Automatically wraps SOL to WSOL or unwraps WSOL to SOL as needed
 * 4. Ensures optimal balance for both Pump.fun (SOL) and other DEXes (WSOL)
 * 
 * Environment Variables:
 * - WRAP_AMOUNT: Amount of SOL to wrap (default: 0.1)
 * - RISK_MANAGEMENT_ENABLED: Enable/disable risk management (default: true)
 * - RISK_TARGET_TOKEN_THRESHOLD: Token threshold for risk alerts (default: 1000)
 * - PERIODIC_SELLING_ENABLED: Enable/disable periodic selling (default: true)
 * - PERIODIC_SELLING_INTERVAL_SECONDS: Interval for periodic selling (default: 120)
 * 
 * Balance Management:
 * The system automatically maintains optimal SOL/WSOL ratios for different DEXes:
 * - Pump.fun requires native SOL for trading
 * - Other DEXes (Raydium, etc.) require WSOL for trading
 * - Default target: 50% SOL / 50% WSOL with 20% deviation threshold
 * - Triggered automatically after Jupiter selling or manually via --balance
 * - RISK_CHECK_INTERVAL_MINUTES: Risk check interval in minutes (default: 10)
 * - PERIODIC_SELLING_ENABLED: Enable/disable periodic selling (default: true)
 * - PERIODIC_SELLING_INTERVAL_SECONDS: Selling interval in seconds (default: 120)
 * - Standard bot configuration variables (RPC URLs, wallet keys, etc.)
 * 
 * Risk Management:
 * The bot automatically monitors target wallet token balances every 10 minutes.
 * If any target wallet has less than 1000 tokens (configurable) of any held token,
 * the system automatically sells ALL held tokens using Jupiter API, clears caches,
 * and resumes monitoring. This protects against following wallets that dump tokens.
 * 
 * Periodic Selling:
 * The bot automatically sells all holdings every 2 minutes using Jupiter API to prevent
 * accumulation of tokens when the copy trading bot misses selling transactions.
 * Features include:
 * - Pre-sale balance verification to ensure tokens still exist
 * - Post-sale balance verification to confirm successful sales
 * - Retry logic (up to 3 attempts) for failed transactions with existing balances
 * - Automatic cache cleanup for successfully sold tokens
 * - Smart balance checking with RPC fallback to cached values
 * This feature can be disabled by setting PERIODIC_SELLING_ENABLED=false.
 */

use anchor_client::solana_sdk::signature::Signer;
use solana_vntr_sniper::{
    common::{config::Config, constants::RUN_MSG, cache::WALLET_TOKEN_ACCOUNTS},
    engine::{
        copy_trading::{start_copy_trading, CopyTradingConfig},
        swap::SwapProtocol,
    },
    services::{cache_maintenance, blockhash_processor::BlockhashProcessor, periodic_seller::PeriodicSellerService, balance_monitor::BalanceMonitorService},
    core::token,
};
use solana_program_pack::Pack;
use anchor_client::solana_sdk::pubkey::Pubkey;
use anchor_client::solana_sdk::transaction::{Transaction, VersionedTransaction};
use anchor_client::solana_sdk::system_instruction;
use std::str::FromStr;
use colored::Colorize;
use spl_token::instruction::sync_native;
use spl_token::ui_amount_to_amount;
use spl_associated_token_account::get_associated_token_address;
use std::sync::Arc;
use serde::{Deserialize, Serialize};
use reqwest;
use std::collections::HashMap;
use base64;

// Jupiter API structures
#[derive(Debug, Serialize, Deserialize)]
struct JupiterQuoteRequest {
    #[serde(rename = "inputMint")]
    input_mint: String,
    #[serde(rename = "outputMint")]
    output_mint: String,
    amount: String,
    #[serde(rename = "slippageBps")]
    slippage_bps: u16,
}

#[derive(Debug, Serialize, Deserialize)]
struct JupiterQuoteResponse {
    #[serde(rename = "inputMint")]
    input_mint: String,
    #[serde(rename = "inAmount")]
    in_amount: String,
    #[serde(rename = "outputMint")]
    output_mint: String,
    #[serde(rename = "outAmount")]
    out_amount: String,
    #[serde(rename = "otherAmountThreshold")]
    other_amount_threshold: String,
    #[serde(rename = "swapMode")]
    swap_mode: String,
    #[serde(rename = "slippageBps")]
    slippage_bps: u16,
    #[serde(rename = "platformFee")]
    platform_fee: Option<PlatformFeeInfo>,
    #[serde(rename = "priceImpactPct")]
    price_impact_pct: Option<String>,
    #[serde(rename = "routePlan")]
    route_plan: Vec<RoutePlanEntry>,
    #[serde(rename = "contextSlot")]
    context_slot: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct PlatformFeeInfo {
    amount: String,
    #[serde(rename = "feeBps")]
    fee_bps: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct RoutePlanEntry {
    #[serde(rename = "swapInfo")]
    swap_info: SwapInfoEntry,
    percent: u8,
}

#[derive(Debug, Serialize, Deserialize)]
struct SwapInfoEntry {
    label: String,
    #[serde(rename = "ammKey")]
    amm_key: String,
    #[serde(rename = "inputMint")]
    input_mint: String,
    #[serde(rename = "outputMint")]
    output_mint: String,
    #[serde(rename = "inAmount")]
    in_amount: String,
    #[serde(rename = "outAmount")]
    out_amount: String,
    #[serde(rename = "feeAmount")]
    fee_amount: String,
    #[serde(rename = "feeMint")]
    fee_mint: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct PrioritizationFeeLamports {
    #[serde(rename = "priorityLevelWithMaxLamports")]
    priority_level_with_max_lamports: PriorityLevelWithMaxLamports,
}

#[derive(Debug, Serialize, Deserialize)]
struct PriorityLevelWithMaxLamports {
    #[serde(rename = "maxLamports")]
    max_lamports: u64,
    #[serde(rename = "priorityLevel")]
    priority_level: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct JupiterSwapRequest {
    #[serde(rename = "quoteResponse")]
    quote_response: JupiterQuoteResponse,
    #[serde(rename = "userPublicKey")]
    user_public_key: String,
    #[serde(rename = "wrapAndUnwrapSol")]
    wrap_and_unwrap_sol: bool,
    #[serde(rename = "dynamicComputeUnitLimit")]
    dynamic_compute_unit_limit: bool,
    #[serde(rename = "prioritizationFeeLamports")]
    prioritization_fee_lamports: PrioritizationFeeLamports,
}

#[derive(Debug, Serialize, Deserialize)]
struct JupiterSwapResponse {
    #[serde(rename = "swapTransaction")]
    swap_transaction: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct JupiterPriceResponse {
    #[serde(rename = "usdPrice")]
    usd_price: f64,
}

#[derive(Debug)]
struct TokenAccount {
    mint: String,
    amount: f64,
    decimals: u8,
    account_address: String,
}

// Constants
const SOL_MINT: &str = "So11111111111111111111111111111111111111112";
const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
const JUPITER_API_URL: &str = "https://quote-api.jup.ag";

/// Initialize the wallet token account list by fetching all token accounts owned by the wallet
async fn initialize_token_account_list(config: &Config) {
    let logger = solana_vntr_sniper::common::logger::Logger::new("[INIT-TOKEN-ACCOUNTS] => ".green().to_string());
    
    if let Ok(wallet_pubkey) = config.app_state.wallet.try_pubkey() {
        logger.log(format!("Initializing token account list for wallet: {}", wallet_pubkey));
        
        // Get the token program pubkey
        let token_program = Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap();
        
        // Query all token accounts owned by the wallet
        let accounts = config.app_state.rpc_client.get_token_accounts_by_owner(
            &wallet_pubkey,
            anchor_client::solana_client::rpc_request::TokenAccountsFilter::ProgramId(token_program)
        );
        match accounts {
            Ok(accounts) => {
                logger.log(format!("Found {} token accounts", accounts.len()));
                
                // Add each token account to our global cache
                for account in accounts {
                    WALLET_TOKEN_ACCOUNTS.insert(Pubkey::from_str(&account.pubkey).unwrap());
                    logger.log(format!("Added token account: {}", account.pubkey ));
                }
                
                logger.log(format!("Token account list initialized with {} accounts", WALLET_TOKEN_ACCOUNTS.size()));
            },
            Err(e) => {
                logger.log(format!("Error fetching token accounts: {}", e));
            }
        }
    } else {
        logger.log("Failed to get wallet pubkey, can't initialize token account list".to_string());
    }
}

/// Wrap SOL to Wrapped SOL (WSOL)
async fn wrap_sol(config: &Config, amount: f64) -> Result<(), String> {
    let logger = solana_vntr_sniper::common::logger::Logger::new("[WRAP-SOL] => ".green().to_string());
    
    // Get wallet pubkey
    let wallet_pubkey = match config.app_state.wallet.try_pubkey() {
        Ok(pk) => pk,
        Err(_) => return Err("Failed to get wallet pubkey".to_string()),
    };
    
    // Create WSOL account instructions
    let (wsol_account, mut instructions) = match token::create_wsol_account(wallet_pubkey) {
        Ok(result) => result,
        Err(e) => return Err(format!("Failed to create WSOL account: {}", e)),
    };
    
    logger.log(format!("WSOL account address: {}", wsol_account));
    
    // Convert UI amount to lamports (1 SOL = 10^9 lamports)
    let lamports = ui_amount_to_amount(amount, 9);
    logger.log(format!("Wrapping {} SOL ({} lamports)", amount, lamports));
    
    // Transfer SOL to the WSOL account
    instructions.push(
        system_instruction::transfer(
            &wallet_pubkey,
            &wsol_account,
            lamports,
        )
    );
    
    // Sync native instruction to update the token balance
    instructions.push(
        sync_native(
            &spl_token::id(),
            &wsol_account,
        ).map_err(|e| format!("Failed to create sync native instruction: {}", e))?
    );
    
    // Send transaction using zeroslot for minimal latency
    let recent_blockhash = match config.app_state.rpc_client.get_latest_blockhash() {
        Ok(hash) => hash,
        Err(e) => return Err(format!("Failed to get recent blockhash for SOL wrapping: {}", e)),
    };
    
    match solana_vntr_sniper::core::tx::new_signed_and_send_zeroslot(
        config.app_state.zeroslot_rpc_client.clone(),
        recent_blockhash,
        &config.app_state.wallet,
        instructions,
        &logger,
    ).await {
        Ok(signatures) => {
            if !signatures.is_empty() {
                logger.log(format!("SOL wrapped successfully, signature: {}", signatures[0]));
                Ok(())
            } else {
                Err("No transaction signature returned".to_string())
            }
        },
        Err(e) => {
            Err(format!("Failed to wrap SOL: {}", e))
        }
    }
}

/// Unwrap SOL from Wrapped SOL (WSOL) account
async fn unwrap_sol(config: &Config) -> Result<(), String> {
    let logger = solana_vntr_sniper::common::logger::Logger::new("[UNWRAP-SOL] => ".green().to_string());
    
    // Get wallet pubkey
    let wallet_pubkey = match config.app_state.wallet.try_pubkey() {
        Ok(pk) => pk,
        Err(_) => return Err("Failed to get wallet pubkey".to_string()),
    };
    
    // Get the WSOL ATA address
    let wsol_account = get_associated_token_address(
        &wallet_pubkey,
        &spl_token::native_mint::id()
    );
    
    logger.log(format!("WSOL account address: {}", wsol_account));
    
    // Check if WSOL account exists
    match config.app_state.rpc_client.get_account(&wsol_account) {
        Ok(_) => {
            logger.log(format!("Found WSOL account: {}", wsol_account));
        },
        Err(_) => {
            return Err(format!("WSOL account does not exist: {}", wsol_account));
        }
    }
    
    // Close the WSOL account to recover SOL
    let close_instruction = token::close_account(
        wallet_pubkey,
        wsol_account,
        wallet_pubkey,
        wallet_pubkey,
        &[&wallet_pubkey],
    ).map_err(|e| format!("Failed to create close account instruction: {}", e))?;
    
    // Send transaction using zeroslot for minimal latency
    let recent_blockhash = match config.app_state.rpc_client.get_latest_blockhash() {
        Ok(hash) => hash,
        Err(e) => return Err(format!("Failed to get recent blockhash for SOL unwrapping: {}", e)),
    };
    
    match solana_vntr_sniper::core::tx::new_signed_and_send_zeroslot(
        config.app_state.zeroslot_rpc_client.clone(),
        recent_blockhash,
        &config.app_state.wallet,
        vec![close_instruction],
        &logger,
    ).await {
        Ok(signatures) => {
            if !signatures.is_empty() {
                logger.log(format!("WSOL unwrapped successfully, signature: {}", signatures[0]));
                Ok(())
            } else {
                Err("No transaction signature returned".to_string())
            }
        },
        Err(e) => {
            Err(format!("Failed to unwrap WSOL: {}", e))
        }
    }
}

/// Close all token accounts owned by the wallet
async fn close_all_token_accounts(config: &Config) -> Result<(), String> {
    let logger = solana_vntr_sniper::common::logger::Logger::new("[CLOSE-TOKEN-ACCOUNTS] => ".green().to_string());
    
    // Get wallet pubkey
    let wallet_pubkey = match config.app_state.wallet.try_pubkey() {
        Ok(pk) => pk,
        Err(_) => return Err("Failed to get wallet pubkey".to_string()),
    };
    
    // Get the token program pubkey
    let token_program = Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap();
    
    // Query all token accounts owned by the wallet
    let accounts = config.app_state.rpc_client.get_token_accounts_by_owner(
        &wallet_pubkey,
        anchor_client::solana_client::rpc_request::TokenAccountsFilter::ProgramId(token_program)
    ).map_err(|e| format!("Failed to get token accounts: {}", e))?;
    
    if accounts.is_empty() {
        logger.log("No token accounts found to close".to_string());
        return Ok(());
    }
    
    logger.log(format!("Found {} token accounts to close", accounts.len()));
    
    let mut closed_count = 0;
    let mut failed_count = 0;
    
    // Close each token account
    for account_info in accounts {
        let token_account = Pubkey::from_str(&account_info.pubkey)
            .map_err(|_| format!("Invalid token account pubkey: {}", account_info.pubkey))?;
        
        // Skip WSOL accounts with non-zero balance (these need to be unwrapped first)
        let account_data = match config.app_state.rpc_client.get_account(&token_account) {
            Ok(data) => data,
            Err(e) => {
                logger.log(format!("Failed to get account data for {}: {}", token_account, e).red().to_string());
                failed_count += 1;
                continue;
            }
        };
        
        // Check if this is a WSOL account with balance
        if let Ok(token_data) = spl_token::state::Account::unpack(&account_data.data) {
            if token_data.mint == spl_token::native_mint::id() && token_data.amount > 0 {
                logger.log(format!("Skipping WSOL account with non-zero balance: {} ({})", 
                                 token_account, 
                                 token_data.amount as f64 / 1_000_000_000.0));
                continue;
            }
        }
        
        // Create close instruction
        let close_instruction = token::close_account(
            wallet_pubkey,
            token_account,
            wallet_pubkey,
            wallet_pubkey,
            &[&wallet_pubkey],
        ).map_err(|e| format!("Failed to create close instruction for {}: {}", token_account, e))?;
        
        // Send transaction using normal RPC for token account closing
        let recent_blockhash = match config.app_state.rpc_client.get_latest_blockhash() {
            Ok(hash) => hash,
            Err(e) => return Err(format!("Failed to get recent blockhash for token account {}: {}", token_account, e)),
        };
        
        match solana_vntr_sniper::core::tx::new_signed_and_send_normal(
            config.app_state.rpc_nonblocking_client.clone(),
            recent_blockhash,
            &config.app_state.wallet,
            vec![close_instruction],
            &logger,
        ).await {
            Ok(signatures) => {
                if !signatures.is_empty() {
                    logger.log(format!("Closed token account {}, signature: {}", token_account, signatures[0]));
                    closed_count += 1;
                } else {
                    logger.log(format!("Failed to close token account {}: No signature returned", token_account).red().to_string());
                    failed_count += 1;
                }
            },
            Err(e) => {
                logger.log(format!("Failed to close token account {}: {}", token_account, e).red().to_string());
                failed_count += 1;
            }
        }
    }
    
    logger.log(format!("Closed {} token accounts, {} failed", closed_count, failed_count));
    
    if failed_count > 0 {
        Err(format!("Failed to close {} token accounts", failed_count))
    } else {
        Ok(())
    }
}

/// Initialize target wallet token list by fetching all token accounts owned by the target wallet
async fn initialize_target_wallet_token_list(config: &Config, target_addresses: &[String]) -> Result<(), String> {
    let logger = solana_vntr_sniper::common::logger::Logger::new("[INIT-TARGET-TOKENS] => ".green().to_string());
    
    // Check if we should initialize
    let should_check = std::env::var("IS_CHECK_TARGET_WALLET_TOKEN_ACCOUNT")
        .ok()
        .and_then(|v| v.parse::<bool>().ok())
        .unwrap_or(false);
        
    if !should_check {
        logger.log("Skipping target wallet token check as IS_CHECK_TARGET_WALLET_TOKEN_ACCOUNT is not true".to_string());
        return Ok(());
    }
    
    // Get the token program pubkey
    let token_program = Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap();
    
    for target_address in target_addresses {
        // Parse target wallet address
        let target_pubkey = match Pubkey::from_str(target_address) {
            Ok(pk) => pk,
            Err(e) => {
                logger.log(format!("Invalid target address {}: {}", target_address, e).red().to_string());
                continue;
            }
        };
        
        // Query all token accounts owned by the target wallet
        match config.app_state.rpc_client.get_token_accounts_by_owner(
            &target_pubkey,
            anchor_client::solana_client::rpc_request::TokenAccountsFilter::ProgramId(token_program)
        ) {
            Ok(accounts) => {
                logger.log(format!("Found {} token accounts for target {}", accounts.len(), target_address));
                
                // Add each token's mint to our global cache
                for account in accounts {
                    if let Ok(token_account) = config.app_state.rpc_client.get_account(&Pubkey::from_str(&account.pubkey).unwrap()) {
                        if let Ok(parsed) = spl_token::state::Account::unpack(&token_account.data) {
                            solana_vntr_sniper::common::cache::TARGET_WALLET_TOKENS.insert(parsed.mint.to_string());
                            logger.log(format!("Added token mint {} to target wallet list", parsed.mint));
                        }
                    }
                }
            },
            Err(e) => {
                logger.log(format!("Error fetching token accounts for target {}: {}", target_address, e).red().to_string());
            }
        }
    }
    
    logger.log(format!(
        "Target wallet token list initialized with {} tokens",
        solana_vntr_sniper::common::cache::TARGET_WALLET_TOKENS.size()
    ));
    
    Ok(())
}

/// Get all token accounts for the wallet
async fn get_wallet_token_accounts(config: &Config) -> Result<Vec<TokenAccount>, String> {
    let logger = solana_vntr_sniper::common::logger::Logger::new("[GET-TOKEN-ACCOUNTS] => ".green().to_string());
    
    let wallet_pubkey = match config.app_state.wallet.try_pubkey() {
        Ok(pk) => pk,
        Err(_) => return Err("Failed to get wallet pubkey".to_string()),
    };
    
    logger.log(format!("Getting token accounts for wallet: {}", wallet_pubkey));
    
    // Get the token program pubkey
    let token_program = Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap();
    
    // Query all token accounts owned by the wallet
    let accounts = config.app_state.rpc_client.get_token_accounts_by_owner(
        &wallet_pubkey,
        anchor_client::solana_client::rpc_request::TokenAccountsFilter::ProgramId(token_program)
    ).map_err(|e| format!("Failed to get token accounts: {}", e))?;
    
    let mut token_accounts = Vec::new();
    
    for account_info in accounts {
        let token_account_pubkey = Pubkey::from_str(&account_info.pubkey)
            .map_err(|_| format!("Invalid token account pubkey: {}", account_info.pubkey))?;
        
        // Get account data
        let account_data = match config.app_state.rpc_client.get_account(&token_account_pubkey) {
            Ok(data) => data,
            Err(e) => {
                logger.log(format!("Failed to get account data for {}: {}", token_account_pubkey, e).yellow().to_string());
                continue;
            }
        };
        
        // Parse token account data
        if let Ok(token_data) = spl_token::state::Account::unpack(&account_data.data) {
            if token_data.amount > 0 {
                // Get proper decimals from mint account
                let decimals = match config.app_state.rpc_client.get_account(&token_data.mint) {
                    Ok(mint_account) => {
                        if let Ok(mint_data) = spl_token::state::Mint::unpack(&mint_account.data) {
                            mint_data.decimals
                        } else {
                            9 // Default decimals
                        }
                    },
                    Err(_) => 9 // Default decimals
                };
                
                // Convert amount to UI amount using proper decimals
                let ui_amount = token_data.amount as f64 / 10f64.powi(decimals as i32);
                
                if ui_amount > 0.0 {
                    token_accounts.push(TokenAccount {
                        mint: token_data.mint.to_string(),
                        amount: ui_amount,
                        decimals,
                        account_address: account_info.pubkey,
                    });
                    
                    logger.log(format!("Found token: {} - Amount: {} - Decimals: {}", 
                                     token_data.mint, ui_amount, decimals));
                }
            }
        }
    }
    
    logger.log(format!("Found {} tokens with balance", token_accounts.len()));
    Ok(token_accounts)
}

/// Get token prices from Jupiter API
async fn get_token_prices(mints: &[String]) -> Result<HashMap<String, JupiterPriceResponse>, String> {
    if mints.is_empty() {
        return Ok(HashMap::new());
    }
    
    let logger = solana_vntr_sniper::common::logger::Logger::new("[GET-PRICES] => ".cyan().to_string());
    logger.log(format!("Getting prices for {} tokens", mints.len()));
    
    let client = reqwest::Client::new();
    let mint_string = mints.join(",");
    let url = format!("{}/v6/price?ids={}", JUPITER_API_URL, mint_string);
    
    let response = client.get(&url)
        .send()
        .await
        .map_err(|e| format!("Failed to get prices: {}", e))?;
    
    if !response.status().is_success() {
        return Err(format!("Price API returned status: {}", response.status()));
    }
    
    let prices: HashMap<String, JupiterPriceResponse> = response.json()
        .await
        .map_err(|e| format!("Failed to parse price response: {}", e))?;
    
    logger.log(format!("Retrieved prices for {} tokens", prices.len()));
    Ok(prices)
}

/// Get Jupiter quote for token swap
async fn get_jupiter_quote(
    input_mint: &str,
    output_mint: &str,
    amount: u64,
    slippage_bps: u16,
) -> Result<JupiterQuoteResponse, String> {
    let logger = solana_vntr_sniper::common::logger::Logger::new("[JUPITER-QUOTE] => ".yellow().to_string());
    logger.log(format!("Getting quote: {} -> {} (amount: {})", input_mint, output_mint, amount));
    
    let client = reqwest::Client::new();
    let url = format!(
        "{}/v6/quote?inputMint={}&outputMint={}&amount={}&slippageBps={}",
        JUPITER_API_URL, input_mint, output_mint, amount, slippage_bps
    );
    
    let response = client.get(&url)
        .send()
        .await
        .map_err(|e| format!("Failed to get quote: {}", e))?;
    
    if !response.status().is_success() {
        let status = response.status();
        let error_text = response.text().await.unwrap_or_default();
        return Err(format!("Quote API returned status: {} - {}", status, error_text));
    }
    
    // Log the raw response for debugging
    let response_text = response.text().await
        .map_err(|e| format!("Failed to read response text: {}", e))?;
    logger.log(format!("Raw quote response (first 500 chars): {}", 
        &response_text[..std::cmp::min(500, response_text.len())]));
    
    let quote: JupiterQuoteResponse = serde_json::from_str(&response_text)
        .map_err(|e| format!("Failed to parse quote response: {}. Response: {}", e, 
            &response_text[..std::cmp::min(200, response_text.len())]))?;
    
    logger.log(format!("Quote received: {} {} -> {} {}", 
                     quote.in_amount, input_mint, quote.out_amount, output_mint));
    
    Ok(quote)
}

/// Get Jupiter swap transaction
async fn get_jupiter_swap_transaction(
    quote: JupiterQuoteResponse,
    user_public_key: &str,
) -> Result<String, String> {
    let logger = solana_vntr_sniper::common::logger::Logger::new("[JUPITER-SWAP] => ".blue().to_string());
    logger.log("Getting swap transaction from Jupiter".to_string());
    
    let client = reqwest::Client::new();
    let url = "https://lite-api.jup.ag/swap/v1/swap";
    
    let swap_request = JupiterSwapRequest {
        quote_response: quote,
        user_public_key: user_public_key.to_string(),
        wrap_and_unwrap_sol: true,
        dynamic_compute_unit_limit: false, 
        prioritization_fee_lamports: PrioritizationFeeLamports {
            priority_level_with_max_lamports: PriorityLevelWithMaxLamports {
                max_lamports: 1000000,
                priority_level: "high".to_string(),
            },
        },
    };
    
    // Log the request for debugging
    logger.log(format!("Sending swap request to: {}", url));
    logger.log(format!("Request payload (first 300 chars): {}", 
        serde_json::to_string(&swap_request)
            .unwrap_or_else(|_| "Failed to serialize".to_string())
            .chars().take(300).collect::<String>()));
    
    let response = client.post(url)
        .json(&swap_request)
        .send()
        .await
        .map_err(|e| format!("Failed to get swap transaction: {}", e))?;
    
    if !response.status().is_success() {
        let status = response.status();
        let error_text = response.text().await.unwrap_or_default();
        logger.log(format!("Jupiter swap API error: Status {}, Response: {}", status, error_text));
        return Err(format!("Swap API returned status: {} - {}", status, error_text));
    }
    
    let swap_response: JupiterSwapResponse = response.json()
        .await
        .map_err(|e| format!("Failed to parse swap response: {}", e))?;
    
    logger.log("Swap transaction received from Jupiter".to_string());
    Ok(swap_response.swap_transaction)
}

/// Execute Jupiter swap transaction
async fn execute_swap_transaction(
    config: &Config,
    swap_transaction_base64: &str,
) -> Result<String, String> {
    let logger = solana_vntr_sniper::common::logger::Logger::new("[EXECUTE-SWAP] => ".green().to_string());
    logger.log("Executing swap transaction".to_string());
    
    // Decode the base64 transaction
    let transaction_bytes = base64::decode(swap_transaction_base64)
        .map_err(|e| format!("Failed to decode transaction: {}", e))?;
    
    // Try to deserialize as versioned transaction first (Jupiter v6 primarily uses versioned transactions)
    if let Ok(mut transaction) = bincode::deserialize::<VersionedTransaction>(&transaction_bytes) {
        logger.log("Processing as versioned transaction".to_string());
        
        // Get recent blockhash
        let recent_blockhash = config.app_state.rpc_client.get_latest_blockhash()
            .map_err(|e| format!("Failed to get recent blockhash: {}", e))?;
        
        // Update the transaction's blockhash
        transaction.message.set_recent_blockhash(recent_blockhash);
        
        // Sign the versioned transaction
        let message_data = transaction.message.serialize();
        let signature = config.app_state.wallet.sign_message(&message_data);
        
        // Find the position of the keypair in the account keys to place the signature
        let account_keys = transaction.message.static_account_keys();
        if let Some(signer_index) = account_keys.iter().position(|key| *key == config.app_state.wallet.pubkey()) {
            // Ensure we have enough signatures
            if transaction.signatures.len() <= signer_index {
                transaction.signatures.resize(signer_index + 1, anchor_client::solana_sdk::signature::Signature::default());
            }
            transaction.signatures[signer_index] = signature;
        } else {
            return Err("Wallet not found in transaction account keys".to_string());
        }
        
        // Send the transaction with retry logic
        let mut attempts = 0;
        let max_attempts = 3;
        
        while attempts < max_attempts {
            attempts += 1;
            
            // Try to convert versioned transaction to legacy transaction for zeroslot compatibility
            if let Some(legacy_tx) = transaction.clone().into_legacy_transaction() {
                // Send the legacy transaction using zeroslot for minimal latency
                match config.app_state.zeroslot_rpc_client.send_transaction(&legacy_tx).await {
                    Ok(signature) => {
                        logger.log(format!("Versioned swap transaction sent via zeroslot: {}", signature));
                        
                        // Wait for confirmation
                        for _ in 0..30 { // Wait up to 30 seconds for confirmation
                            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                            
                            if let Ok(status) = config.app_state.rpc_client.get_signature_status(&signature) {
                                if let Some(result) = status {
                                    if result.is_ok() {
                                        logger.log(format!("Versioned swap transaction confirmed: {}", signature));
                                        return Ok(signature.to_string());
                                    } else {
                                        return Err(format!("Transaction failed: {:?}", result));
                                    }
                                }
                            }
                        }
                        
                        return Err("Transaction confirmation timeout".to_string());
                    },
                    Err(e) => {
                        logger.log(format!("Versioned swap attempt {}/{} failed: {}", attempts, max_attempts, e).red().to_string());
                        
                        if attempts >= max_attempts {
                            return Err(format!("All versioned swap attempts failed. Last error: {}", e));
                        }
                        
                        // Wait before retry
                        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                    }
                }
            } else {
                // If conversion to legacy transaction fails, use normal RPC client as fallback
                logger.log("Cannot convert versioned transaction to legacy, using normal RPC".yellow().to_string());
                match config.app_state.rpc_client.send_transaction(&transaction) {
                    Ok(signature) => {
                        logger.log(format!("Versioned swap transaction sent via normal RPC: {}", signature));
                        
                        // Wait for confirmation
                        for _ in 0..30 { // Wait up to 30 seconds for confirmation
                            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                            
                            if let Ok(status) = config.app_state.rpc_client.get_signature_status(&signature) {
                                if let Some(result) = status {
                                    if result.is_ok() {
                                        logger.log(format!("Versioned swap transaction confirmed: {}", signature));
                                        return Ok(signature.to_string());
                                    } else {
                                        return Err(format!("Transaction failed: {:?}", result));
                                    }
                                }
                            }
                        }
                        
                        return Err("Transaction confirmation timeout".to_string());
                    },
                    Err(e) => {
                        logger.log(format!("Versioned swap attempt {}/{} failed: {}", attempts, max_attempts, e).red().to_string());
                        
                        if attempts >= max_attempts {
                            return Err(format!("All versioned swap attempts failed. Last error: {}", e));
                        }
                        
                        // Wait before retry
                        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                    }
                }
            }
        }
    } else if let Ok(mut transaction) = bincode::deserialize::<Transaction>(&transaction_bytes) {
        logger.log("Processing as legacy transaction".to_string());
        
        // Get recent blockhash
        let recent_blockhash = config.app_state.rpc_client.get_latest_blockhash()
            .map_err(|e| format!("Failed to get recent blockhash: {}", e))?;
        
        // Update the transaction's blockhash
        transaction.message.recent_blockhash = recent_blockhash;
        
        // Sign the transaction
        transaction.sign(&[&config.app_state.wallet], recent_blockhash);
        
        // Send the transaction with retry logic
        let mut attempts = 0;
        let max_attempts = 3;
        
        while attempts < max_attempts {
            attempts += 1;
            
            // Send the transaction using zeroslot for minimal latency
            match config.app_state.zeroslot_rpc_client.send_transaction(&transaction).await {
                Ok(signature) => {
                    logger.log(format!("Swap transaction sent via zeroslot: {}", signature));
                    
                    // Wait for confirmation
                    for _ in 0..30 { // Wait up to 30 seconds for confirmation
                        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                        
                        if let Ok(status) = config.app_state.rpc_client.get_signature_status(&signature) {
                            if let Some(result) = status {
                                if result.is_ok() {
                                    logger.log(format!("Swap transaction confirmed: {}", signature));
                                    return Ok(signature.to_string());
                                } else {
                                    return Err(format!("Transaction failed: {:?}", result));
                                }
                            }
                        }
                    }
                    
                    return Err("Transaction confirmation timeout".to_string());
                },
                Err(e) => {
                    logger.log(format!("Swap attempt {}/{} failed: {}", attempts, max_attempts, e).red().to_string());
                    
                    if attempts >= max_attempts {
                        return Err(format!("All swap attempts failed. Last error: {}", e));
                    }
                    
                    // Wait before retry
                    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                }
            }
        }
    } else {
        // If Jupiter returns neither transaction format, return an error
        logger.log("Transaction format not supported - unable to deserialize as legacy or versioned transaction".yellow().to_string());
        return Err("Unsupported transaction format - unable to deserialize as legacy or versioned transaction".to_string());
    }
    
    Err("Maximum retry attempts exceeded".to_string())
}

/// Sell a specific token using Jupiter API
async fn sell_token(config: &Config, token: &TokenAccount) -> Result<String, String> {
    let logger = solana_vntr_sniper::common::logger::Logger::new(format!("[SELL-TOKEN] {} => ", token.mint).magenta().to_string());
    
    // Skip SOL and WSOL
    if token.mint == SOL_MINT || token.mint == WSOL_MINT {
        logger.log("Skipping SOL/WSOL token".to_string());
        return Ok("Skipped SOL/WSOL".to_string());
    }
    
    logger.log(format!("Selling {} {} (decimals: {})", token.amount, token.mint, token.decimals));
    
    // Convert amount to smallest unit
    let amount_in_smallest_unit = (token.amount * 10f64.powi(token.decimals as i32)) as u64;
    logger.log(format!("Amount in smallest unit: {}", amount_in_smallest_unit));
    
    // Get quote from Jupiter
    let quote = get_jupiter_quote(
        &token.mint,
        SOL_MINT,
        amount_in_smallest_unit,
        100, // 1% slippage
    ).await?;
    
    // Calculate expected SOL output
    let expected_sol = quote.out_amount.parse::<u64>()
        .map_err(|e| format!("Failed to parse output amount: {}", e))? as f64 / 1e9;
    
    logger.log(format!("Expected SOL output: {}", expected_sol));
    
    // Get wallet pubkey
    let wallet_pubkey = config.app_state.wallet.try_pubkey()
        .map_err(|_| "Failed to get wallet pubkey".to_string())?;
    
    // Get swap transaction
    let swap_transaction = get_jupiter_swap_transaction(quote, &wallet_pubkey.to_string()).await?;
    
    // Execute the swap
    let signature = execute_swap_transaction(config, &swap_transaction).await?;
    
    logger.log(format!("Token sold successfully! Signature: {}", signature));
    Ok(signature)
}

/// Sell all holding tokens using Jupiter API
async fn sell_all_tokens(config: &Config) -> Result<(), String> {
    let logger = solana_vntr_sniper::common::logger::Logger::new("[SELL-ALL-TOKENS] => ".red().to_string());
    
    logger.log("Starting to sell all holding tokens".to_string());
    
    // Get all token accounts
    let token_accounts = get_wallet_token_accounts(config).await?;
    
    if token_accounts.is_empty() {
        logger.log("No tokens found to sell".to_string());
        return Ok(());
    }
    
    logger.log(format!("Found {} tokens to potentially sell", token_accounts.len()));
    
    // Get token prices to filter valuable tokens
    let mints: Vec<String> = token_accounts.iter().map(|t| t.mint.clone()).collect();
    let prices = get_token_prices(&mints).await.unwrap_or_default();
    
    let mut successful_sales = 0;
    let mut failed_sales = 0;
    let mut total_sol_received = 0.0;
    
    // Sell each token
    for token in &token_accounts {
        // Check if token is valuable (worth more than $1)
        let is_valuable = if let Some(price_info) = prices.get(&token.mint) {
            let token_value = token.amount * price_info.usd_price;
            logger.log(format!("Token {} value: ${:.2}", token.mint, token_value));
            token_value > 1.0
        } else {
            logger.log(format!("No price found for token {}, attempting to sell anyway", token.mint));
            true
        };
        
        if !is_valuable {
            logger.log(format!("Skipping token {} (value < $1)", token.mint));
            continue;
        }
        
        match sell_token(config, token).await {
            Ok(signature) => {
                logger.log(format!("✅ Successfully sold {}: {}", token.mint, signature));
                successful_sales += 1;
                
                // Try to estimate SOL received (this is approximate)
                if let Ok(quote) = get_jupiter_quote(&token.mint, SOL_MINT, 
                    (token.amount * 10f64.powi(token.decimals as i32)) as u64, 100).await {
                    if let Ok(sol_amount) = quote.out_amount.parse::<u64>() {
                        total_sol_received += sol_amount as f64 / 1e9;
                    }
                }
            },
            Err(e) => {
                logger.log(format!("❌ Failed to sell {}: {}", token.mint, e).red().to_string());
                failed_sales += 1;
            }
        }
        
        // Add delay between swaps to avoid rate limiting
        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    }
    
    logger.log(format!(
        "Selling completed! ✅ {} successful, ❌ {} failed, ~{:.6} SOL received",
        successful_sales, failed_sales, total_sol_received
    ));
    
    if failed_sales > 0 {
        Err(format!("Some token sales failed: {} out of {}", failed_sales, successful_sales + failed_sales))
    } else {
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    /* Initial Settings */
    let config = Config::new().await;
    let config = config.lock().await;

    /* Running Bot */
    let run_msg = RUN_MSG;
    println!("{}", run_msg);
    
    // Initialize blockhash processor
    let blockhash_processor = match BlockhashProcessor::new(config.app_state.rpc_client.clone()).await {
        Ok(processor) => {
            if let Err(e) = processor.start().await {
                eprintln!("Failed to start blockhash processor: {}", e);
                return;
            }
            println!("Blockhash processor started successfully");
            processor
        },
        Err(e) => {
            eprintln!("Failed to initialize blockhash processor: {}", e);
            return;
        }
    };

    // Parse command line arguments
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        // Check for command line arguments
        if args.contains(&"--wrap".to_string()) {
            println!("Wrapping SOL to WSOL...");
            
            // Get wrap amount from .env
            let wrap_amount = std::env::var("WRAP_AMOUNT")
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.1);
            
            match wrap_sol(&config, wrap_amount).await {
                Ok(_) => {
                    println!("Successfully wrapped {} SOL to WSOL", wrap_amount);
                    return;
                },
                Err(e) => {
                    eprintln!("Failed to wrap SOL: {}", e);
                    return;
                }
            }
        } else if args.contains(&"--unwrap".to_string()) {
            println!("Unwrapping WSOL to SOL...");
            
            match unwrap_sol(&config).await {
                Ok(_) => {
                    println!("Successfully unwrapped WSOL to SOL");
                    return;
                },
                Err(e) => {
                    eprintln!("Failed to unwrap WSOL: {}", e);
                    return;
                }
            }
        } else if args.contains(&"--close".to_string()) {
            println!("Closing all token accounts...");
            
            match close_all_token_accounts(&config).await {
                Ok(_) => {
                    println!("Successfully closed all token accounts");
                    return;
                },
                Err(e) => {
                    eprintln!("Failed to close all token accounts: {}", e);
                    return;
                }
            }
        } else if args.contains(&"--check-tokens".to_string()) {
            println!("Checking token tracking status...");
            
            let token_monitor = solana_vntr_sniper::services::token_monitor::TokenMonitor::new(
                Arc::new(config.app_state.clone())
            );
            
            let summary = token_monitor.get_tracking_summary();
            println!("{}", summary);
            return;
        } else if args.contains(&"--sell".to_string()) {
            println!("Selling all holding tokens using Jupiter API...");
            match sell_all_tokens(&config).await {
                Ok(_) => {
                    println!("Successfully sold all holding tokens!");
                    return;
                },
                Err(e) => {
                    eprintln!("Failed to sell all tokens: {}", e);
                    return;
                }
            }
        } else if args.contains(&"--balance".to_string()) {
            println!("Checking and managing SOL/WSOL balance...");
            
            use solana_vntr_sniper::services::balance_manager::BalanceManager;
            let balance_manager = BalanceManager::new(Arc::new(config.app_state.clone()));
            
            match balance_manager.get_current_balances().await {
                Ok(balance_info) => {
                    println!("Current balances:");
                    println!("  SOL:   {:.6}", balance_info.sol_balance);
                    println!("  WSOL:  {:.6}", balance_info.wsol_balance);
                    println!("  Total: {:.6}", balance_info.total_balance);
                    println!("  Ratio: {:.2}", balance_info.balance_ratio);
                    
                    if balance_manager.needs_rebalancing(&balance_info) {
                        println!("\nRebalancing needed! Performing automatic rebalancing...");
                        match balance_manager.rebalance().await {
                            Ok(_) => println!("✅ Rebalancing completed successfully!"),
                            Err(e) => eprintln!("❌ Rebalancing failed: {}", e),
                        }
                    } else {
                        println!("\n✅ Balances are well balanced - no action needed");
                    }
                    return;
                },
                Err(e) => {
                    eprintln!("Failed to check balances: {}", e);
                    return;
                }
            }
        } else if args.contains(&"--risk-check".to_string()) {
            println!("Running manual risk management check...");
            
            // Seller-bot: use our own wallet as the only target address
            let wallet_pubkey_str = match config.app_state.wallet.try_pubkey() {
                Ok(pk) => pk.to_string(),
                Err(e) => {
                    eprintln!("Failed to get wallet pubkey: {}", e);
                    return;
                }
            };
            let target_addresses = vec![wallet_pubkey_str];
            
            let risk_service = solana_vntr_sniper::services::risk_management::RiskManagementService::new(
                Arc::new(config.app_state.clone()),
                target_addresses
            );
            
            match risk_service.manual_risk_check().await {
                Ok(_) => {
                    println!("Risk check completed successfully!");
                    return;
                },
                Err(e) => {
                    eprintln!("Risk check failed: {}", e);
                    return;
                }
            }
        }
    }


    
    // Initialize token account list
    initialize_token_account_list(&config).await;
    
    // Start cache maintenance service (clean up expired cache entries every 60 seconds)
    cache_maintenance::start_cache_maintenance(60).await;
    println!("Cache maintenance service started");

    // Seller-bot mode: subscribe to our own wallet pubkey
    let wallet_pubkey_str = match config.app_state.wallet.try_pubkey() {
        Ok(pk) => pk.to_string(),
        Err(e) => {
            eprintln!("Failed to get wallet pubkey: {}", e);
            return;
        }
    };
    let target_addresses = vec![wallet_pubkey_str];
    let excluded_addresses: Vec<String> = vec![];
    
    // Get protocol preference from environment
    let protocol_preference = std::env::var("PROTOCOL_PREFERENCE")
        .ok()
        .map(|p| match p.to_lowercase().as_str() {
            "pumpfun" => SwapProtocol::PumpFun,
            "pumpswap" => SwapProtocol::PumpSwap,
            "raydium_launchpad" => SwapProtocol::RaydiumLaunchpad,
            _ => SwapProtocol::Auto,
        })
        .unwrap_or(SwapProtocol::Auto);
    
    // Read WAIT_TIME from .env (seconds)
    let wait_time_seconds = std::env::var("WAIT_TIME")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(3);

    // Create seller-bot config
    let copy_trading_config = CopyTradingConfig {
        yellowstone_grpc_http: config.yellowstone_grpc_http.clone(),
        yellowstone_grpc_token: config.yellowstone_grpc_token.clone(),
        app_state: config.app_state.clone(),
        swap_config: config.swap_config.clone(),
        target_addresses: target_addresses.clone(),
        excluded_addresses,
        protocol_preference,
        min_dev_buy: 0.001, // Default value since this field is not in Config
        max_dev_buy: 0.1, // Default value since this field is not in Config
        transaction_landing_mode: config.transaction_landing_mode.clone(),
        wait_time_seconds,
    };
    
    // Create and start the periodic selling service
    let periodic_seller_service = PeriodicSellerService::new(
        Arc::new(config.app_state.clone())
    );
    
    // Start periodic selling service in background
    let periodic_seller_clone = periodic_seller_service.clone();
    tokio::spawn(async move {
        if let Err(e) = periodic_seller_clone.start_periodic_selling().await {
            eprintln!("Periodic selling error: {}", e);
        }
    });
    
    println!("Periodic selling service started (selling every 2 minutes)");
    
    // Create and start the balance monitoring service
    let balance_monitor_service = BalanceMonitorService::new(
        Arc::new(config.app_state.clone())
    );
    
    // Start balance monitoring service in background
    let balance_monitor_clone = balance_monitor_service.clone();
    tokio::spawn(async move {
        if let Err(e) = balance_monitor_clone.start_monitoring().await {
            eprintln!("Balance monitoring error: {}", e);
        }
    });
    
    println!("Balance monitoring service started (checking every 2 minutes with must-selling)");
    
    // Start the copy trading bot
    if let Err(e) = start_copy_trading(copy_trading_config).await {
        eprintln!("Copy trading error: {}", e);
    }
}
