use super::icpswap::IcpSwap;
use super::market::Market;
use super::{now, registry};
use ic_agent::Agent;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

const LISTED_INTERVAL: Duration = Duration::from_secs(60 * 60);
const LATEST_INTERVAL: Duration = Duration::from_secs(60);
const RETRY_DELAY: Duration = Duration::from_secs(60);
const IDLE_DELAY: Duration = Duration::from_secs(10);
// A week has 168 hours, and these reach back before it, for the price at its start, through hours
// without trades, which ICPSwap leaves out
const HOURLY_LIMIT: u32 = 200;
// ICPSwap's charts are slow, so they're fetched two at a time, which gets through OpenChat's ~140
// tokens in about 10 minutes
const HOURLY_WORKERS: usize = 2;

/// Keeps the market up to date, in the background, for as long as the process runs: OpenChat's
/// tokens hourly, their latest prices every minute, and each token's hourly closes every 15 minutes
pub fn spawn(market: Arc<Market>, icpswap: Arc<IcpSwap>, mainnet: Agent) {
    tokio::spawn(refresh_listed(market.clone(), mainnet));
    tokio::spawn(refresh_latest(market.clone(), icpswap.clone()));
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
