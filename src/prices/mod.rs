use anyhow::anyhow;
use cache::Cache;
pub use coingecko::CoinGecko;
#[cfg(test)]
pub use coingecko::CoinMarket;
use market::History;
use oc_bots_sdk::types::{CanisterId, TimestampMillis};
pub use registry::ListedToken;
use serde::{Deserialize, Serialize};
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
const DAY: Duration = Duration::from_secs(24 * 60 * 60);

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
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Source {
    /// A token on the Internet Computer, by its ledger
    Icp(CanisterId),
    /// Any other token, by its CoinGecko id
    CoinGecko(String),
}

impl Source {
    pub fn provider(&self) -> Provider {
        match self {
            Source::Icp(_) => Provider::IcpSwap,
            Source::CoinGecko(_) => Provider::CoinGecko,
        }
    }
}

/// A service the prices come from, and how it's credited where they're shown
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    IcpSwap,
    CoinGecko,
}

impl Provider {
    pub const ALL: [Provider; 2] = [Provider::IcpSwap, Provider::CoinGecko];

    /// How the price API refers to it
    pub fn id(self) -> &'static str {
        match self {
            Provider::IcpSwap => "icpswap",
            Provider::CoinGecko => "coingecko",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Provider::IcpSwap => "ICPSwap",
            Provider::CoinGecko => "CoinGecko",
        }
    }

    /// Where a credit links to
    pub fn url(self) -> &'static str {
        match self {
            Provider::IcpSwap => "https://www.icpswap.com",
            Provider::CoinGecko => "https://www.coingecko.com",
        }
    }

    /// Whether its prices must be credited, with a link, wherever they're shown. CoinGecko's free
    /// API requires it, eg. "Price data by CoinGecko", close to the prices:
    /// https://brand.coingecko.com/resources/attribution-guide
    pub fn attribution_required(self) -> bool {
        matches!(self, Provider::CoinGecko)
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

/// Prices for ChartBot, which can chart any token. Its last week comes from the market, which keeps
/// it up to date, and anything older from its source.
pub struct Prices {
    market: Arc<Market>,
    coingecko: Arc<CoinGecko>,
    icpswap: Arc<IcpSwap>,
    found: Cache<String, Option<Token>>,
    /// The prices fetched from the sources, each with when it was fetched
    fetched: Cache<(Source, Period), (TimestampMillis, Vec<PricePoint>)>,
}

impl Prices {
    pub fn new(market: Arc<Market>, coingecko: Arc<CoinGecko>, icpswap: Arc<IcpSwap>) -> Self {
        Prices {
            market,
            coingecko,
            icpswap,
            found: Cache::default(),
            fetched: Cache::default(),
        }
    }

    /// The token the symbol most likely means: one OpenChat lists, else the coin with the largest
    /// market cap on CoinGecko, which the market has if it's among the 250 largest
    pub async fn find(&self, symbol: &str) -> Result<Option<Token>, PriceError> {
        let key = symbol.to_lowercase();
        if let Some(token) = self.found.get(&key) {
            return Ok(token);
        }

        let listed = self
            .market
            .listed()
            .ok_or_else(|| anyhow!("OpenChat's tokens haven't loaded yet"))?;
        let largest_coin = self
            .market
            .coins(now())
            .and_then(|coins| coins.by_symbol.get(&symbol.to_uppercase()).cloned());
        let token = match (find_listed(&listed.tokens, symbol), largest_coin) {
            (Some(token), _) => Some(token),
            (None, Some(coin)) => Some(Token {
                symbol: coin.symbol.clone(),
                name: coin.name.clone(),
                source: Source::CoinGecko(coin.id.clone()),
            }),
            (None, None) => self.coingecko.find(symbol).await?.map(|coin| Token {
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
        let now = now();
        let pinned = matches!(&token.source, Source::Icp(ledger) if self.market.is_pinned(*ledger));
        // The market has the last week or so of the tokens OpenChat lists and the largest coins,
        // and has stablecoins at $1 for any period
        let up_to_a_week = if period.days() <= 7 || pinned {
            period
        } else {
            Period::Week
        };
        let from_market = match &token.source {
            Source::Icp(ledger) => self.market.history(*ledger, up_to_a_week, now),
            Source::CoinGecko(id) => self.market.coin_history(id, up_to_a_week, now),
        };
        let week = match from_market {
            History::Points(points) if up_to_a_week == period => return Ok(points),
            History::Points(points) => Some(points),
            _ => None,
        };

        // Prices more than a week old don't change, so with the market's week, what was fetched is
        // good for a day
        let max_age = if week.is_some() {
            DAY
        } else {
            cache_duration(period)
        };
        let fetched = self.fetch(&token.source, period, max_age, now).await?;
        let points = match week {
            Some(week) => join(fetched, week),
            None => fetched,
        };
        Ok(within_period(points, period, now))
    }

    // The source's prices over the period, fetched within `max_age`
    async fn fetch(
        &self,
        source: &Source,
        period: Period,
        max_age: Duration,
        now: TimestampMillis,
    ) -> Result<Vec<PricePoint>, PriceError> {
        let key = (source.clone(), period);
        if let Some((fetched_at, points)) = self.fetched.get(&key)
            && now.saturating_sub(fetched_at) < max_age.as_millis() as u64
        {
            return Ok(points);
        }

        let points = match source {
            Source::Icp(ledger) => outliers::filter(self.icpswap.history(*ledger, period).await?),
            Source::CoinGecko(id) => self.coingecko.history(id, period).await?,
        };
        self.fetched.insert(key, (now, points.clone()), DAY);
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

// The fetched prices from before the week, then the week's. The week's are thinned out to the
// fetched prices' spacing, eg. daily for a year, so the chart looks the same throughout, keeping its
// last, the latest price.
fn join(fetched: Vec<PricePoint>, week: Vec<PricePoint>) -> Vec<PricePoint> {
    let Some(start) = week.first().map(|p| p.time) else {
        return fetched;
    };
    let mut joined: Vec<PricePoint> = fetched.into_iter().filter(|p| p.time < start).collect();
    let spacing = median_spacing(&joined);

    let last = week.len() - 1;
    let mut previous: Option<TimestampMillis> = None;
    for (i, point) in week.into_iter().enumerate() {
        if i == last || previous.is_none_or(|time| point.time - time >= spacing) {
            joined.push(point);
            previous = Some(point.time);
        }
    }
    joined
}

fn median_spacing(points: &[PricePoint]) -> TimestampMillis {
    let mut spacings: Vec<TimestampMillis> =
        points.windows(2).map(|w| w[1].time - w[0].time).collect();
    spacings.sort_unstable();
    spacings.get(spacings.len() / 2).copied().unwrap_or(0)
}

// Without the market's week, recent periods are fetched again sooner, as their latest price matters
// more
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
    use coingecko::CoinMarket;
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
        let prices = Prices::new(
            market,
            Arc::new(CoinGecko::new(http.clone(), None)),
            Arc::new(IcpSwap::new(http)),
        );

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

    const HOUR_MS: u64 = 60 * 60 * 1000;
    const DAY_MS: u64 = 24 * HOUR_MS;

    #[test]
    fn the_week_is_joined_to_what_was_fetched_at_its_spacing() {
        // Daily prices up to day 99, and the market's hourly week from day 93 to its latest price
        let fetched: Vec<_> = (80..100).map(|d| point(d * DAY_MS, 1.0)).collect();
        let week: Vec<_> = (0..168)
            .map(|h| point(93 * DAY_MS + h * HOUR_MS, 2.0))
            .chain([point(100 * DAY_MS, 3.0)])
            .collect();

        let joined = join(fetched, week);
        let days: Vec<u64> = joined.iter().map(|p| p.time / DAY_MS).collect();
        assert_eq!(days, (80..=100).collect::<Vec<_>>());
        assert!(joined.iter().all(|p| p.time % DAY_MS == 0));
        assert_eq!(joined[12].price, 1.0, "day 92 is fetched");
        assert_eq!(joined[13].price, 2.0, "day 93 is the market's");
        assert_eq!(joined.last().unwrap().price, 3.0);
    }

    #[test]
    fn an_hourly_week_is_kept_whole_after_hourly_prices() {
        let fetched: Vec<_> = (0..24 * 30).map(|h| point(h * HOUR_MS, 1.0)).collect();
        let week: Vec<_> = (0..169)
            .map(|h| point(23 * DAY_MS + h * HOUR_MS, 2.0))
            .collect();

        let joined = join(fetched, week.clone());
        assert_eq!(joined.len(), 23 * 24 + 169);
        assert_eq!(&joined[23 * 24..], week);
    }

    // Prices whose price APIs can't be reached, so whatever they'd fetch fails
    fn offline_prices(market: Arc<Market>) -> Prices {
        let http = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:9").unwrap())
            .build()
            .unwrap();
        Prices::new(
            market,
            Arc::new(CoinGecko::new(http.clone(), None)),
            Arc::new(IcpSwap::new(http)),
        )
    }

    // The market has ADA's last week, at 0.2, and its latest price, 0.3
    fn market_with_ada(now: TimestampMillis) -> Arc<Market> {
        let market = Arc::new(Market::new(std::collections::HashMap::new()));
        market.set_listed(Vec::new());
        market.set_coins(
            vec![CoinMarket {
                id: "cardano".to_string(),
                symbol: "ada".to_string(),
                name: "Cardano".to_string(),
                usd: 0.3,
                updated: now,
                sparkline: vec![0.2; 168],
            }],
            now,
        );
        market
    }

    fn coin(id: &str) -> Token {
        Token {
            symbol: id.to_uppercase(),
            name: id.to_string(),
            source: Source::CoinGecko(id.to_string()),
        }
    }

    // Daily prices at 0.5, for a little over a year up to `until`
    fn fetched_year(until: TimestampMillis) -> Vec<PricePoint> {
        (0..=366)
            .map(|d| point(until - (366 - d) * DAY_MS, 0.5))
            .collect()
    }

    #[tokio::test]
    async fn a_year_fetched_hours_ago_is_joined_to_the_markets_week_without_fetching_again() {
        let now = now();
        let prices = offline_prices(market_with_ada(now));
        let fetched_at = now - 4 * HOUR_MS;
        prices.fetched.insert(
            (coin("cardano").source, Period::Year),
            (fetched_at, fetched_year(fetched_at)),
            DAY,
        );

        let points = prices
            .history(&coin("cardano"), Period::Year)
            .await
            .unwrap();

        // From the year's start, a fetched price a day until the market's week, then the week a
        // day at a time, then the latest price
        assert!(points[0].time >= now - Period::Year.millis());
        assert!(points.windows(2).all(|w| w[0].time < w[1].time));
        let at = |price: f64| points.iter().filter(|p| p.price == price).count();
        assert_eq!(at(0.5), 359);
        assert_eq!(at(0.2), 7);
        assert_eq!(*points.last().unwrap(), point(now, 0.3));
    }

    #[tokio::test]
    async fn without_the_markets_week_a_year_is_fetched_again_after_an_hour() {
        let now = now();
        let prices = offline_prices(market_with_ada(now));
        // CoinGecko's Solchat, which isn't among the largest coins
        let key = (coin("solchat").source, Period::Year);

        let fetched_at = now - 30 * 60 * 1000;
        prices
            .fetched
            .insert(key.clone(), (fetched_at, fetched_year(fetched_at)), DAY);
        assert!(prices.history(&coin("solchat"), Period::Year).await.is_ok());

        // It's fetched again, which fails offline
        let fetched_at = now - 2 * HOUR_MS;
        prices
            .fetched
            .insert(key, (fetched_at, fetched_year(fetched_at)), DAY);
        assert!(
            prices
                .history(&coin("solchat"), Period::Year)
                .await
                .is_err()
        );
    }
}
