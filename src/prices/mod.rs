use anyhow::anyhow;
use cache::Cache;
use coingecko::CoinGecko;
use market::History;
use oc_bots_sdk::types::{CanisterId, TimestampMillis};
pub use registry::ListedToken;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod cache;
mod coingecko;
mod icpswap;
mod market;
mod outliers;
mod period;
mod refresh;
mod registry;

pub use icpswap::IcpSwap;
#[cfg(test)]
pub use icpswap::TokenPrice;
pub use market::{History as MarketHistory, Market};
pub use period::Period;
pub use refresh::spawn as spawn_refresh;

const HOUR: Duration = Duration::from_secs(60 * 60);

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PricePoint {
    pub time: TimestampMillis,
    /// In USD
    pub price: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    /// As its source writes it, eg. ckBTC
    pub symbol: String,
    pub name: String,
    pub source: Source,
}

/// Where a token's prices come from
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    /// A token on the Internet Computer, by its ledger
    Icp(CanisterId),
    /// Any other token, by its CoinGecko id
    CoinGecko(String),
}

impl Source {
    pub fn name(&self) -> &'static str {
        match self {
            Source::Icp(_) => "ICPSwap",
            Source::CoinGecko(_) => "CoinGecko",
        }
    }
}

#[derive(Debug)]
pub enum PriceError {
    /// The source is getting too many requests, from the bot or from others
    RateLimited(&'static str),
    Other(anyhow::Error),
}

impl From<anyhow::Error> for PriceError {
    fn from(error: anyhow::Error) -> Self {
        PriceError::Other(error)
    }
}

/// Prices for ChartBot, which can chart any token. Those OpenChat lists come from the market, as
/// far back as it goes, and otherwise from their source.
pub struct Prices {
    market: Arc<Market>,
    coingecko: CoinGecko,
    icpswap: Arc<IcpSwap>,
    found: Cache<String, Option<Token>>,
    histories: Cache<(Source, Period), Vec<PricePoint>>,
}

impl Prices {
    pub fn new(
        http: reqwest::Client,
        coingecko_api_key: Option<String>,
        market: Arc<Market>,
        icpswap: Arc<IcpSwap>,
    ) -> Self {
        Prices {
            market,
            coingecko: CoinGecko::new(http, coingecko_api_key),
            icpswap,
            found: Cache::default(),
            histories: Cache::default(),
        }
    }

    /// The token the symbol most likely means: one OpenChat lists, else the one with the largest
    /// market cap on CoinGecko
    pub async fn find(&self, symbol: &str) -> Result<Option<Token>, PriceError> {
        let key = symbol.to_lowercase();
        if let Some(token) = self.found.get(&key) {
            return Ok(token);
        }

        let listed = self
            .market
            .listed()
            .ok_or_else(|| anyhow!("OpenChat's tokens haven't loaded yet"))?;
        let token = match find_listed(&listed.tokens, symbol) {
            Some(token) => Some(token),
            None => self.coingecko.find(symbol).await?.map(|coin| Token {
                symbol: coin.symbol.to_uppercase(),
                name: coin.name,
                source: Source::CoinGecko(coin.id),
            }),
        };

        self.found.insert(key, token.clone(), HOUR);
        Ok(token)
    }

    /// The token's prices over the period, oldest first, the last being the latest
    pub async fn history(
        &self,
        token: &Token,
        period: Period,
    ) -> Result<Vec<PricePoint>, PriceError> {
        if let Source::Icp(ledger) = token.source
            && (period.days() <= 7 || self.market.is_pinned(ledger))
            && let History::Points(points) = self.market.history(ledger, period, now())
        {
            return Ok(points);
        }

        let key = (token.source.clone(), period);
        if let Some(points) = self.histories.get(&key) {
            return Ok(points);
        }

        let points = match &token.source {
            Source::Icp(ledger) => outliers::filter(self.icpswap.history(*ledger, period).await?),
            Source::CoinGecko(id) => self.coingecko.history(id, period).await?,
        };
        let points = within_period(points, period, now());

        self.histories
            .insert(key, points.clone(), cache_duration(period));
        Ok(points)
    }
}

pub fn http_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        // CoinGecko refuses requests without one
        .user_agent("OpenChat price-service")
        .build()
}

// Enabled tokens first, as several share a symbol
fn find_listed(tokens: &[ListedToken], symbol: &str) -> Option<Token> {
    let matches = |t: &&ListedToken| t.symbol.eq_ignore_ascii_case(symbol);
    tokens
        .iter()
        .filter(|t| t.enabled)
        .find(matches)
        .or_else(|| tokens.iter().find(matches))
        .map(|t| Token {
            symbol: t.symbol.clone(),
            name: t.name.clone(),
            source: Source::Icp(t.ledger),
        })
}

// Recent periods are cached more briefly, as their latest price matters more
fn cache_duration(period: Period) -> Duration {
    match period {
        Period::Day => Duration::from_secs(5 * 60),
        Period::Week => Duration::from_secs(15 * 60),
        _ => HOUR,
    }
}

// The points within the period. The price at its start is the last one before it, if there is one,
// as a token which trades rarely may not have traded for a while.
fn within_period(points: Vec<PricePoint>, period: Period, now: TimestampMillis) -> Vec<PricePoint> {
    let start = now.saturating_sub(period.millis());
    let before = points
        .iter()
        .rev()
        .find(|p| p.time < start)
        .map(|p| PricePoint {
            time: start,
            price: p.price,
        });
    before
        .into_iter()
        .chain(points.into_iter().filter(|p| p.time >= start))
        .collect()
}

pub fn now() -> TimestampMillis {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as TimestampMillis
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_agent::export::Principal;

    #[test]
    fn listed_tokens_are_found_ignoring_case() {
        let chat = Principal::from_text("2ouva-viaaa-aaaaq-aaamq-cai")
            .unwrap()
            .into();
        let tokens = vec![ListedToken {
            ledger: chat,
            symbol: "ckBTC".to_string(),
            name: "ckBTC".to_string(),
            enabled: true,
        }];

        let token = find_listed(&tokens, "CKBTC").unwrap();
        assert_eq!(token.symbol, "ckBTC");
        assert_eq!(token.source, Source::Icp(chat));
        assert!(find_listed(&tokens, "BTC").is_none());
    }

    #[test]
    fn enabled_tokens_are_found_before_disabled_ones_with_the_same_symbol() {
        let ledger = |text: &str| -> CanisterId { Principal::from_text(text).unwrap().into() };
        let sneed = |ledger: CanisterId, enabled: bool| ListedToken {
            ledger,
            symbol: "SNEED".to_string(),
            name: "Sneed".to_string(),
            enabled,
        };
        let old = ledger("2ouva-viaaa-aaaaq-aaamq-cai");
        let new = ledger("ryjl3-tyaaa-aaaaa-aaaba-cai");
        let tokens = vec![sneed(old, false), sneed(new, true)];
        assert_eq!(
            find_listed(&tokens, "sneed").unwrap().source,
            Source::Icp(new)
        );
    }

    #[tokio::test]
    async fn listed_tokens_are_found_and_charted_for_up_to_a_week_from_the_market() {
        let chat: CanisterId = Principal::from_text("2ouva-viaaa-aaaaq-aaamq-cai")
            .unwrap()
            .into();
        let market = Arc::new(Market::new(std::collections::HashMap::new()));
        market.set_listed(vec![ListedToken {
            ledger: chat,
            symbol: "CHAT".to_string(),
            name: "OpenChat".to_string(),
            enabled: true,
        }]);
        market.set_hourly(chat, Some(vec![point(now() - 60 * 60 * 1000, 0.03)]), now());
        let http = http_client().unwrap();
        let prices = Prices::new(http.clone(), None, market, Arc::new(IcpSwap::new(http)));

        // Neither calls a price API
        let token = prices.find("chat").await.unwrap().unwrap();
        assert_eq!(token.source, Source::Icp(chat));
        let points = prices.history(&token, Period::Week).await.unwrap();
        assert_eq!(points.iter().map(|p| p.price).collect::<Vec<_>>(), [0.03]);
    }

    fn point(time: TimestampMillis, price: f64) -> PricePoint {
        PricePoint { time, price }
    }

    #[test]
    fn the_period_starts_at_the_last_price_before_it() {
        let day = Period::Day.millis();
        let now = 10 * day;
        let points = vec![
            point(now - day - 2, 1.0),
            point(now - day - 1, 2.0),
            point(now - 10, 3.0),
            point(now, 4.0),
        ];
        assert_eq!(
            within_period(points, Period::Day, now),
            vec![point(now - day, 2.0), point(now - 10, 3.0), point(now, 4.0)]
        );
    }

    #[test]
    fn a_point_at_the_start_of_the_period_is_kept() {
        let day = Period::Day.millis();
        let points = vec![point(9 * day, 2.0), point(10 * day, 3.0)];
        assert_eq!(within_period(points.clone(), Period::Day, 10 * day), points);
    }
}
