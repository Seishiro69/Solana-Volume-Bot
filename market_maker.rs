use std::sync::Arc;
use std::time::Duration;
use std::collections::{HashMap, VecDeque};
use tokio::time::Instant;
use anyhow::Result;
use anchor_client::solana_sdk::signature::Signature;
use anchor_client::solana_sdk::signer::Signer;
use anchor_client::solana_sdk::pubkey::Pubkey;
use anchor_client::solana_sdk::system_instruction;
use anchor_client::solana_sdk::transaction::Transaction;
use colored::Colorize;
use tokio::time;
use tokio::sync::Mutex;
use futures_util::stream::StreamExt;
use futures_util::{SinkExt, Sink};
use yellowstone_grpc_client::{ClientTlsConfig, GeyserGrpcClient};
use yellowstone_grpc_proto::geyser::{
    subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest, SubscribeRequestPing,
    SubscribeRequestFilterTransactions, SubscribeUpdate,
};
use crate::engine::transaction_parser;
use crate::common::{
    config::{AppState, SwapConfig, JUPITER_PROGRAM, OKX_DEX_PROGRAM},
    logger::Logger,
    wallet_pool::{WalletPool, RandomizationConfig, TradeType},
    price_monitor::{GlobalPriceMonitor, create_global_price_monitor},
    dynamic_ratios::{GlobalDynamicRatioManager, create_global_dynamic_ratio_manager},
    volume_waves::{GlobalVolumeWaveManager, create_global_volume_wave_manager},
    guardian_mode::{GlobalGuardianMode, create_global_guardian_mode},
};
use crate::dex::raydium_cpmm::RaydiumCPMM;
use crate::engine::swap::{SwapDirection, SwapInType};
use crate::core::token;
use spl_token::instruction::sync_native;
use spl_associated_token_account::get_associated_token_address;
use solana_program_pack::Pack;
use std::str::FromStr;
use rand::Rng;
use crate::engine::transaction_parser::{parse_target_token_transaction, TradeInfoFromToken};

// Activity tracking structures for token analysis
#[derive(Debug, Clone)]
pub struct TokenActivity {
    pub timestamp: Instant,
    pub is_buy: bool,
    pub volume_sol: f64,
    pub user: String,
    pub price: f64,
}

#[derive(Debug, Default)]
pub struct TokenActivityReport {
    pub total_trades: u32,
    pub buy_trades: u32,
    pub sell_trades: u32,
    pub total_volume_sol: f64,
    pub buy_volume_sol: f64,
    pub sell_volume_sol: f64,
    pub average_price: f64,
    pub min_price: f64,
    pub max_price: f64,
    pub unique_traders: u32,
    pub report_period_minutes: u64,
}

/// Configuration for market maker bot with advanced multi-wallet support
#[derive(Clone)]
pub struct MarketMakerConfig {
    pub yellowstone_grpc_http: String,
    pub yellowstone_grpc_token: String,
    pub app_state: Arc<AppState>,
    pub target_token_mint: String,
    pub slippage: u64,
    pub randomization_config: RandomizationConfig,
    pub enable_multi_wallet: bool,
    pub max_concurrent_trades: usize,
    pub enable_telegram_notifications: bool,
}

impl MarketMakerConfig {
    /// Create a new MarketMakerConfig with stealth mode settings
    pub fn stealth_mode(
        yellowstone_grpc_http: String,
        yellowstone_grpc_token: String,
        app_state: Arc<AppState>,
        target_token_mint: String,
    ) -> Self {
        Self {
            yellowstone_grpc_http,
            yellowstone_grpc_token,
            app_state,
            target_token_mint,
            slippage: 1000, // 10%
            randomization_config: RandomizationConfig::stealth_mode(),
            enable_multi_wallet: true,
            max_concurrent_trades: 3,
            enable_telegram_notifications: true,
        }
    }

    /// Create a new MarketMakerConfig with conservative settings
    pub fn conservative_mode(
        yellowstone_grpc_http: String,
        yellowstone_grpc_token: String,
        app_state: Arc<AppState>,
        target_token_mint: String,
    ) -> Self {
        Self {
            yellowstone_grpc_http,
            yellowstone_grpc_token,
            app_state,
            target_token_mint,
            slippage: 1500, // 15%
            randomization_config: RandomizationConfig::conservative_mode(),
            enable_multi_wallet: true,
            max_concurrent_trades: 2,
            enable_telegram_notifications: true,
        }
    }

    /// Create a new MarketMakerConfig with default settings
    pub fn new(
        yellowstone_grpc_http: String,
        yellowstone_grpc_token: String,
        app_state: Arc<AppState>,
        target_token_mint: String,
    ) -> Self {
        Self {
            yellowstone_grpc_http,
            yellowstone_grpc_token,
            app_state,
            target_token_mint,
            slippage: 1000, // 10%
            randomization_config: RandomizationConfig::default(),
            enable_multi_wallet: true,
            max_concurrent_trades: 2,
            enable_telegram_notifications: true,
        }
    }
}

/// Advanced market maker bot with multi-wallet support and sophisticated randomization
pub struct MarketMaker {
    config: MarketMakerConfig,
    wallet_pool: Arc<Mutex<WalletPool>>,
    raydium_cpmm: RaydiumCPMM,
    logger: Logger,
    is_running: Arc<tokio::sync::RwLock<bool>>,
    recent_trades: Arc<Mutex<VecDeque<TradeType>>>,
    trade_counter: Arc<Mutex<u32>>,
    current_wallet: Arc<Mutex<Option<Arc<anchor_client::solana_sdk::signature::Keypair>>>>,
    wallet_change_counter: Arc<Mutex<u32>>,
    token_activities: Arc<Mutex<VecDeque<TokenActivity>>>,
    last_activity_report: Arc<Mutex<Instant>>,
    price_monitor: GlobalPriceMonitor,
    dynamic_ratio_manager: GlobalDynamicRatioManager,
    volume_wave_manager: GlobalVolumeWaveManager,
    guardian_mode: GlobalGuardianMode,
}

impl MarketMaker {
    /// Create a new advanced market maker instance
    pub fn new(config: MarketMakerConfig) -> Result<Self, String> {
        let wallet_pool = WalletPool::new()?;
        let wallet_count = wallet_pool.wallet_count();
        let wallet_pool = Arc::new(Mutex::new(wallet_pool));

        let raydium_cpmm = RaydiumCPMM::new(
            config.app_state.wallet.clone(),
            Some(config.app_state.rpc_client.clone()),
            Some(config.app_state.rpc_nonblocking_client.clone()),
        );

        let logger = Logger::new("[STEALTH-MARKET-MAKER] => ".green().bold().to_string());

        logger.log(format!("🎯 Advanced Market Maker initialized with {} wallets", wallet_count).green().bold().to_string());

        // Create price monitor with default threshold of 15%
        let price_monitor = create_global_price_monitor(0.15);
        
        // Create dynamic ratio manager with weekly changes (168 hours)
        let dynamic_ratio_manager = create_global_dynamic_ratio_manager(0.67, 0.73, 168);
        
        // Create volume wave manager with 2 hour active, 6 hour slow cycles
        let volume_wave_manager = create_global_volume_wave_manager(2, 6);
        
        // Create guardian mode with 10% drop threshold
        let guardian_mode = create_global_guardian_mode(true, 0.10);

        Ok(Self {
            config,
            wallet_pool,
            raydium_cpmm,
            logger,
            is_running: Arc::new(tokio::sync::RwLock::new(false)),
            recent_trades: Arc::new(Mutex::new(VecDeque::with_capacity(20))),
            trade_counter: Arc::new(Mutex::new(0)),
            current_wallet: Arc::new(Mutex::new(None)),
            wallet_change_counter: Arc::new(Mutex::new(0)),
            token_activities: Arc::new(Mutex::new(VecDeque::with_capacity(20))),
            last_activity_report: Arc::new(Mutex::new(Instant::now())),
            price_monitor,
            dynamic_ratio_manager,
            volume_wave_manager,
            guardian_mode,
        })
    }

    /// Start the advanced market maker bot
    pub async fn start(&self) -> Result<(), String> {
        {
            let mut running = self.is_running.write().await;
            if *running {
                return Err("Market maker is already running".to_string());
            }
            *running = true;
        }

        self.logger.log("🚀 Starting Advanced Stealth Market Maker...".green().bold().to_string());
        self.logger.log(format!("Target token: {}", self.config.target_token_mint));
        self.logger.log(format!("Buy amount ratio: {:.1}% - {:.1}% of wrapped WSOL", 
            self.config.randomization_config.min_amount_sol * 100.0, 
            self.config.randomization_config.max_amount_sol * 100.0));
        self.logger.log(format!("Buy/Sell ratio: {:.0}% buy / {:.0}% sell", 
            self.config.randomization_config.buy_sell_ratio * 100.0,
            (1.0 - self.config.randomization_config.buy_sell_ratio) * 100.0));
        self.logger.log(format!("Wallet rotation: Every {} trades", 
            self.config.randomization_config.wallet_rotation_frequency));
        self.logger.log(format!("Max concurrent trades: {}", self.config.max_concurrent_trades));

        // Initialize first wallet
        {
            let mut wallet_pool = self.wallet_pool.lock().await;
            let first_wallet = wallet_pool.get_random_wallet();
            let mut current_wallet = self.current_wallet.lock().await;
            *current_wallet = Some(first_wallet.clone());
            self.logger.log(format!("🔑 Starting with wallet: {}", first_wallet.pubkey()));
        }

        // Start GRPC streaming for token monitoring
        let grpc_task = self.start_grpc_monitoring();
        
        // Start the unified trading engine
        let trading_task = self.start_advanced_trading_engine();

        // Run all tasks concurrently
        tokio::select! {
            result = grpc_task => {
                if let Err(e) = result {
                    self.logger.log(format!("GRPC monitoring failed: {}", e).red().to_string());
                }
            }
            result = trading_task => {
                if let Err(e) = result {
                    self.logger.log(format!("Trading engine failed: {}", e).red().to_string());
                }
            }
        }

        Ok(())
    }

    /// Stop the market maker bot
    pub async fn stop(&self) {
        let mut running = self.is_running.write().await;
        *running = false;
        self.logger.log("Advanced Market Maker stopped".red().to_string());
    }

    /// Check if the market maker is running
    pub async fn is_running(&self) -> bool {
        *self.is_running.read().await
    }

    /// Advanced trading engine with sophisticated randomization
    async fn start_advanced_trading_engine(&self) -> Result<(), String> {
        self.logger.log("🎰 Starting Advanced Trading Engine...".cyan().bold().to_string());

        while self.is_running().await {
                    // Determine next trade type based on recent history with dynamic ratio and guardian mode
        let should_buy = {
            let recent_trades = self.recent_trades.lock().await;
            let trades_vec: Vec<TradeType> = recent_trades.iter().copied().collect();
            let wallet_pool = self.wallet_pool.lock().await;
            
            // Get current dynamic buy ratio
            let mut dynamic_ratio_manager = self.dynamic_ratio_manager.lock().await;
            let mut current_buy_ratio = dynamic_ratio_manager.get_current_buy_ratio();
            
            // Apply guardian mode bias if active
            let guardian_mode = self.guardian_mode.lock().await;
            let guardian_buy_bias = guardian_mode.get_buy_bias();
            if guardian_buy_bias > 0.0 {
                current_buy_ratio = (current_buy_ratio + guardian_buy_bias).min(0.95); // Cap at 95%
                self.logger.log(format!(
                    "🛡️ Guardian mode applying buy bias: +{:.1}% (Total ratio: {:.1}%)",
                    guardian_buy_bias * 100.0,
                    current_buy_ratio * 100.0
                ).red().to_string());
            }
            
            wallet_pool.should_buy_next(&trades_vec, current_buy_ratio)
        };

            // Check if we need to rotate wallet
            let should_rotate_wallet = {
                let wallet_change_counter = self.wallet_change_counter.lock().await;
                *wallet_change_counter >= self.config.randomization_config.wallet_rotation_frequency
            };

            if should_rotate_wallet {
                self.rotate_wallet().await;
            }

            // Execute the trade
            if should_buy {
                // Execute stealth buy with proper amount calculation
                if let Err(e) = self.execute_advanced_buy_debug(0.0).await {
                    self.logger.log(format!("❌ Advanced buy failed: {}", e).red().to_string());
                }
            } else {
                // Generate random sell percentage (10% to 50%)
                let sell_percentage = 0.1 + (rand::random::<f64>() * 0.4);
                if let Err(e) = self.execute_advanced_sell(sell_percentage).await {
                    self.logger.log(format!("❌ Advanced sell failed: {}", e).red().to_string());
                }
            }

            // Generate next interval with price-based throttling, volume waves, and guardian mode
            let next_interval = {
                let wallet_pool = self.wallet_pool.lock().await;
                let price_monitor = self.price_monitor.lock().await;
                let mut volume_wave_manager = self.volume_wave_manager.lock().await;
                let guardian_mode = self.guardian_mode.lock().await;
                
                let base_interval = if should_buy {
                    self.config.randomization_config.base_buy_interval_ms
                } else {
                    self.config.randomization_config.base_sell_interval_ms
                };
                
                // Get raw interval with wallet pool randomization
                let raw_interval = wallet_pool.generate_random_interval(base_interval);
                
                // Apply price-based throttling
                let throttling_multiplier = price_monitor.get_throttling_multiplier();
                let throttled_interval = (raw_interval as f64 * throttling_multiplier) as u64;
                
                // Apply volume wave patterns
                let current_phase = volume_wave_manager.get_current_phase();
                let wave_interval = volume_wave_manager.get_natural_interval(throttled_interval);
                
                // Apply guardian mode acceleration
                let guardian_multiplier = guardian_mode.get_frequency_multiplier();
                let final_interval = (wave_interval as f64 * guardian_multiplier) as u64;
                
                // Log comprehensive status when multiple systems are active
                let is_complex = throttling_multiplier != 1.0 || guardian_multiplier != 1.0 || guardian_mode.is_active();
                if is_complex {
                    self.logger.log(format!(
                        "⚡ Complex interval: Phase: {:?} | Price: {:.1}x | Guardian: {:.1}x | Final: {:.1}min",
                        current_phase,
                        throttling_multiplier,
                        guardian_multiplier,
                        final_interval as f64 / 60000.0
                    ).cyan().to_string());
                }
                
                final_interval
            };

                                if next_interval > 600000 {
                        self.logger.log(format!("🐌 Price throttling active - Next trade in {:.1} minutes", next_interval as f64 / 60000.0).red().to_string());
                    } else {
                        self.logger.log(format!("⏰ Next trade in {:.1} minutes", next_interval as f64 / 60000.0).yellow().to_string());
                    }
            
            // Check and log activity report if it's time
            self.check_and_log_activity_report().await;
            
            // Wait for next trade
            time::sleep(Duration::from_millis(next_interval)).await;

            if !self.is_running().await {
                break;
            }
        }

        Ok(())
    }

    /// Rotate to a new wallet
    async fn rotate_wallet(&self) {
        let new_wallet = {
            let mut wallet_pool = self.wallet_pool.lock().await;
            wallet_pool.get_random_wallet()
        };

        {
            let mut current_wallet = self.current_wallet.lock().await;
            *current_wallet = Some(new_wallet.clone());
        }

        {
            let mut wallet_change_counter = self.wallet_change_counter.lock().await;
            *wallet_change_counter = 0;
        }

        self.logger.log(format!("🔄 Rotated to wallet: {}", new_wallet.pubkey()).magenta().to_string());
    }

    /// Execute an advanced buy transaction with separated steps for debugging
    async fn execute_advanced_buy_debug(&self, _amount_sol: f64) -> Result<Signature, String> {
        let start_time = Instant::now();
        
        let current_wallet = {
            let current_wallet = self.current_wallet.lock().await;
            current_wallet.clone().ok_or("No current wallet set")?
        };

        let wallet_pubkey = current_wallet.pubkey();
        let wsol_account = get_associated_token_address(&wallet_pubkey, &spl_token::native_mint::id());
        
        // Parse target token mint
        let target_token_mint = Pubkey::from_str(&self.config.target_token_mint)
            .map_err(|e| format!("Invalid target token mint: {}", e))?;
        let target_token_account = get_associated_token_address(&wallet_pubkey, &target_token_mint);

        // Get current SOL balance
        let sol_balance = self.config.app_state.rpc_client.get_balance(&wallet_pubkey)
            .map_err(|e| format!("Failed to get SOL balance: {}", e))?;
        let sol_balance_f64 = sol_balance as f64 / 1_000_000_000.0;

        self.logger.log(format!("🔍 INITIAL SOL Balance: {:.6} SOL ({} lamports)", sol_balance_f64, sol_balance).cyan().to_string());

        // Check if accounts exist
        let wsol_exists = self.config.app_state.rpc_client.get_account(&wsol_account).is_ok();
        let target_token_exists = self.config.app_state.rpc_client.get_account(&target_token_account).is_ok();

        self.logger.log(format!("🔍 Account Status - WSOL exists: {}, Target token exists: {}", wsol_exists, target_token_exists).cyan().to_string());

        // Step 1: Create WSOL account if needed
        if !wsol_exists {
            self.logger.log("🔧 Step 1: Creating WSOL account...".yellow().to_string());
            
            let balance_before = self.config.app_state.rpc_client.get_balance(&wallet_pubkey)
                .map_err(|e| format!("Failed to get balance before WSOL creation: {}", e))?;
            
            match self.create_wsol_account_only(&current_wallet).await {
                Ok(()) => {
                    let balance_after = self.config.app_state.rpc_client.get_balance(&wallet_pubkey)
                        .map_err(|e| format!("Failed to get balance after WSOL creation: {}", e))?;
                    let cost = balance_before - balance_after;
                    self.logger.log(format!("✅ Step 1 SUCCESS - WSOL account created. Cost: {:.6} SOL", cost as f64 / 1_000_000_000.0).green().to_string());
                },
                Err(e) => {
                    self.logger.log(format!("❌ Step 1 FAILED - WSOL account creation failed: {}", e).red().to_string());
                    return Err(format!("Step 1 failed: {}", e));
                }
            }
        } else {
            self.logger.log("✅ Step 1 SKIPPED - WSOL account already exists".green().to_string());
        }

        // Step 2: Create target token account if needed
        if !target_token_exists {
            self.logger.log("🔧 Step 2: Creating target token account...".yellow().to_string());
            
            let balance_before = self.config.app_state.rpc_client.get_balance(&wallet_pubkey)
                .map_err(|e| format!("Failed to get balance before target token creation: {}", e))?;
            
            match self.create_target_token_account(&current_wallet, &target_token_mint).await {
                Ok(()) => {
                    let balance_after = self.config.app_state.rpc_client.get_balance(&wallet_pubkey)
                        .map_err(|e| format!("Failed to get balance after target token creation: {}", e))?;
                    let cost = balance_before - balance_after;
                    self.logger.log(format!("✅ Step 2 SUCCESS - Target token account created. Cost: {:.6} SOL", cost as f64 / 1_000_000_000.0).green().to_string());
                },
                Err(e) => {
                    self.logger.log(format!("❌ Step 2 FAILED - Target token account creation failed: {}", e).red().to_string());
                    return Err(format!("Step 2 failed: {}", e));
                }
            }
        } else {
            self.logger.log("✅ Step 2 SKIPPED - Target token account already exists".green().to_string());
        }

        // Step 3: Smart SOL/WSOL Balance Management
        self.logger.log("🔧 Step 3: Smart balance management...".yellow().to_string());
        
        let current_balance = self.config.app_state.rpc_client.get_balance(&wallet_pubkey)
            .map_err(|e| format!("Failed to get current balance: {}", e))?;
        let current_balance_f64 = current_balance as f64 / 1_000_000_000.0;
        
        // Get WSOL balance
        let wsol_balance = match self.config.app_state.rpc_client.get_account(&wsol_account) {
            Ok(account) => {
                match spl_token::state::Account::unpack(&account.data) {
                    Ok(token_account) => token_account.amount as f64 / 1_000_000_000.0,
                    Err(_) => 0.0,
                }
            },
            Err(_) => 0.0,
        };
        
        // Read balance thresholds from config (will get from environment variables)
        // TODO: Get these from global config - for now use hardcoded values
        let minimal_balance_for_fee = 0.005; // Reduced threshold - 0.005 SOL should be enough for fees
        let minimal_wsol_balance_for_trading = 0.001; // Will be read from env
        let critical_sol_threshold = 0.003; // Critical threshold - below this, definitely need to unwrap
        
        self.logger.log(format!("💰 Step 3 - SOL: {:.6}, WSOL: {:.6}, Critical SOL: {:.6}, WSOL threshold: {:.6}", 
            current_balance_f64, wsol_balance, critical_sol_threshold, minimal_wsol_balance_for_trading).cyan().to_string());
        
                if current_balance_f64 > critical_sol_threshold && wsol_balance > minimal_wsol_balance_for_trading {
            // Case 1: Sufficient SOL and WSOL - don't wrap, use existing WSOL
            self.logger.log("✅ Step 3 SKIPPED - Sufficient SOL and WSOL balances, no wrapping needed".green().to_string());
        } else if current_balance_f64 <= critical_sol_threshold && wsol_balance > minimal_wsol_balance_for_trading {
             // Case 2: Low SOL but sufficient WSOL - unwrap some WSOL to SOL for fees
             // Note: unwrapping also returns rent exemption (~0.00204 SOL), so we can unwrap less
             let needed_sol = minimal_balance_for_fee - current_balance_f64;
             let rent_exemption_bonus = 0.00204; // Approximate rent exemption we'll get back
             let unwrap_amount = (needed_sol - rent_exemption_bonus).max(0.0001); // Minimum 0.0001 WSOL unwrap
             
             self.logger.log(format!("🔄 Step 3 - Low SOL, unwrapping {:.6} WSOL to SOL for fees (will get ~{:.6} SOL total)", 
                 unwrap_amount, unwrap_amount + rent_exemption_bonus).yellow().to_string());
             
             if wsol_balance >= unwrap_amount {
                let balance_before = self.config.app_state.rpc_client.get_balance(&wallet_pubkey)
                    .map_err(|e| format!("Failed to get balance before unwrap: {}", e))?;
                
                match self.unwrap_wsol_to_sol(&current_wallet, unwrap_amount).await {
                    Ok(()) => {
                        let balance_after = self.config.app_state.rpc_client.get_balance(&wallet_pubkey)
                            .map_err(|e| format!("Failed to get balance after unwrap: {}", e))?;
                        let gained = balance_after - balance_before;
                        self.logger.log(format!("✅ Step 3 SUCCESS - WSOL unwrapped to SOL. Amount: {:.6} WSOL, SOL gained: {:.6}", 
                            unwrap_amount, gained as f64 / 1_000_000_000.0).green().to_string());
                    },
                    Err(e) => {
                        self.logger.log(format!("❌ Step 3 FAILED - WSOL unwrapping failed: {}", e).red().to_string());
                        return Err(format!("Step 3 failed: {}", e));
                    }
                }
            } else {
                return Err(format!("Insufficient WSOL for unwrapping. Need: {:.6}, Have: {:.6}", unwrap_amount, wsol_balance));
            }
        } else {
            // Case 3: Need to wrap SOL to WSOL (original logic)
            let reserve_for_fees = 0.0005; // Reserve for transaction fees
            let available_sol = current_balance_f64 - reserve_for_fees;
            
            if available_sol <= 0.0 {
                return Err(format!("Insufficient SOL for wrapping. Current: {:.6}, Reserved: {:.6}", current_balance_f64, reserve_for_fees));
            }
            
            let wrap_amount = available_sol * 0.75; // Use 75% of available SOL
            
            self.logger.log(format!("🔧 Step 3 - Wrapping {:.6} SOL to WSOL", wrap_amount).yellow().to_string());
            
            let balance_before = self.config.app_state.rpc_client.get_balance(&wallet_pubkey)
                .map_err(|e| format!("Failed to get balance before wrap: {}", e))?;
            
            match self.wrap_sol_to_wsol(&current_wallet, wrap_amount).await {
                Ok(()) => {
                    let balance_after = self.config.app_state.rpc_client.get_balance(&wallet_pubkey)
                        .map_err(|e| format!("Failed to get balance after wrap: {}", e))?;
                    let cost = balance_before - balance_after;
                    self.logger.log(format!("✅ Step 3 SUCCESS - SOL wrapped to WSOL. Amount: {:.6} SOL, Total cost: {:.6} SOL", 
                        wrap_amount, cost as f64 / 1_000_000_000.0).green().to_string());
                },
                Err(e) => {
                    self.logger.log(format!("❌ Step 3 FAILED - SOL wrapping failed: {}", e).red().to_string());
                    return Err(format!("Step 3 failed: {}", e));
                }
            }
        }

        // Step 4: Execute swap
        self.logger.log("🔧 Step 4: Executing swap...".yellow().to_string());
        
        // Get WSOL balance after balance management
        let wsol_balance_after_management = match self.config.app_state.rpc_client.get_account(&wsol_account) {
            Ok(account) => {
                match spl_token::state::Account::unpack(&account.data) {
                    Ok(token_account) => token_account.amount as f64 / 1_000_000_000.0,
                    Err(_) => 0.0,
                }
            },
            Err(_) => 0.0,
        };
        
        if wsol_balance_after_management <= 0.0 {
            return Err("No WSOL balance available for swap".to_string());
        }
        
        // Calculate buy amount based on current WSOL balance (after smart management)
        let mut rng = rand::thread_rng();
        let random_multiplier = self.config.randomization_config.min_amount_sol + 
            (self.config.randomization_config.max_amount_sol - self.config.randomization_config.min_amount_sol) * rng.gen::<f64>();
        let final_buy_amount = wsol_balance_after_management * random_multiplier;
        
        self.logger.log(format!("🎯 Step 4 - WSOL Balance: {:.6}, Multiplier: {:.3}, Buy Amount: {:.6} SOL", 
            wsol_balance_after_management, random_multiplier, final_buy_amount).cyan().to_string());
        
        // Create swap configuration
        let swap_config = SwapConfig {
            mint: self.config.target_token_mint.clone(),
            swap_direction: SwapDirection::Buy,
            in_type: SwapInType::Qty,
            amount_in: final_buy_amount,
            slippage: self.config.slippage,
            max_buy_amount: final_buy_amount,
        };

        // Create RaydiumCPMM instance with current wallet
        let raydium_cpmm = RaydiumCPMM::new(
            current_wallet.clone(),
            Some(self.config.app_state.rpc_client.clone()),
            Some(self.config.app_state.rpc_nonblocking_client.clone()),
        );

        // Build and execute swap
        let balance_before = self.config.app_state.rpc_client.get_balance(&wallet_pubkey)
            .map_err(|e| format!("Failed to get balance before swap: {}", e))?;
        
        match raydium_cpmm.build_swap_from_default_info(swap_config).await {
            Ok((keypair, instructions, token_price)) => {
                self.logger.log(format!("Token price: ${:.8}", token_price));
                
                // Get recent blockhash for the skip simulation transaction
                let recent_blockhash = self.config.app_state.rpc_client.get_latest_blockhash()
                    .map_err(|e| format!("Failed to get recent blockhash: {}", e))?;
                
                // Use the new skip simulation function for testing on-chain behavior
                match crate::core::tx::new_signed_and_send_skip_simulation_force(
                    self.config.app_state.rpc_nonblocking_client.clone(),
                    recent_blockhash,
                    &keypair,
                    instructions,
                    &self.logger,
                ).await {
                    Ok(signatures) => {
                        let signature = signatures[0];
                        let balance_after = self.config.app_state.rpc_client.get_balance(&wallet_pubkey)
                            .map_err(|e| format!("Failed to get balance after swap: {}", e))?;
                        let cost = balance_before - balance_after;
                        
                        self.logger.log(format!("✅ Step 4 SUCCESS - Swap executed with SKIP SIMULATION. Amount: {:.6} SOL, Cost: {:.6} SOL, Signature: {}", 
                            final_buy_amount, cost as f64 / 1_000_000_000.0, signature).green().to_string());
                        
                        // Update trade tracking
                        {
                            let mut recent_trades = self.recent_trades.lock().await;
                            recent_trades.push_back(TradeType::Buy);
                            if recent_trades.len() > 20 {
                                recent_trades.pop_front();
                            }
                        }

                        {
                            let mut trade_counter = self.trade_counter.lock().await;
                            *trade_counter += 1;
                        }

                        {
                            let mut wallet_change_counter = self.wallet_change_counter.lock().await;
                            *wallet_change_counter += 1;
                        }

                        self.logger.log(format!(
                            "🎉 DEBUG BUY COMPLETED with SKIP SIMULATION! Total time: {:?}",
                            start_time.elapsed()
                        ).green().bold().to_string());
                        
                        Ok(signature)
                    },
                    Err(e) => {
                        self.logger.log(format!("❌ Step 4 FAILED - ON-CHAIN transaction failed (this is the real error): {}", e).red().to_string());
                        Err(format!("Step 4 failed: {}", e))
                    }
                }
            },
            Err(e) => {
                self.logger.log(format!("❌ Step 4 FAILED - Swap building failed: {}", e).red().to_string());
                Err(format!("Step 4 failed: {}", e))
            }
        }
    }

    /// Execute an advanced buy transaction with the current wallet
    async fn execute_advanced_buy(&self, _amount_sol: f64) -> Result<Signature, String> {
        let start_time = Instant::now();
        
        let current_wallet = {
            let current_wallet = self.current_wallet.lock().await;
            current_wallet.clone().ok_or("No current wallet set")?
        };

        let wallet_pubkey = current_wallet.pubkey();
        
        // Get current SOL balance
        let sol_balance = self.config.app_state.rpc_client.get_balance(&wallet_pubkey)
            .map_err(|e| format!("Failed to get SOL balance: {}", e))?;
        let sol_balance_f64 = sol_balance as f64 / 1_000_000_000.0;
        
        // Check if we have enough SOL for operations
        if sol_balance_f64 < 0.002 {
            return Err(format!("Insufficient SOL balance: {} SOL", sol_balance_f64));
        }
        
        // Calculate amount to wrap to WSOL (85% of available SOL, keeping 15% for fees)
        let fee_reserve = 0.0015; // Reserve for transaction fees
        let available_sol = sol_balance_f64 - fee_reserve;
        let wrap_amount = if available_sol > 0.0 {
            available_sol * 0.85 // Wrap 85% of available SOL
        } else {
            return Err("Insufficient SOL for wrapping".to_string());
        };
        
        // Calculate buy amount based on WSOL balance (after wrapping, WSOL balance = wrap_amount)
        // Apply randomization ratio directly to the WSOL balance
        let wsol_balance_after_wrap = wrap_amount; // This will be the WSOL balance after wrapping
        
        // Get ratio range from config (these are ratios between 0 and 1)
        let min_ratio = self.config.randomization_config.min_amount_sol.max(0.1).min(1.0);
        let max_ratio = self.config.randomization_config.max_amount_sol.max(min_ratio).min(1.0);
        
        let mut rng = rand::thread_rng();
        let random_multiplier = min_ratio + (max_ratio - min_ratio) * rng.gen::<f64>();
        let final_buy_amount = wsol_balance_after_wrap * random_multiplier *0.1; // for me to see what happend 
        
        // Get WSOL and target token account addresses
        let wsol_account = get_associated_token_address(&wallet_pubkey, &spl_token::native_mint::id());
        let target_token_mint = Pubkey::from_str(&self.config.target_token_mint)
            .map_err(|e| format!("Invalid target token mint: {}", e))?;
        let target_token_account = get_associated_token_address(&wallet_pubkey, &target_token_mint);
        
        // Check if accounts exist
        let wsol_exists = self.config.app_state.rpc_client.get_account(&wsol_account).is_ok();
        let target_token_exists = self.config.app_state.rpc_client.get_account(&target_token_account).is_ok();
        
        // Start building instructions
        let mut instructions = Vec::new();
        
        // Create WSOL account if needed
        if !wsol_exists {
            let create_wsol_instruction = spl_associated_token_account::instruction::create_associated_token_account(
                &wallet_pubkey,  // payer
                &wallet_pubkey,  // owner
                &spl_token::native_mint::id(), // mint
                &spl_token::id(), // token program
            );
            instructions.push(create_wsol_instruction);
            self.logger.log("🔧 Added WSOL account creation instruction".yellow().to_string());
        }
        
        // Create target token account if needed
        if !target_token_exists {
            let create_target_token_instruction = spl_associated_token_account::instruction::create_associated_token_account(
                &wallet_pubkey,  // payer
                &wallet_pubkey,  // owner
                &target_token_mint, // mint
                &spl_token::id(), // token program
            );
            instructions.push(create_target_token_instruction);
            self.logger.log("🔧 Added target token account creation instruction".yellow().to_string());
        }
        
        // Wrap SOL to WSOL
        let wrap_lamports = (wrap_amount * 1_000_000_000.0) as u64;
        instructions.push(
            system_instruction::transfer(
                &wallet_pubkey,
                &wsol_account,
                wrap_lamports,
            )
        );
        instructions.push(
            sync_native(&spl_token::id(), &wsol_account)
                .map_err(|e| format!("Failed to create sync native instruction: {}", e))?
        );
        
        self.logger.log(format!("💰 SOL Balance: {:.6}, Available: {:.6}, Wrap: {:.6} SOL", 
            sol_balance_f64, available_sol, wrap_amount).cyan().to_string());
        self.logger.log(format!("🎯 Buy calculation: WSOL({:.6}) * {:.3} = {:.6} SOL", 
            wsol_balance_after_wrap, random_multiplier, final_buy_amount).cyan().to_string());
        self.logger.log(format!("🔥 STEALTH BUY - Wrap: {:.6} SOL, Buy: {:.6} SOL - Wallet: {}", 
            wrap_amount, final_buy_amount, wallet_pubkey).green().bold().to_string());
        
        // Create swap configuration
        let swap_config = SwapConfig {
            mint: self.config.target_token_mint.clone(),
            swap_direction: SwapDirection::Buy,
            in_type: SwapInType::Qty,
            amount_in: final_buy_amount,
            slippage: self.config.slippage,
            max_buy_amount: final_buy_amount,
        };

        // Create RaydiumCPMM instance with current wallet
        let raydium_cpmm = RaydiumCPMM::new(
            current_wallet.clone(),
            Some(self.config.app_state.rpc_client.clone()),
            Some(self.config.app_state.rpc_nonblocking_client.clone()),
        );

        // Build swap instructions only (not the full transaction)
        let (_, swap_instructions, token_price) = raydium_cpmm
            .build_swap_from_default_info(swap_config)
            .await
            .map_err(|e| format!("Failed to build buy transaction: {}", e))?;

        self.logger.log(format!("Token price: ${:.8}", token_price));
        
        // Add swap instructions to our combined transaction
        instructions.extend(swap_instructions);
        
        // Send the combined transaction
        let recent_blockhash = self.config.app_state.rpc_client.get_latest_blockhash()
            .map_err(|e| format!("Failed to get recent blockhash: {}", e))?;

        let transaction = Transaction::new_signed_with_payer(
            &instructions,
            Some(&wallet_pubkey),
            &[current_wallet.as_ref()],
            recent_blockhash,
        );

        let signature = self.config.app_state.rpc_client.send_and_confirm_transaction(&transaction)
            .map_err(|e| format!("Failed to send combined transaction: {}", e))?;

        // Update trade tracking
        {
            let mut recent_trades = self.recent_trades.lock().await;
            recent_trades.push_back(TradeType::Buy);
            if recent_trades.len() > 20 {
                recent_trades.pop_front();
            }
        }

        {
            let mut trade_counter = self.trade_counter.lock().await;
            *trade_counter += 1;
        }

        {
            let mut wallet_change_counter = self.wallet_change_counter.lock().await;
            *wallet_change_counter += 1;
        }

        self.logger.log(format!(
            "✅ STEALTH BUY SUCCESS! Wrapped: {:.6} SOL → WSOL, Used: {:.6} SOL ({:.1}%), Signature: {}, Time: {:?}",
            wrap_amount, final_buy_amount, (final_buy_amount / wrap_amount * 100.0), signature, start_time.elapsed()
        ).green().bold().to_string());

        Ok(signature)
    }

    /// Execute an advanced sell transaction with the current wallet
    async fn execute_advanced_sell(&self, percentage: f64) -> Result<Signature, String> {
        let start_time = Instant::now();
        
        let current_wallet = {
            let current_wallet = self.current_wallet.lock().await;
            current_wallet.clone().ok_or("No current wallet set")?
        };

        // Check and prepare wallet (SOL, WSOL, Token balances)
        self.check_and_prepare_wallet(&current_wallet).await?;

        // Log wallet and WSOL account before trading
        let wsol_account = get_associated_token_address(&current_wallet.pubkey(), &spl_token::native_mint::id());
        self.logger.log(format!("🔥 STEALTH SELL - Percentage: {:.1}% - Wallet: {} - WSOL: {}", 
            percentage * 100.0, current_wallet.pubkey(), wsol_account).blue().bold().to_string());

        let swap_config = SwapConfig {
            mint: self.config.target_token_mint.clone(),
            swap_direction: SwapDirection::Sell,
            in_type: SwapInType::Pct,
            amount_in: percentage,
            slippage: self.config.slippage,
            max_buy_amount: 0.0,
        };

        // Create RaydiumCPMM instance with current wallet
        let raydium_cpmm = RaydiumCPMM::new(
            current_wallet.clone(),
            Some(self.config.app_state.rpc_client.clone()),
            Some(self.config.app_state.rpc_nonblocking_client.clone()),
        );

        // Build swap transaction
        let (keypair, instructions, token_price) = raydium_cpmm
            .build_swap_from_default_info(swap_config)
            .await
            .map_err(|e| format!("Failed to build sell transaction: {}", e))?;

        self.logger.log(format!("Token price: ${:.8}", token_price));

        // Send transaction
        let signature = self.send_transaction(&keypair, instructions).await
            .map_err(|e| format!("Failed to send sell transaction: {}", e))?;

        // Update trade tracking
        {
            let mut recent_trades = self.recent_trades.lock().await;
            recent_trades.push_back(TradeType::Sell);
            if recent_trades.len() > 20 {
                recent_trades.pop_front();
            }
        }

        {
            let mut trade_counter = self.trade_counter.lock().await;
            *trade_counter += 1;
        }

        {
            let mut wallet_change_counter = self.wallet_change_counter.lock().await;
            *wallet_change_counter += 1;
        }

        self.logger.log(format!(
            "✅ STEALTH SELL SUCCESS! Percentage: {:.1}%, Signature: {}, Time: {:?}",
            percentage * 100.0, signature, start_time.elapsed()
        ).blue().bold().to_string());

        Ok(signature)
    }

    /// Start GRPC monitoring for the target token
    async fn start_grpc_monitoring(&self) -> Result<(), String> {
        self.logger.log("🔍 Starting GRPC token monitoring...".cyan().to_string());

        // Connect to Yellowstone gRPC
        let mut client = GeyserGrpcClient::build_from_shared(self.config.yellowstone_grpc_http.clone())
            .map_err(|e| format!("Failed to build GRPC client: {}", e))?
            .x_token::<String>(Some(self.config.yellowstone_grpc_token.clone()))
            .map_err(|e| format!("Failed to set x_token: {}", e))?
            .tls_config(ClientTlsConfig::new().with_native_roots())
            .map_err(|e| format!("Failed to set tls config: {}", e))?
            .connect()
            .await
            .map_err(|e| format!("Failed to connect to GRPC: {}", e))?;

        // Set up subscription
        let mut retry_count = 0;
        const MAX_RETRIES: u32 = 3;
        let (subscribe_tx, mut stream) = loop {
            match client.subscribe().await {
                Ok(pair) => break pair,
                Err(e) => {
                    retry_count += 1;
                    if retry_count >= MAX_RETRIES {
                        return Err(format!("Failed to subscribe after {} attempts: {}", MAX_RETRIES, e));
                    }
                    self.logger.log(format!(
                        "[CONNECTION ERROR] => Failed to subscribe (attempt {}/{}): {}. Retrying in 5 seconds...",
                        retry_count, MAX_RETRIES, e
                    ).red().to_string());
                    time::sleep(Duration::from_secs(5)).await;
                }
            }
        };

        let subscribe_tx = Arc::new(tokio::sync::Mutex::new(subscribe_tx));

        // Set up subscription for target token
        let subscription_request = SubscribeRequest {
            transactions: maplit::hashmap! {
                "TargetToken".to_owned() => SubscribeRequestFilterTransactions {
                    vote: Some(false),
                    failed: Some(false),
                    signature: None,
                    account_include: vec!["CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C".to_string()],
                    account_exclude: vec![JUPITER_PROGRAM.to_string(), OKX_DEX_PROGRAM.to_string()],
                    account_required: Vec::<String>::new(),
                }
            },
            commitment: Some(CommitmentLevel::Processed as i32),
            ..Default::default()
        };

        subscribe_tx
            .lock()
            .await
            .send(subscription_request)
            .await
            .map_err(|e| format!("Failed to send subscribe request: {}", e))?;

        // Spawn heartbeat task
        let subscribe_tx_clone = subscribe_tx.clone();
        tokio::spawn(async move {
            loop {
                time::sleep(Duration::from_secs(30)).await;
                if let Err(e) = send_heartbeat_ping(&subscribe_tx_clone).await {
                    eprintln!("Heartbeat ping failed: {}", e);
                }
            }
        });

        // Process incoming messages
        self.logger.log("✅ GRPC monitoring started, processing transactions...".green().to_string());
        while let Some(msg) = stream.next().await {
            if !self.is_running().await {
                break;
            }

            match msg {
                Ok(msg) => {
                    if let Err(e) = self.process_grpc_message(&msg).await {
                        self.logger.log(format!("Error processing message: {}", e).red().to_string());
                    }
                },
                Err(e) => {
                    self.logger.log(format!("Stream error: {}", e).red().to_string());
                    break;
                }
            }
        }

        Ok(())
    }

    /// Check and prepare wallet for trading (check balances, create/wrap WSOL if needed)
    async fn check_and_prepare_wallet(&self, wallet: &Arc<anchor_client::solana_sdk::signature::Keypair>) -> Result<(), String> {
        let wallet_pubkey = wallet.pubkey();
        
        // Log current trading wallet
        self.logger.log(format!("🔍 Current trading wallet: {}", wallet_pubkey).cyan().to_string());

        // Get SOL balance
        let sol_balance = self.config.app_state.rpc_client.get_balance(&wallet_pubkey)
            .map_err(|e| format!("Failed to get SOL balance: {}", e))?;
        let sol_balance_f64 = sol_balance as f64 / 1_000_000_000.0;
        
        // Get WSOL account address
        let wsol_account = get_associated_token_address(&wallet_pubkey, &spl_token::native_mint::id());
        
        // Log WSOL account
        self.logger.log(format!("🔍 WSOL account: {}", wsol_account).cyan().to_string());
        
        // Check if WSOL account exists and get balance
        let (wsol_exists, wsol_balance) = match self.config.app_state.rpc_client.get_account(&wsol_account) {
            Ok(account) => {
                match spl_token::state::Account::unpack(&account.data) {
                    Ok(token_account) => {
                        let balance = token_account.amount as f64 / 1_000_000_000.0;
                        self.logger.log(format!("💰 WSOL balance: {} SOL", balance).green().to_string());
                        (true, balance)
                    },
                    Err(_) => {
                        self.logger.log("❌ WSOL account exists but couldn't parse data".red().to_string());
                        (false, 0.0)
                    }
                }
            },
            Err(_) => {
                self.logger.log("❌ WSOL account doesn't exist".red().to_string());
                (false, 0.0)
            }
        };

        // Get target token balance
        let target_token_mint = Pubkey::from_str(&self.config.target_token_mint)
            .map_err(|e| format!("Invalid target token mint: {}", e))?;
        let target_token_account = get_associated_token_address(&wallet_pubkey, &target_token_mint);
        
        let (target_token_exists, target_token_balance) = match self.config.app_state.rpc_client.get_account(&target_token_account) {
            Ok(account) => {
                match spl_token::state::Account::unpack(&account.data) {
                    Ok(token_account) => {
                        let balance = token_account.amount;
                        self.logger.log(format!("🎯 Target token balance: {}", balance).green().to_string());
                        (true, balance)
                    },
                    Err(_) => {
                        self.logger.log("❌ Target token account exists but couldn't parse data".red().to_string());
                        (false, 0)
                    }
                }
            },
            Err(_) => {
                self.logger.log("❌ Target token account doesn't exist".red().to_string());
                (false, 0)
            }
        };

        // Log all balances
        self.logger.log(format!("💰 Wallet balances - SOL: {:.6}, WSOL: {:.6}, Token: {}", 
            sol_balance_f64, wsol_balance, target_token_balance).purple().to_string());

        // Create WSOL account if it doesn't exist
        if !wsol_exists {
            self.logger.log("🔧 Creating WSOL account...".yellow().to_string());
            if let Err(e) = self.create_wsol_account_only(wallet).await {
                self.logger.log(format!("❌ Failed to create WSOL account: {}", e).red().to_string());
                return Err(format!("Failed to create WSOL account: {}", e));
            }
            self.logger.log("✅ WSOL account created successfully".green().to_string());
        }

        // Create target token account if it doesn't exist
        if !target_token_exists {
            self.logger.log("🔧 Creating target token account...".yellow().to_string());
            if let Err(e) = self.create_target_token_account(wallet, &target_token_mint).await {
                self.logger.log(format!("❌ Failed to create target token account: {}", e).red().to_string());
                return Err(format!("Failed to create target token account: {}", e));
            }
            self.logger.log("✅ Target token account created successfully".green().to_string());
        }

        // Check if we need to wrap SOL to WSOL
        if wsol_balance < 0.01 && sol_balance_f64 > 0.05 {
            // Calculate amount to wrap based on user's requirements
            // If we have SOL balance similar to the user's (0.001205), wrap 85% of it
            let fee_reserve = 0.0005; // Reserve for transaction fees
            let available_sol = sol_balance_f64 - fee_reserve;
            let wrap_amount = if available_sol > 0.001 {
                available_sol * 0.85 // Wrap 85% of available SOL
            } else {
                // Fallback to old logic for very small amounts
                (sol_balance_f64 - 0.01) * 0.75
            };
            
            if wrap_amount > 0.0005 {
                self.logger.log(format!("🔄 Wrapping {} SOL to WSOL (85% of available balance)", wrap_amount).yellow().to_string());
                
                // Wrap SOL to WSOL
                if let Err(e) = self.wrap_sol_to_wsol(wallet, wrap_amount).await {
                    self.logger.log(format!("❌ Failed to wrap SOL to WSOL: {}", e).red().to_string());
                    return Err(format!("Failed to wrap SOL to WSOL: {}", e));
                }
                
                self.logger.log(format!("✅ Successfully wrapped {} SOL to WSOL", wrap_amount).green().to_string());
            }
        }

        Ok(())
    }

    /// Create WSOL account and wrap SOL
    async fn create_and_wrap_wsol(&self, wallet: &Arc<anchor_client::solana_sdk::signature::Keypair>, amount: f64) -> Result<(), String> {
        let wallet_pubkey = wallet.pubkey();
        
        // Create WSOL account instructions
        let (wsol_account, mut instructions) = token::create_wsol_account(wallet_pubkey)
            .map_err(|e| format!("Failed to create WSOL account instructions: {}", e))?;
        
        // Convert to lamports
        let lamports = (amount * 1_000_000_000.0) as u64;
        
        // Transfer SOL to the WSOL account
        instructions.push(
            system_instruction::transfer(
                &wallet_pubkey,
                &wsol_account,
                lamports,
            )
        );
        
        // Sync native instruction
        instructions.push(
            sync_native(&spl_token::id(), &wsol_account)
                .map_err(|e| format!("Failed to create sync native instruction: {}", e))?
        );
        
        // Send transaction
        let recent_blockhash = self.config.app_state.rpc_client.get_latest_blockhash()
            .map_err(|e| format!("Failed to get recent blockhash: {}", e))?;
        
        let transaction = Transaction::new_signed_with_payer(
            &instructions,
            Some(&wallet_pubkey),
            &[wallet],
            recent_blockhash,
        );
        
        let signature = self.config.app_state.rpc_client.send_and_confirm_transaction(&transaction)
            .map_err(|e| format!("Failed to send WSOL wrap transaction: {}", e))?;
        
        self.logger.log(format!("✅ WSOL wrap transaction sent: {}", signature).green().to_string());
        
        Ok(())
    }

    /// Create WSOL account only (without wrapping)
    async fn create_wsol_account_only(&self, wallet: &Arc<anchor_client::solana_sdk::signature::Keypair>) -> Result<(), String> {
        let wallet_pubkey = wallet.pubkey();
        
        // Create WSOL account instructions
        let (wsol_account, instructions) = token::create_wsol_account(wallet_pubkey)
            .map_err(|e| format!("Failed to create WSOL account instructions: {}", e))?;
        
        // Send transaction
        let recent_blockhash = self.config.app_state.rpc_client.get_latest_blockhash()
            .map_err(|e| format!("Failed to get recent blockhash: {}", e))?;
        
        let transaction = Transaction::new_signed_with_payer(
            &instructions,
            Some(&wallet_pubkey),
            &[wallet],
            recent_blockhash,
        );
        
        let signature = self.config.app_state.rpc_client.send_and_confirm_transaction(&transaction)
            .map_err(|e| format!("Failed to send WSOL account creation transaction: {}", e))?;
        
        self.logger.log(format!("✅ WSOL account created: {} - Signature: {}", wsol_account, signature).green().to_string());
        
        Ok(())
    }

    /// Create target token account
    async fn create_target_token_account(&self, wallet: &Arc<anchor_client::solana_sdk::signature::Keypair>, token_mint: &Pubkey) -> Result<(), String> {
        let wallet_pubkey = wallet.pubkey();
        
        // Create associated token account instruction
        let create_ata_instruction = spl_associated_token_account::instruction::create_associated_token_account(
            &wallet_pubkey,  // payer
            &wallet_pubkey,  // owner
            token_mint,      // mint
            &spl_token::id(), // token program
        );
        
        // Send transaction
        let recent_blockhash = self.config.app_state.rpc_client.get_latest_blockhash()
            .map_err(|e| format!("Failed to get recent blockhash: {}", e))?;
        
        let transaction = Transaction::new_signed_with_payer(
            &[create_ata_instruction],
            Some(&wallet_pubkey),
            &[wallet],
            recent_blockhash,
        );
        
        let signature = self.config.app_state.rpc_client.send_and_confirm_transaction(&transaction)
            .map_err(|e| format!("Failed to send target token account creation transaction: {}", e))?;
        
        let target_token_account = get_associated_token_address(&wallet_pubkey, token_mint);
        self.logger.log(format!("✅ Target token account created: {} - Signature: {}", target_token_account, signature).green().to_string());
        
        Ok(())
    }

    /// Wrap SOL to WSOL (assuming WSOL account already exists)
    async fn wrap_sol_to_wsol(&self, wallet: &Arc<anchor_client::solana_sdk::signature::Keypair>, amount: f64) -> Result<(), String> {
        let wallet_pubkey = wallet.pubkey();
        let wsol_account = get_associated_token_address(&wallet_pubkey, &spl_token::native_mint::id());
        
        // Convert to lamports
        let lamports = (amount * 1_000_000_000.0) as u64;
        
        let mut instructions = Vec::new();
        
        // Transfer SOL to the WSOL account
        instructions.push(
            system_instruction::transfer(
                &wallet_pubkey,
                &wsol_account,
                lamports,
            )
        );
        
        // Sync native instruction
        instructions.push(
            sync_native(&spl_token::id(), &wsol_account)
                .map_err(|e| format!("Failed to create sync native instruction: {}", e))?
        );
        
        // Send transaction
        let recent_blockhash = self.config.app_state.rpc_client.get_latest_blockhash()
            .map_err(|e| format!("Failed to get recent blockhash: {}", e))?;
        
        let transaction = Transaction::new_signed_with_payer(
            &instructions,
            Some(&wallet_pubkey),
            &[wallet],
            recent_blockhash,
        );
        
        let signature = self.config.app_state.rpc_client.send_and_confirm_transaction(&transaction)
            .map_err(|e| format!("Failed to send SOL wrap transaction: {}", e))?;
        
        self.logger.log(format!("✅ SOL wrapped to WSOL: {} - Signature: {}", amount, signature).green().to_string());
        
        Ok(())
    }

    /// Unwrap WSOL to SOL (for getting SOL back when needed for fees)
    async fn unwrap_wsol_to_sol(&self, wallet: &Arc<anchor_client::solana_sdk::signature::Keypair>, amount: f64) -> Result<(), String> {
        let wallet_pubkey = wallet.pubkey();
        let wsol_account = get_associated_token_address(&wallet_pubkey, &spl_token::native_mint::id());
        
        // Convert to lamports  
        let lamports_to_unwrap = (amount * 1_000_000_000.0) as u64;
        
        let mut instructions = Vec::new();
        
        // Get the minimum balance required for rent exemption of a token account
        let rent_exempt_lamports = self.config.app_state.rpc_client
            .get_minimum_balance_for_rent_exemption(spl_token::state::Account::LEN)
            .map_err(|e| format!("Failed to get rent exemption amount: {}", e))?;
        
        // We need to transfer the unwrap amount + rent exempt amount to create a valid account
        let total_lamports_needed = lamports_to_unwrap + rent_exempt_lamports;
        
        // Create a temporary WSOL account that will be properly funded
        let temp_account = anchor_client::solana_sdk::signature::Keypair::new();
        
        // Create the temporary account with proper rent-exempt amount
        instructions.push(
            system_instruction::create_account(
                &wallet_pubkey,
                &temp_account.pubkey(),
                rent_exempt_lamports, // Use rent-exempt amount for account creation
                spl_token::state::Account::LEN as u64,
                &spl_token::id(),
            )
        );
        
        // Initialize the temporary account
        instructions.push(
            spl_token::instruction::initialize_account(
                &spl_token::id(),
                &temp_account.pubkey(),
                &spl_token::native_mint::id(),
                &wallet_pubkey,
            ).map_err(|e| format!("Failed to create initialize account instruction: {}", e))?
        );
        
        // Transfer WSOL tokens to the temporary account
        instructions.push(
            spl_token::instruction::transfer(
                &spl_token::id(),
                &wsol_account,
                &temp_account.pubkey(),
                &wallet_pubkey,
                &[&wallet_pubkey],
                lamports_to_unwrap, // Only transfer the amount we want to unwrap
            ).map_err(|e| format!("Failed to create transfer instruction: {}", e))?
        );
        
        // Sync native (this converts the transferred WSOL tokens to SOL in the account)
        instructions.push(
            sync_native(&spl_token::id(), &temp_account.pubkey())
                .map_err(|e| format!("Failed to create sync native instruction: {}", e))?
        );
        
        // Close the temporary account (this releases ALL SOL to the wallet, including unwrapped amount + rent)
        instructions.push(
            spl_token::instruction::close_account(
                &spl_token::id(),
                &temp_account.pubkey(),
                &wallet_pubkey, // destination (where SOL goes)
                &wallet_pubkey, // owner
                &[&wallet_pubkey],
            ).map_err(|e| format!("Failed to create close account instruction: {}", e))?
        );
        
        // Send transaction
        let recent_blockhash = self.config.app_state.rpc_client.get_latest_blockhash()
            .map_err(|e| format!("Failed to get recent blockhash: {}", e))?;
        
        let transaction = Transaction::new_signed_with_payer(
            &instructions,
            Some(&wallet_pubkey),
            &[wallet, &temp_account],
            recent_blockhash,
        );
        
        let signature = self.config.app_state.rpc_client.send_and_confirm_transaction(&transaction)
            .map_err(|e| format!("Failed to send WSOL unwrap transaction: {}", e))?;
        
        self.logger.log(format!("✅ WSOL unwrapped to SOL: {:.6} WSOL + rent ({:.6} SOL total) - Signature: {}", 
            amount, (rent_exempt_lamports as f64 / 1_000_000_000.0), signature).green().to_string());
        
        Ok(())
    }

    /// Process incoming GRPC messages
    async fn process_grpc_message(&self, msg: &SubscribeUpdate) -> Result<(), String> {
        if let Some(update_oneof) = &msg.update_oneof {
            if let UpdateOneof::Transaction(txn_info) = update_oneof {
                // Parse the transaction for our target token
                if let Some(trade_info) = parse_target_token_transaction(txn_info, &self.config.target_token_mint) {
                    self.logger.log(format!(
                        "🎯 Detected {} trade: User: {}, Volume: {:.6} SOL",
                        if trade_info.is_buy { "BUY" } else { "SELL" },
                        trade_info.user,
                        trade_info.volume_change
                    ).magenta().to_string());
                    
                    // Add to activity tracking for analysis
                    let activity = TokenActivity {
                        timestamp: Instant::now(),
                        is_buy: trade_info.is_buy,
                        volume_sol: trade_info.volume_change,
                        user: trade_info.user.clone(),
                        price: 0.0, // Would need to calculate from pool reserves
                    };
                    self.add_token_activity(activity).await;
                }
            }
        }
        Ok(())
    }

    /// Send transaction to the network
    async fn send_transaction(
        &self,
        keypair: &Arc<anchor_client::solana_sdk::signature::Keypair>,
        instructions: Vec<anchor_client::solana_sdk::instruction::Instruction>,
    ) -> Result<Signature, String> {
        use anchor_client::solana_sdk::transaction::Transaction;
        use anchor_client::solana_sdk::signer::Signer;

        // Get recent blockhash
        let recent_blockhash = self.config.app_state.rpc_client
            .get_latest_blockhash()
            .map_err(|e| format!("Failed to get recent blockhash: {}", e))?;

        // Create and sign transaction
        let transaction = Transaction::new_signed_with_payer(
            &instructions,
            Some(&keypair.pubkey()),
            &[keypair.as_ref()],
            recent_blockhash,
        );

        // Send transaction
        let signature = self.config.app_state.rpc_client
            .send_and_confirm_transaction(&transaction)
            .map_err(|e| format!("Failed to send transaction: {}", e))?;

        Ok(signature)
    }

    /// Get trading statistics
    pub async fn get_trading_stats(&self) -> (u32, usize, HashMap<String, u32>) {
        let trade_count = *self.trade_counter.lock().await;
        let wallet_count = {
            let wallet_pool = self.wallet_pool.lock().await;
            wallet_pool.wallet_count()
        };
        let usage_stats = {
            let wallet_pool = self.wallet_pool.lock().await;
            wallet_pool.get_usage_stats()
        };
        
        (trade_count, wallet_count, usage_stats)
    }

    /// Calculate stealth buy amount based on WSOL balance
    async fn calculate_stealth_buy_amount(&self, wallet: &Arc<anchor_client::solana_sdk::signature::Keypair>) -> Result<f64, String> {
        let wallet_pubkey = wallet.pubkey();
        
        // Get WSOL account address
        let wsol_account = get_associated_token_address(&wallet_pubkey, &spl_token::native_mint::id());
        
        // Get WSOL balance
        let wsol_balance = match self.config.app_state.rpc_client.get_account(&wsol_account) {
            Ok(account) => {
                match spl_token::state::Account::unpack(&account.data) {
                    Ok(token_account) => {
                        token_account.amount as f64 / 1_000_000_000.0
                    },
                    Err(_) => 0.0
                }
            },
            Err(_) => 0.0
        };
        
        if wsol_balance < 0.0001 {
            return Err("Insufficient WSOL balance for stealth buy".to_string());
        }
        
        // Calculate stealth buy amount: wsol_balance * 0.85 * random_range
        let base_amount = wsol_balance * 0.85;
        
        // Apply random multiplier from environment range
        let min_ratio = self.config.randomization_config.min_amount_sol;
        let max_ratio = self.config.randomization_config.max_amount_sol;
        
        let mut rng = rand::thread_rng();
        let random_multiplier = min_ratio + (max_ratio - min_ratio) * rng.gen::<f64>();
        
        let stealth_amount = base_amount * random_multiplier;
        
        // Ensure we don't exceed available WSOL balance
        let max_safe_amount = wsol_balance * 0.95; // Leave 5% buffer
        let final_amount = stealth_amount.min(max_safe_amount);
        
        self.logger.log(format!("💰 Stealth buy calculation: WSOL: {:.6}, Base: {:.6}, Multiplier: {:.3}, Final: {:.6}", 
            wsol_balance, base_amount, random_multiplier, final_amount).cyan().to_string());
        
        Ok(final_amount)
    }

    /// Generate token activity analysis report
    pub async fn generate_activity_report(&self) -> TokenActivityReport {
        let activities = self.token_activities.lock().await;
        let now = Instant::now();
        
        // Filter activities from the last hour
        let recent_activities: Vec<_> = activities
            .iter()
            .filter(|activity| now.duration_since(activity.timestamp).as_secs() <= 3600)
            .collect();
        
        if recent_activities.is_empty() {
            return TokenActivityReport {
                report_period_minutes: 60,
                ..Default::default()
            };
        }
        
        let total_trades = recent_activities.len() as u32;
        let buy_trades = recent_activities.iter().filter(|a| a.is_buy).count() as u32;
        let sell_trades = total_trades - buy_trades;
        
        let total_volume_sol: f64 = recent_activities.iter().map(|a| a.volume_sol).sum();
        let buy_volume_sol: f64 = recent_activities.iter()
            .filter(|a| a.is_buy)
            .map(|a| a.volume_sol)
            .sum();
        let sell_volume_sol = total_volume_sol - buy_volume_sol;
        
        let prices: Vec<f64> = recent_activities.iter().map(|a| a.price).collect();
        let average_price = prices.iter().sum::<f64>() / prices.len() as f64;
        let min_price = prices.iter().fold(f64::INFINITY, |a, &b| a.min(b));
        let max_price = prices.iter().fold(f64::NEG_INFINITY, |a, &b| a.max(b));
        
        let unique_traders = recent_activities
            .iter()
            .map(|a| &a.user)
            .collect::<std::collections::HashSet<_>>()
            .len() as u32;
        
        TokenActivityReport {
            total_trades,
            buy_trades,
            sell_trades,
            total_volume_sol,
            buy_volume_sol,
            sell_volume_sol,
            average_price,
            min_price: if min_price == f64::INFINITY { 0.0 } else { min_price },
            max_price: if max_price == f64::NEG_INFINITY { 0.0 } else { max_price },
            unique_traders,
            report_period_minutes: 60,
        }
    }
    
    /// Log activity report if enough time has passed
    pub async fn check_and_log_activity_report(&self) {
        let now = Instant::now();
        let should_report = {
            let mut last_report = self.last_activity_report.lock().await;
            if now.duration_since(*last_report).as_secs() >= 1800 { // 30 minutes
                *last_report = now;
                true
            } else {
                false
            }
        };
        
        if should_report {
            let report = self.generate_activity_report().await;
            self.log_activity_report(&report).await;
        }
    }
    
    /// Log the activity report with detailed statistics
    pub async fn log_activity_report(&self, report: &TokenActivityReport) {
        self.logger.log("📊 ═══════════════════════════════════════════════".cyan().bold().to_string());
        self.logger.log("📊 TOKEN ACTIVITY ANALYSIS REPORT (Last 60 minutes)".cyan().bold().to_string());
        self.logger.log("📊 ═══════════════════════════════════════════════".cyan().bold().to_string());
        
        // Trade Statistics
        self.logger.log(format!("🔢 Total Trades: {}", report.total_trades).green().to_string());
        self.logger.log(format!("📈 Buy Trades: {} ({:.1}%)", 
            report.buy_trades, 
            if report.total_trades > 0 { (report.buy_trades as f64 / report.total_trades as f64) * 100.0 } else { 0.0 }
        ).green().to_string());
        self.logger.log(format!("📉 Sell Trades: {} ({:.1}%)", 
            report.sell_trades,
            if report.total_trades > 0 { (report.sell_trades as f64 / report.total_trades as f64) * 100.0 } else { 0.0 }
        ).red().to_string());
        
        // Volume Statistics
        self.logger.log(format!("💰 Total Volume: {:.6} SOL", report.total_volume_sol).cyan().to_string());
        self.logger.log(format!("💚 Buy Volume: {:.6} SOL ({:.1}%)", 
            report.buy_volume_sol,
            if report.total_volume_sol > 0.0 { (report.buy_volume_sol / report.total_volume_sol) * 100.0 } else { 0.0 }
        ).green().to_string());
        self.logger.log(format!("💔 Sell Volume: {:.6} SOL ({:.1}%)", 
            report.sell_volume_sol,
            if report.total_volume_sol > 0.0 { (report.sell_volume_sol / report.total_volume_sol) * 100.0 } else { 0.0 }
        ).red().to_string());
        
        // Price Statistics
        self.logger.log(format!("📊 Average Price: ${:.8}", report.average_price).yellow().to_string());
        self.logger.log(format!("📈 Highest Price: ${:.8}", report.max_price).green().to_string());
        self.logger.log(format!("📉 Lowest Price: ${:.8}", report.min_price).red().to_string());
        self.logger.log(format!("💹 Price Range: ${:.8} ({:.2}%)", 
            report.max_price - report.min_price,
            if report.min_price > 0.0 { ((report.max_price - report.min_price) / report.min_price) * 100.0 } else { 0.0 }
        ).magenta().to_string());
        
        // Trader Statistics
        self.logger.log(format!("👥 Unique Traders: {}", report.unique_traders).blue().to_string());
        self.logger.log(format!("📊 Avg Trades per Trader: {:.1}", 
            if report.unique_traders > 0 { report.total_trades as f64 / report.unique_traders as f64 } else { 0.0 }
        ).blue().to_string());
        
        self.logger.log("📊 ═══════════════════════════════════════════════".cyan().bold().to_string());
    }
    
    /// Add a detected token activity for analysis
    pub async fn add_token_activity(&self, activity: TokenActivity) {
        let mut activities = self.token_activities.lock().await;
        activities.push_back(activity.clone());
        
        // Keep only last 100 activities to prevent memory issues
        if activities.len() > 100 {
            activities.pop_front();
        }
        
        // Add price data to price monitor and guardian mode
        if activity.price > 0.0 {
            let mut price_monitor = self.price_monitor.lock().await;
            price_monitor.add_price_point(activity.price, activity.volume_sol);
            
            let mut guardian_mode = self.guardian_mode.lock().await;
            guardian_mode.add_price_point(activity.price, activity.volume_sol);
        }
    }
}

/// Helper to send heartbeat pings to maintain GRPC connection
async fn send_heartbeat_ping(
    subscribe_tx: &Arc<tokio::sync::Mutex<impl Sink<SubscribeRequest, Error = impl std::fmt::Debug> + Unpin>>,
) -> Result<(), String> {
    let ping_request = SubscribeRequest {
        ping: Some(SubscribeRequestPing { id: 0 }),
        ..Default::default()
    };
    
    let mut tx = subscribe_tx.lock().await;
    match tx.send(ping_request).await {
        Ok(_) => Ok(()),
        Err(e) => Err(format!("Failed to send ping: {:?}", e)),
    }
}

/// Start advanced market maker with configuration
pub async fn start_market_maker(config: MarketMakerConfig) -> Result<(), String> {
    let market_maker = MarketMaker::new(config)?;
    market_maker.start().await
} 