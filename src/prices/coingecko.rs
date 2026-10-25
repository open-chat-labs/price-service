use super::Period;
use super::{PriceError, PricePoint};
use crate::prices::cache::RateLimiter;
use anyhow::anyhow;
use chrono::DateTime;
use oc_bots_sdk::types::TimestampMillis;
use reqwest::StatusCode;
use serde::Deserialize;

const BASE_URL: &str = "https://api.coingecko.com/api/v3";

/// CoinGecko's free API, for tokens which aren't on the Internet Computer
pub struct CoinGecko {
    http: reqwest::Client,
    api_key: Option<String>,
    rate_limiter: RateLimiter,
}

/// A coin, as CoinGecko's markets list it
#[derive(Clone, Deserialize, Debug, PartialEq)]
pub struct Coin {
    pub id: String,
    pub symbol: String,
    pub name: String,
}

/// One of the largest coins, with its latest price and its prices over the last week
#[derive(Clone, Debug, PartialEq)]
pub struct CoinMarket {
    pub id: String,
    /// As CoinGecko writes it, in lower case, eg. "ada"
    pub symbol: String,
    pub name: String,
    pub usd: f64,
    /// When CoinGecko last updated its price
    pub updated: TimestampMillis,
    /// Hourly, oldest first, the last being an hour before `updated`, and NaN where CoinGecko has
    /// none
    pub sparkline: Vec<f64>,
}

#[derive(Deserialize)]
struct Market {
    id: String,
    symbol: String,
    name: String,
    current_price: Option<f64>,
    last_updated: Option<String>,
    sparkline_in_7d: Option<Sparkline>,
}

#[derive(Deserialize)]
struct Sparkline {
    price: Vec<Option<f64>>,
}

#[derive(Deserialize)]
struct MarketChart {
    /// [time in ms, price in USD]
    prices: Vec<(f64, f64)>,
}

impl CoinGecko {
    pub fn new(http: reqwest::Client, api_key: Option<String>) -> Self {
        // Within its limits: a Demo key's is at least 30 calls a minute, and without one it's 10 to
        // 30, shared with whoever else calls from the same address
        let limit = if api_key.is_some() { 30 } else { 10 };
        CoinGecko {
            http,
            api_key,
            rate_limiter: RateLimiter::per_minute(limit),
        }
    }

    /// The coin with the largest market cap which has the symbol. The market keeps the largest
    /// coins, so this is only needed for the others.
    pub async fn find(&self, symbol: &str) -> Result<Option<Coin>, PriceError> {
        let coins: Vec<Coin> = self
            .get(
                "/coins/markets",
                &[
                    ("vs_currency", "usd"),
                    ("symbols", &symbol.to_lowercase()),
                    ("include_tokens", "top"),
                ],
            )
            .await?;
        Ok(find(&coins, symbol))
    }

    /// The 250 coins with the largest market caps, largest first, with their prices over the last
    /// week, in a single call
    pub async fn top_coins(&self) -> Result<Vec<CoinMarket>, PriceError> {
        let markets: Vec<Market> = self
            .get(
                "/coins/markets",
                &[
                    ("vs_currency", "usd"),
                    ("order", "market_cap_desc"),
                    ("per_page", "250"),
                    ("sparkline", "true"),
                ],
            )
            .await?;
        Ok(coin_markets(markets))
    }

    pub async fn history(&self, id: &str, period: Period) -> Result<Vec<PricePoint>, PriceError> {
        let chart: MarketChart = self
            .get(
                &format!("/coins/{id}/market_chart"),
                &[("vs_currency", "usd"), ("days", &period.days().to_string())],
            )
            .await?;
        Ok(points(chart))
    }

    async fn get<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, PriceError> {
        if !self.rate_limiter.try_call() {
            return Err(PriceError::RateLimited("CoinGecko"));
        }
        let mut request = self.http.get(format!("{BASE_URL}{path}")).query(query);
        if let Some(key) = &self.api_key {
            request = request.header("x-cg-demo-api-key", key);
        }
        let response = request.send().await.map_err(anyhow::Error::from)?;
        match response.status() {
            StatusCode::TOO_MANY_REQUESTS => Err(PriceError::RateLimited("CoinGecko")),
            status if !status.is_success() => {
                Err(anyhow!("CoinGecko returned {status} for {path}").into())
            }
            _ => Ok(response.json().await.map_err(anyhow::Error::from)?),
        }
    }
}

// The first coin with the symbol, ie. the largest if they're in order of market cap
fn find(coins: &[Coin], symbol: &str) -> Option<Coin> {
    coins
        .iter()
        .find(|c| c.symbol.eq_ignore_ascii_case(symbol))
        .cloned()
}

// Those with a price, and when it was updated
fn coin_markets(markets: Vec<Market>) -> Vec<CoinMarket> {
    markets
        .into_iter()
        .filter_map(|market| {
            let usd = market.current_price.filter(|p| p.is_finite() && *p > 0.0)?;
            let updated = DateTime::parse_from_rfc3339(&market.last_updated?).ok()?;
            Some(CoinMarket {
                id: market.id,
                symbol: market.symbol,
                name: market.name,
                usd,
                updated: updated.timestamp_millis() as TimestampMillis,
                sparkline: market
                    .sparkline_in_7d
                    .map(|s| s.price)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|p| p.filter(|p| p.is_finite() && *p > 0.0).unwrap_or(f64::NAN))
                    .collect(),
            })
        })
        .collect()
}

// CoinGecko gives a point every 5 minutes over a day, every hour up to 90 days, and every day beyond,
// and its last point is the latest price
fn points(chart: MarketChart) -> Vec<PricePoint> {
    let mut points: Vec<PricePoint> = chart
        .prices
        .into_iter()
        .filter(|(time, price)| *time > 0.0 && price.is_finite() && *price > 0.0)
        .map(|(time, price)| PricePoint {
            time: time as u64,
            price,
        })
        .collect();
    points.sort_by_key(|p| p.time);
    points
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "calls CoinGecko"]
    async fn finds_coins_and_their_prices() {
        let coingecko = CoinGecko::new(crate::prices::http_client().unwrap(), None);
        let coin = coingecko.find("ETH").await.unwrap().unwrap();
        assert_eq!(coin.id, "ethereum");

        let points = coingecko.history(&coin.id, Period::Day).await.unwrap();
        assert!(points.len() > 200, "{} points", points.len());
        assert!(coingecko.find("NOTATOKENXYZ").await.unwrap().is_none());
    }

    #[test]
    fn market_charts_map_to_points_in_time_order() {
        let chart: MarketChart = serde_json::from_str(
            r#"{
                "prices": [[1760000000000, 5.12], [1759996400000, 5.01], [1760007200000, 5.3], [1760010800000, 0]],
                "market_caps": [[1760000000000, 1]],
                "total_volumes": [[1760000000000, 1]]
            }"#,
        )
        .unwrap();

        assert_eq!(
            points(chart),
            vec![
                PricePoint {
                    time: 1759996400000,
                    price: 5.01
                },
                PricePoint {
                    time: 1760000000000,
                    price: 5.12
                },
                PricePoint {
                    time: 1760007200000,
                    price: 5.3
                },
            ]
        );
    }

    #[test]
    fn the_largest_coin_with_the_symbol_is_found() {
        let coin = |id: &str, symbol: &str| Coin {
            id: id.to_string(),
            symbol: symbol.to_string(),
            name: id.to_string(),
        };
        let coins = vec![
            coin("bitcoin", "btc"),
            coin("tether", "usdt"),
            coin("bitcoin-bep2", "btc"),
        ];
        assert_eq!(find(&coins, "BTC"), Some(coin("bitcoin", "btc")));
        assert_eq!(find(&coins, "eth"), None);
    }

    #[test]
    fn top_coins_have_their_price_update_time_and_sparkline() {
        // Trimmed from /coins/markets?sparkline=true
        let markets: Vec<Market> = serde_json::from_str(
            r#"[
                {"id": "cardano", "symbol": "ada", "name": "Cardano", "current_price": 0.204964, "market_cap_rank": 10, "last_updated": "2026-10-25T21:08:58.000Z", "sparkline_in_7d": {"price": [0.21306, null, 0.20584]}},
                {"id": "dead", "symbol": "dead", "name": "Dead", "current_price": null, "last_updated": "2026-10-25T21:08:58.000Z", "sparkline_in_7d": {"price": []}},
                {"id": "new", "symbol": "new", "name": "New", "current_price": 1.5, "last_updated": "2026-10-25T21:08:58.000Z"}
            ]"#,
        )
        .unwrap();
        let coins = coin_markets(markets);

        assert_eq!(coins.len(), 2);
        assert_eq!(coins[0].id, "cardano");
        assert_eq!(coins[0].symbol, "ada");
        assert_eq!(coins[0].usd, 0.204964);
        assert_eq!(coins[0].updated, 1792962538000);
        assert_eq!(coins[0].sparkline[0], 0.21306);
        assert!(coins[0].sparkline[1].is_nan());
        assert_eq!(coins[0].sparkline[2], 0.20584);
        assert!(coins[1].sparkline.is_empty());
    }

    #[tokio::test]
    #[ignore = "calls CoinGecko"]
    async fn gets_the_top_coins() {
        let coingecko = CoinGecko::new(crate::prices::http_client().unwrap(), None);
        let coins = coingecko.top_coins().await.unwrap();
        assert!(coins.len() > 200, "{} coins", coins.len());
        let ada = coins.iter().find(|c| c.id == "cardano").unwrap();
        assert!(ada.sparkline.len() > 100);
    }

    #[test]
    fn markets_deserialize_to_coins() {
        let coins: Vec<Coin> = serde_json::from_str(
            r#"[{"id": "bitcoin", "symbol": "btc", "name": "Bitcoin", "image": "x", "current_price": 67000.1, "market_cap": 1}]"#,
        )
        .unwrap();
        assert_eq!(
            coins,
            vec![Coin {
                id: "bitcoin".to_string(),
                symbol: "btc".to_string(),
                name: "Bitcoin".to_string()
            }]
        );
    }
}
