use super::coingecko::CoinGecko;
use super::icpswap::IcpSwap;
use super::market::Market;
use super::{now, registry};
use ic_agent::Agent;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

const LISTED_INTERVAL: Duration = Duration::from_secs(60 * 60);
const LATEST_INTERVAL: Duration = Duration::from_secs(60);
// About 7,200 calls a month, inside a CoinGecko Demo key's 10,000, with room for ChartBot's
const COINS_INTERVAL: Duration = Duration::from_secs(6 * 60);
const RETRY_DELAY: Duration = Duration::from_secs(60);
const IDLE_DELAY: Duration = Duration::from_secs(10);
// A week has 168 hours, and these reach back before it, for the price at its start, through hours
// without trades, which ICPSwap leaves out
const HOURLY_LIMIT: u32 = 200;
// ICPSwap's charts are slow, so they're fetched two at a time, which gets through OpenChat's ~140
// tokens in about 10 minutes
const HOURLY_WORKERS: usize = 2;

/// Keeps the market up to date, in the background, for as long as the process runs: OpenChat's
/// tokens hourly, their latest prices every minute, each token's hourly closes every 15 minutes, and
/// the largest coins every 6 minutes
pub fn spawn(
    market: Arc<Market>,
    icpswap: Arc<IcpSwap>,
    coingecko: Arc<CoinGecko>,
    mainnet: Agent,
) {
    tokio::spawn(refresh_listed(market.clone(), mainnet));
    tokio::spawn(refresh_latest(market.clone(), icpswap.clone()));
    tokio::spawn(refresh_coins(market.clone(), coingecko));
    for _ in 0..HOURLY_WORKERS {
        tokio::spawn(refresh_hourly(market.clone(), icpswap.clone()));
    }
}

async fn refresh_listed(market: Arc<Market>, mainnet: Agent) {
    loop {
        let delay = match registry::listed_tokens(&mainnet).await {
            Ok(tokens) if !tokens.is_empty() => {
                info!(tokens = tokens.len(), "Refreshed OpenChat's tokens");
                market.set_listed(tokens);
                LISTED_INTERVAL
            }
            Ok(_) => {
                warn!("OpenChat's Registry listed no tokens");
                RETRY_DELAY
            }
            Err(error) => {
                warn!(%error, "Failed to get OpenChat's tokens");
                RETRY_DELAY
            }
        };
        tokio::time::sleep(delay).await;
    }
}

async fn refresh_latest(market: Arc<Market>, icpswap: Arc<IcpSwap>) {
    loop {
        // Only the listed tokens' prices are kept
        if market.listed().is_none() {
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }
        match icpswap.token_prices().await {
            Ok(prices) => market.set_latest(prices, now()),
            Err(error) => warn!(?error, "Failed to get the latest prices"),
        }
        tokio::time::sleep(LATEST_INTERVAL).await;
    }
}

async fn refresh_coins(market: Arc<Market>, coingecko: Arc<CoinGecko>) {
    loop {
        // Coins whose symbols OpenChat lists are left out
        if market.listed().is_none() {
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }
        let delay = match coingecko.top_coins().await {
            Ok(coins) => {
                market.set_coins(coins, now());
                COINS_INTERVAL
            }
            Err(error) => {
                warn!(?error, "Failed to get the largest coins");
                RETRY_DELAY
            }
        };
        tokio::time::sleep(delay).await;
    }
}

async fn refresh_hourly(market: Arc<Market>, icpswap: Arc<IcpSwap>) {
    loop {
        let Some(ledger) = market.next_due(now()) else {
            tokio::time::sleep(IDLE_DELAY).await;
            continue;
        };
        let closes = match icpswap.hourly(ledger, HOURLY_LIMIT).await {
            Ok(closes) => Some(closes),
            Err(error) => {
                warn!(%ledger, ?error, "Failed to get a token's hourly closes");
                None
            }
        };
        market.set_hourly(ledger, closes, now());
    }
}
