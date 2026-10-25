//! The prices OpenChat's website shows: every USD value, and cashtags' charts, for the tokens
//! OpenChat lists and for the largest other coins, eg. ADA. See the README.

use crate::prices::{Market, MarketHistory, Period, Provider, now};
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use ic_agent::export::Principal;
use oc_bots_sdk::types::CanisterId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

pub fn router(market: Arc<Market>) -> Router {
    Router::new()
        .route("/v1/prices", get(prices))
        .route("/v1/prices/{ledger}/history", get(history))
        .route("/v1/coins/{id}/history", get(coin_history))
        .with_state(market)
}

#[derive(Serialize)]
struct PricesResponse {
    /// When the prices were refreshed
    timestamp: u64,
    /// By ledger
    prices: BTreeMap<String, TokenPrice>,
    /// The largest coins whose symbols OpenChat doesn't list, by symbol, in upper case. Left out
    /// while they're stale.
    #[serde(skip_serializing_if = "Option::is_none")]
    coins: Option<BTreeMap<String, CoinPrice>>,
    /// Where the prices come from, by the `source` the prices and histories give
    sources: BTreeMap<&'static str, SourceInfo>,
}

#[derive(Serialize)]
struct TokenPrice {
    usd: f64,
    /// Left out for stablecoins, which are priced at $1 rather than from a source
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<&'static str>,
}

#[derive(Serialize)]
struct CoinPrice {
    /// CoinGecko's id, for its history
    id: String,
    name: String,
    usd: f64,
    source: &'static str,
}

#[derive(Serialize)]
struct SourceInfo {
    name: &'static str,
    url: &'static str,
    /// Its prices must be credited, eg. "Price data by CoinGecko" linking to `url`, wherever
    /// they're shown
    attribution_required: bool,
}

#[derive(Deserialize)]
struct HistoryQuery {
    range: Option<String>,
}

#[derive(Serialize)]
struct HistoryResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    ledger: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<&'static str>,
    range: &'static str,
    interval: &'static str,
    /// [timestamp, usd], oldest first
    points: Vec<(u64, f64)>,
}

// The latest price of every token OpenChat lists which has one, and of the largest other coins
async fn prices(State(market): State<Arc<Market>>) -> Response {
    let now = now();
    let Some(latest) = market.latest(now) else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "Prices are stale");
    };
    let prices = latest
        .prices
        .iter()
        .map(|(ledger, usd)| {
            let price = TokenPrice {
                usd: *usd,
                source: icpswap_unless_pinned(&market, *ledger),
            };
            (ledger.to_string(), price)
        })
        .collect();
    let coins = market.coins(now).map(|coins| {
        coins
            .by_symbol
            .iter()
            .map(|(symbol, coin)| {
                let price = CoinPrice {
                    id: coin.id.clone(),
                    name: coin.name.clone(),
                    usd: coin.usd,
                    source: Provider::CoinGecko.id(),
                };
                (symbol.clone(), price)
            })
            .collect()
    });
    ok(
        PricesResponse {
            timestamp: latest.timestamp,
            prices,
            coins,
            sources: sources(),
        },
        60,
    )
}

// One token's prices over a recent range, for a chart
async fn history(
    State(market): State<Arc<Market>>,
    Path(ledger): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> Response {
    let Some((range, period)) = range(&query) else {
        return unsupported_range();
    };
    let Ok(canister_id) = Principal::from_text(&ledger) else {
        return error(StatusCode::NOT_FOUND, "Unknown ledger");
    };
    let canister_id = canister_id.into();
    let history = market.history(canister_id, period, now());
    let source = icpswap_unless_pinned(&market, canister_id);
    history_response(history, range, Some(ledger), None, source, "Unknown ledger")
}

// One of the largest coins' prices over a recent range, by CoinGecko's id
async fn coin_history(
    State(market): State<Arc<Market>>,
    Path(id): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> Response {
    let Some((range, period)) = range(&query) else {
        return unsupported_range();
    };
    let history = market.coin_history(&id, period, now());
    let source = Some(Provider::CoinGecko.id());
    history_response(history, range, None, Some(id), source, "Unknown coin")
}

// Stablecoins are priced at $1, so their prices have no source
fn icpswap_unless_pinned(market: &Market, ledger: CanisterId) -> Option<&'static str> {
    (!market.is_pinned(ledger)).then_some(Provider::IcpSwap.id())
}

fn sources() -> BTreeMap<&'static str, SourceInfo> {
    Provider::ALL
        .into_iter()
        .map(|provider| {
            let info = SourceInfo {
                name: provider.name(),
                url: provider.url(),
                attribution_required: provider.attribution_required(),
            };
            (provider.id(), info)
        })
        .collect()
}

fn range(query: &HistoryQuery) -> Option<(&'static str, Period)> {
    match query.range.as_deref() {
        Some("1d") => Some(("1d", Period::Day)),
        Some("7d") => Some(("7d", Period::Week)),
        _ => None,
    }
}

fn unsupported_range() -> Response {
    error(StatusCode::BAD_REQUEST, "Unsupported range: use 1d or 7d")
}

fn history_response(
    history: MarketHistory,
    range: &'static str,
    ledger: Option<String>,
    id: Option<String>,
    source: Option<&'static str>,
    unknown: &str,
) -> Response {
    match history {
        MarketHistory::Points(points) => ok(
            HistoryResponse {
                ledger,
                id,
                source,
                range,
                interval: "1h",
                points: points.into_iter().map(|p| (p.time, p.price)).collect(),
            },
            300,
        ),
        MarketHistory::Unknown => error(StatusCode::NOT_FOUND, unknown),
        MarketHistory::NotLoaded => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "The token's prices haven't loaded yet",
        ),
        MarketHistory::Stale => error(StatusCode::SERVICE_UNAVAILABLE, "Prices are stale"),
    }
}

/// For any path which isn't served
pub async fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "Not found")
}

fn ok<T: Serialize>(body: T, max_age: u32) -> Response {
    let mut response = Json(body).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_str(&format!("public, max-age={max_age}")).unwrap(),
    );
    response
}

// Errors aren't cached, so a CDN asks again
fn error(status: StatusCode, message: &str) -> Response {
    let mut response = (status, Json(ErrorBody { error: message })).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
}
