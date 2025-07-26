# Raydium CPMM Market Bot

A sophisticated Solana trading bot that implements advanced market making strategies with multi-wallet support, stealth trading, and comprehensive monitoring capabilities.

## Project Structure

The codebase is organized into several modules:

### Core Modules

- **`src/common/`** - Shared utilities, configuration, and advanced trading features
  - `config.rs` - Configuration management and environment variables
  - `cache.rs` - Global caching system for token accounts and balances
  - `constants.rs` - Application constants and program IDs
  - `dynamic_ratios.rs` - Dynamic buy/sell ratio management
  - `guardian_mode.rs` - Risk management and loss prevention
  - `logger.rs` - Logging utilities
  - `price_monitor.rs` - Real-time price monitoring and analysis
  - `volume_waves.rs` - Volume-based trading strategies
  - `wallet_pool.rs` - Multi-wallet management and rotation

- **`src/core/`** - Core system functionality
  - `token.rs` - Token account management and operations
  - `tx.rs` - Transaction handling and processing

- **`src/dex/`** - Protocol-specific implementations
  - `raydium_cpmm.rs` - Raydium CPMM (Constant Product Market Maker) integration

- **`src/engine/`** - Advanced trading engine and strategies
  - `market_maker.rs` - Main market making engine with stealth trading
  - `random_trader.rs` - Randomized trading strategies
  - `transaction_parser.rs` - Transaction parsing and analysis
  - `swap.rs` - Swap execution and protocol selection
  - `monitor.rs` - Market monitoring utilities

- **`src/error/`** - Error handling and definitions
  - `mod.rs` - Custom error types and handling

- **`src/services/`** - External services integration
  - `telegram.rs` - Telegram notification service
  - `rpc_client.rs` - RPC client management
  - `blockhash_processor.rs` - Blockhash processing for transactions
  - `cache_maintenance.rs` - Cache maintenance utilities
  - `nozomi.rs` - Nozomi RPC integration

## Advanced Features

### Multi-Wallet Support
- **Wallet Pool Management**: Automatic wallet rotation and load balancing
- **Stealth Trading**: Randomized delays and amounts to avoid detection
- **Concurrent Trading**: Multiple wallets can trade simultaneously
- **Balance Distribution**: Automatic SOL distribution across wallets

### Advanced Trading Strategies
- **Dynamic Ratios**: Adaptive buy/sell ratios based on market conditions
- **Volume Wave Analysis**: Time-based volume analysis for optimal entry/exit
- **Guardian Mode**: Risk management with automatic stop-loss mechanisms
- **Price Monitoring**: Real-time price tracking with alerts

### Market Making Capabilities
- **Constant Product Market Making**: Raydium CPMM integration
- **Progressive Selling**: Gradual position reduction strategies
- **Slippage Protection**: Configurable slippage tolerance
- **Token Activity Tracking**: Comprehensive trade activity monitoring

## Setup

### Environment Variables

#### Required Variables
- `YELLOWSTONE_GRPC_HTTP` - Your Yellowstone gRPC endpoint URL
- `YELLOWSTONE_GRPC_TOKEN` - Your Yellowstone authentication token
- `WALLET_PRIVATE_KEY` - Your wallet private key (base58 encoded)

#### Trading Configuration
- `TARGET_TOKEN_MINT` - Token mint address to trade (default: CGrptxv4hSiNSCTufJzBMzarfrfjNhD9vMmhYQ8eVPsA)
- `SLIPPAGE` - Slippage tolerance in basis points (default: 10000 = 10%)
- `MIN_BUY_AMOUNT` - Minimum buy amount in SOL (default: 0.2)
- `MAX_BUY_AMOUNT` - Maximum buy amount in SOL (default: 0.005)
- `MIN_SOL` - Minimum SOL balance to maintain (default: 0.005)

#### Advanced Features
- `MIN_SELL_DELAY_HOURS` - Minimum delay before selling (default: 24)
- `MAX_SELL_DELAY_HOURS` - Maximum delay before selling (default: 72)
- `PRICE_CHANGE_THRESHOLD` - Price change threshold for alerts (default: 0.15)
- `MIN_BUY_RATIO` - Minimum buy ratio (default: 0.67)
- `MAX_BUY_RATIO` - Maximum buy ratio (default: 0.73)
- `VOLUME_WAVE_ACTIVE_HOURS` - Active trading hours (default: 2)
- `VOLUME_WAVE_SLOW_HOURS` - Slow trading hours (default: 6)
- `GUARDIAN_MODE_ENABLED` - Enable guardian mode (default: true)
- `GUARDIAN_DROP_THRESHOLD` - Guardian mode drop threshold (default: 0.10)

#### Multi-Wallet Configuration
- `WALLET_COUNT` - Number of wallets to generate (default: 5)
- `ENABLE_MULTI_WALLET` - Enable multi-wallet trading (default: true)
- `MAX_CONCURRENT_TRADES` - Maximum concurrent trades (default: 3)

#### Telegram Notifications
- `TELEGRAM_BOT_TOKEN` - Your Telegram bot token
- `TELEGRAM_CHAT_ID` - Your chat ID for receiving notifications

## Usage

### Basic Setup
```bash
# Build the project
cargo build --release

# Run the market maker bot
cargo run --release
```

### Advanced Commands

#### Wallet Management
```bash
# Generate multiple wallets
cargo run --release -- --generate-wallets

# Distribute SOL across wallets
cargo run --release -- --distribute-sol

# Collect SOL from all wallets
cargo run --release -- --collect-sol
```

#### Token Operations
```bash
# Wrap SOL to WSOL
cargo run --release -- --wrap <amount>

# Unwrap WSOL to SOL
cargo run --release -- --unwrap

# Close all token accounts
cargo run --release -- --close
```

#### Trading Modes
```bash
# Stealth mode (recommended)
cargo run --release -- --stealth-mode

# Conservative mode
cargo run --release -- --conservative-mode

# Aggressive mode
cargo run --release -- --aggressive-mode
```

## Trading Strategies

### Stealth Mode
- Randomized trading delays (1-5 seconds)
- Variable buy amounts (67-73% of target)
- Multi-wallet rotation
- Minimal transaction footprint

### Conservative Mode
- Longer delays between trades
- Smaller position sizes
- Enhanced risk management
- Focus on capital preservation

### Aggressive Mode
- Faster execution
- Larger position sizes
- Higher risk tolerance
- Maximum profit potential

## Monitoring and Analytics

The bot provides comprehensive monitoring capabilities:

- **Real-time Activity Tracking**: Monitor all trading activities
- **Price Analysis**: Track price movements and trends
- **Volume Analysis**: Analyze trading volume patterns
- **Performance Metrics**: Track profit/loss and success rates
- **Telegram Notifications**: Real-time alerts and updates

## Recent Updates

- **Advanced Multi-Wallet Support**: Enhanced wallet pool management
- **Stealth Trading Algorithms**: Improved detection avoidance
- **Dynamic Ratio Management**: Adaptive trading strategies
- **Guardian Mode**: Enhanced risk management
- **Volume Wave Analysis**: Time-based trading optimization
- **Comprehensive Monitoring**: Real-time analytics and reporting

## Security Features

- **Private Key Management**: Secure wallet handling
- **Transaction Validation**: Comprehensive transaction verification
- **Error Recovery**: Robust error handling and recovery
- **Rate Limiting**: Protection against API rate limits
- **Balance Monitoring**: Continuous balance verification

## Contact

if you're really interested in the script, feel free to contact me. 
**twitter**: https://x.com/0xSeishiro69
**telegram**:@Sync404error

## Disclaimer

This software is for educational and research purposes. Trading cryptocurrencies involves significant risk. Use at your own discretion and never invest more than you can afford to lose.
