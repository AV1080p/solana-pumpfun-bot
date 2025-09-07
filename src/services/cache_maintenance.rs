use std::time::Duration;
use tokio::time::interval;
use crate::common::cache::{TOKEN_ACCOUNT_CACHE, TOKEN_MINT_CACHE, TOKEN_BALANCE_CACHE};
use crate::common::logger::Logger;
use colored::Colorize;

pub struct CacheMaintenanceService {
    logger: Logger,
    cleanup_interval: Duration,
}

impl CacheMaintenanceService {
    pub fn new(cleanup_interval_seconds: u64) -> Self {
        Self {
            logger: Logger::new("[CACHE-MAINTENANCE] => ".cyan().to_string()),
            cleanup_interval: Duration::from_secs(cleanup_interval_seconds),
        }
    }

    pub async fn start_maintenance_loop(&self) {
        self.logger.log("Starting cache maintenance service".to_string());
        
        let mut interval = interval(self.cleanup_interval);
        
        loop {
            interval.tick().await;
            
            self.cleanup_expired_entries().await;
        }
    }

    async fn cleanup_expired_entries(&self) {
        let start_time = std::time::Instant::now();
        
        // Clean up token account cache
        let account_cache_size_before = TOKEN_ACCOUNT_CACHE.size();
        TOKEN_ACCOUNT_CACHE.clear_expired();
        let account_cache_size_after = TOKEN_ACCOUNT_CACHE.size();
        
        // Clean up token mint cache
        let mint_cache_size_before = TOKEN_MINT_CACHE.size();
        TOKEN_MINT_CACHE.clear_expired();
        let mint_cache_size_after = TOKEN_MINT_CACHE.size();
        
        // Clean up token balance cache
        let balance_cache_size_before = TOKEN_BALANCE_CACHE.size();
        TOKEN_BALANCE_CACHE.clear_stale();
        let balance_cache_size_after = TOKEN_BALANCE_CACHE.size();
        
        let cleanup_duration = start_time.elapsed();
        
        if account_cache_size_before != account_cache_size_after || 
           mint_cache_size_before != mint_cache_size_after ||
           balance_cache_size_before != balance_cache_size_after {
            self.logger.log(format!(
                "Cache cleanup completed in {:?} - Account cache: {} -> {}, Mint cache: {} -> {}, Balance cache: {} -> {}",
                cleanup_duration,
                account_cache_size_before, account_cache_size_after,
                mint_cache_size_before, mint_cache_size_after,
                balance_cache_size_before, balance_cache_size_after
            ).blue().to_string());
        }
    }

    pub fn log_cache_stats(&self) {
        let account_cache_size = TOKEN_ACCOUNT_CACHE.size();
        let mint_cache_size = TOKEN_MINT_CACHE.size();
        let balance_cache_size = TOKEN_BALANCE_CACHE.size();
        
        self.logger.log(format!(
            "Cache stats - Account cache: {} entries, Mint cache: {} entries, Balance cache: {} entries",
            account_cache_size, mint_cache_size, balance_cache_size
        ).blue().to_string());
    }
}

/// Start the cache maintenance service with the specified cleanup interval
pub async fn start_cache_maintenance(cleanup_interval_seconds: u64) {
    let service = CacheMaintenanceService::new(cleanup_interval_seconds);
    
    // Spawn the maintenance loop in the background
    tokio::spawn(async move {
        service.start_maintenance_loop().await;
    });
} 