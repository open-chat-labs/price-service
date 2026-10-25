use super::Period;
use super::{PriceError, PricePoint};
use anyhow::anyhow;
use futures::future::try_join_all;
use ic_agent::export::Principal;
use oc_bots_sdk::types::CanisterId;
use reqwest::StatusCode;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::time::Duration;
use tokio::sync::Semaphore;

const BASE_URL: &str = "https://api.icpswap.com/info";
// Its charts can take half a minute
const TIMEOUT: Duration = Duration::from_secs(30);
// It refuses more than about 15 requests at once
const MAX_CONCURRENT_REQUESTS: usize = 4;
const RETRY_DELAY: Duration = Duration::from_secs(2);
// It returns at most this many candles per page
const MAX_PAGE_SIZE: u32 = 365;

/// ICPSwap's API, for tokens on the Internet Computer
pub struct IcpSwap {
    http: reqwest::Client,
    permits: Semaphore,
}

// Every response is wrapped in this, with an HTTP status of 200 even for an error
#[derive(Deserialize)]
struct Envelope<T> {
    code: u32,
    message: Option<String>,
    data: Option<T>,
}

#[derive(Deserialize)]
struct Page {
    content: Vec<Candle>,
}

/// The token's trades in an hour or a day, its prices as decimal strings. It's left out if there
/// were none.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Candle {
    /// The price at the last trade, in USD. `price` and `low` and `high` can be far off it, from
    /// single trades.
    close: Option<String>,
    /// When the last trade was
    close_time: Option<u64>,
    begin_time: u64,
}

impl IcpSwap {
    pub fn new(http: reqwest::Client) -> Self {
        IcpSwap {
            http,
            permits: Semaphore::new(MAX_CONCURRENT_REQUESTS),
        }
    }

    /// The token's hourly or daily prices, newest first, going back at least the period if there's
    /// enough history, so that the price at the start of the period is known
    pub async fn history(
        &self,
        ledger: CanisterId,
        period: Period,
    ) -> Result<Vec<PricePoint>, PriceError> {
        // Hours without trades are left out, so these allow for gaps, and reach back before the
        // period's start
        let (level, limit, pages) = match period {
            Period::Day => ("h1", 48, 1),
            Period::Week => ("h1", MAX_PAGE_SIZE, 1),
            Period::Month => ("h1", MAX_PAGE_SIZE, 2),
            Period::Quarter => ("d1", 100, 1),
            Period::Year => ("d1", MAX_PAGE_SIZE, 2),
        };
        let pages =
            try_join_all((1..=pages).map(|page| self.chart(ledger, level, page, limit))).await?;
        Ok(points(pages.into_iter().flatten()))
    }

    /// A token's hourly closes, oldest first, in the latest `limit` hours it was traded in
    pub async fn hourly(
        &self,
        ledger: CanisterId,
        limit: u32,
    ) -> Result<Vec<PricePoint>, PriceError> {
        let candles = self.chart(ledger, "h1", 1, limit).await?;
        Ok(points(candles.into_iter()))
    }

    /// Every token's latest price, leaving out those it can't price
    pub async fn token_prices(&self) -> Result<Vec<TokenPrice>, PriceError> {
        let tokens: Vec<TokenSummary> = self.get("/token/all".to_string(), &[]).await?;
        Ok(token_prices(tokens))
    }

    async fn chart(
        &self,
        ledger: CanisterId,
        level: &str,
        page: u32,
        limit: u32,
    ) -> Result<Vec<Candle>, PriceError> {
        let page: Page = self
            .get(
                format!("/token/{ledger}/chart/{level}"),
                &[("page", page), ("limit", limit)],
            )
            .await?;
        Ok(page.content)
    }

    // Tries again once if it's refused for too many requests, which happens now and then even to
    // a few at once, without saying how long to wait
    async fn get<T: DeserializeOwned>(
        &self,
        path: String,
        query: &[(&str, u32)],
    ) -> Result<T, PriceError> {
        match self.get_once(&path, query).await {
            Err(PriceError::RateLimited(_)) => {
                tokio::time::sleep(RETRY_DELAY).await;
                self.get_once(&path, query).await
            }
            result => result,
        }
    }

    async fn get_once<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, u32)],
    ) -> Result<T, PriceError> {
        let _permit = self.permits.acquire().await.map_err(anyhow::Error::from)?;
        let response = self
            .http
            .get(format!("{BASE_URL}{path}"))
            .query(query)
            .timeout(TIMEOUT)
            .send()
            .await
            .map_err(anyhow::Error::from)?;

        match response.status() {
            StatusCode::TOO_MANY_REQUESTS => Err(PriceError::RateLimited("ICPSwap")),
            status if !status.is_success() => {
                Err(anyhow!("ICPSwap returned {status} for {path}").into())
            }
            _ => {
                let envelope: Envelope<T> = response.json().await.map_err(anyhow::Error::from)?;
                data(envelope)
            }
        }
    }
}

fn data<T>(envelope: Envelope<T>) -> Result<T, PriceError> {
    match (envelope.code, envelope.data) {
        (200, Some(data)) => Ok(data),
        (429, _) => Err(PriceError::RateLimited("ICPSwap")),
        (code, _) => Err(anyhow!(
            "ICPSwap returned {code}: {}",
            envelope.message.unwrap_or_default()
        )
        .into()),
    }
}

/// A token's latest price on ICPSwap, and how much is in its pools
#[derive(Clone, Debug, PartialEq)]
pub struct TokenPrice {
    pub ledger: CanisterId,
    pub usd: f64,
    pub tvl_usd: f64,
}

// One of /token/all's tokens. Its numbers are decimal strings, and a price of "0" means it can't
// price the token.
#[derive(Deserialize)]
struct TokenSummary {
    #[serde(rename = "tokenLedgerId")]
    ledger: String,
    price: Option<String>,
    #[serde(rename = "tvlUSD")]
    tvl_usd: Option<String>,
}

fn token_prices(tokens: Vec<TokenSummary>) -> Vec<TokenPrice> {
    tokens
        .into_iter()
        .filter_map(|token| {
            let ledger = Principal::from_text(&token.ledger).ok()?.into();
            let usd: f64 = token.price?.parse().ok()?;
            let tvl_usd = token
                .tvl_usd
                .and_then(|tvl| tvl.parse().ok())
                .unwrap_or(0.0);
            (usd.is_finite() && usd > 0.0).then_some(TokenPrice {
                ledger,
                usd,
                tvl_usd,
            })
        })
        .collect()
}

// Each candle's last price, at its last trade, oldest first. Candles from before a token's USD price
// was tracked have no close, or a close of 0.
fn points(candles: impl Iterator<Item = Candle>) -> Vec<PricePoint> {
    let mut points: Vec<PricePoint> = candles
        .filter_map(|candle| {
            let price: f64 = candle.close?.parse().ok()?;
            (price.is_finite() && price > 0.0).then_some(PricePoint {
                time: candle.close_time.unwrap_or(candle.begin_time),
                price,
            })
        })
        .collect();
    points.sort_by_key(|p| p.time);
    points.dedup_by_key(|p| p.time);
    points
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_agent::export::Principal;

    // Trimmed from real responses, newest first
    const ICP_DAILY: &str = r#"{"code":200,"message":null,"data":{"totalElements":1386,"content":[
        {"snapshotTime":1749637782000,"level":"d1","tokenLedgerId":"ryjl3-tyaaa-aaaaa-aaaba-cai","price":"12.401800000000000000","low":"11.965000000000000000","high":"12.414600000000000000","open":"12.311500000000000000","close":"12.401800000000000000","openTime":1716509946000,"closeTime":1716595137000,"beginTime":1716508800000,"endTime":1716595200000},
        {"snapshotTime":1749637680000,"level":"d1","tokenLedgerId":"ryjl3-tyaaa-aaaaa-aaaba-cai","price":"12.288000000000000000","low":"11.894450000000000000","high":"12.880950000000000000","open":"12.880950000000000000","close":"12.288000000000000000","openTime":1716450092000,"closeTime":1716507614000,"beginTime":1716422400000,"endTime":1716508800000},
        {"snapshotTime":1749637499000,"level":"d1","tokenLedgerId":"ryjl3-tyaaa-aaaaa-aaaba-cai","price":"0.000000000000000000","low":null,"high":null,"open":null,"close":null,"openTime":null,"closeTime":null,"beginTime":1716336000000,"endTime":1716422400000}
    ],"page":null,"limit":365}}"#;

    const CHAT_HOURLY_OUTLIER: &str = r#"{"code":200,"message":null,"data":{"totalElements":25926,"content":[
        {"snapshotTime":1791727293000,"level":"h1","tokenLedgerId":"2ouva-viaaa-aaaaq-aaamq-cai","price":"0.008644895775479200","tvlUSD":"86162.380933582391170127","txCount":"49","low":"0.008644895775479200","high":"0.052162558509264900","open":"0.049962825861764900","close":"0.050507180838758800","openTime":1791723691000,"closeTime":1791727154000,"beginTime":1791723600000,"endTime":1791727200000}
    ],"page":1,"limit":365}}"#;

    fn parse(json: &str) -> Result<Vec<PricePoint>, PriceError> {
        let envelope: Envelope<Page> = serde_json::from_str(json).unwrap();
        data(envelope).map(|page| points(page.content.into_iter()))
    }

    #[test]
    fn candles_map_to_their_closes_oldest_first() {
        assert_eq!(
            parse(ICP_DAILY).unwrap(),
            vec![
                PricePoint {
                    time: 1716507614000,
                    price: 12.288
                },
                PricePoint {
                    time: 1716595137000,
                    price: 12.4018
                },
            ]
        );
    }

    #[test]
    fn the_close_is_used_rather_than_the_price() {
        let points = parse(CHAT_HOURLY_OUTLIER).unwrap();
        assert_eq!(points[0].price, 0.0505071808387588);
    }

    #[test]
    fn an_unknown_token_has_no_prices() {
        let json = r#"{"code":200,"message":null,"data":{"totalElements":0,"content":[],"page":1,"limit":5}}"#;
        assert!(parse(json).unwrap().is_empty());
    }

    #[test]
    fn errors_come_in_the_envelope() {
        let rate_limited = r#"{"code":429,"message":"Too many requests."}"#;
        assert!(matches!(
            parse(rate_limited),
            Err(PriceError::RateLimited("ICPSwap"))
        ));

        let bad_level = r#"{"code":701,"message":"level only support h1/d1","data":null}"#;
        assert!(matches!(parse(bad_level), Err(PriceError::Other(_))));
    }

    #[test]
    fn token_prices_leave_out_those_it_cant_price() {
        // Trimmed from /token/all, which gives "0" for a price it doesn't know
        let json = r#"{"code":200,"message":null,"data":[
            {"tokenLedgerId":"2ouva-viaaa-aaaaq-aaamq-cai","tokenName":"CHAT","tokenSymbol":"CHAT","price":"0.030800119010086400","priceChange24H":"24.0929","tvlUSD":"60257.35","priceHigh24H":"0.681569537505376200"},
            {"tokenLedgerId":"mxzaz-hqaaa-aaaar-qaada-cai","tokenName":"ckBTC","tokenSymbol":"ckBTC","price":"80670.445463354770314900","tvlUSD":"1200000.5"},
            {"tokenLedgerId":"ryjl3-tyaaa-aaaaa-aaaba-cai","tokenName":null,"tokenSymbol":null,"price":"0.000007515611407881","tvlUSD":null},
            {"tokenLedgerId":"73mez-iiaaa-aaaaq-aaasq-cai","tokenName":"Zero","tokenSymbol":"ZERO","price":"0.000000000000000000","tvlUSD":"1"},
            {"tokenLedgerId":"not a principal","tokenName":"Bad","tokenSymbol":"BAD","price":"1","tvlUSD":"1"}
        ]}"#;
        let envelope: Envelope<Vec<TokenSummary>> = serde_json::from_str(json).unwrap();
        let prices = token_prices(data(envelope).unwrap());

        let ledger = |text: &str| -> CanisterId { Principal::from_text(text).unwrap().into() };
        assert_eq!(
            prices,
            vec![
                TokenPrice {
                    ledger: ledger("2ouva-viaaa-aaaaq-aaamq-cai"),
                    usd: 0.0308001190100864,
                    tvl_usd: 60257.35,
                },
                TokenPrice {
                    ledger: ledger("mxzaz-hqaaa-aaaar-qaada-cai"),
                    usd: 80670.44546335477,
                    tvl_usd: 1200000.5,
                },
                TokenPrice {
                    ledger: ledger("ryjl3-tyaaa-aaaaa-aaaba-cai"),
                    usd: 0.000007515611407881,
                    tvl_usd: 0.0,
                },
            ]
        );
    }

    #[tokio::test]
    #[ignore = "calls ICPSwap"]
    async fn gets_the_latest_prices() {
        let icpswap = IcpSwap::new(crate::prices::http_client().unwrap());
        let prices = icpswap.token_prices().await.unwrap();
        let chat = Principal::from_text("2ouva-viaaa-aaaaq-aaamq-cai").unwrap();
        assert!(prices.iter().any(|p| p.ledger == chat.into()));
        assert!(prices.len() > 300, "{} prices", prices.len());
    }

    #[tokio::test]
    #[ignore = "calls ICPSwap"]
    async fn gets_chats_prices() {
        let chat = Principal::from_text("2ouva-viaaa-aaaaq-aaamq-cai").unwrap();
        let icpswap = IcpSwap::new(crate::prices::http_client().unwrap());
        for period in [Period::Day, Period::Month, Period::Year] {
            let points = icpswap.history(chat.into(), period).await.unwrap();
            let span = points.last().unwrap().time - points[0].time;
            assert!(
                span >= period.millis(),
                "{period:?}: {} points",
                points.len()
            );
        }
    }
}
