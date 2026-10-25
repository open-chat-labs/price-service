//! The prices OpenChat's website shows: every USD value, and cashtags' charts. See the README.

use crate::prices::{Market, MarketHistory, Period, now};
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use ic_agent::export::Principal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

pub fn router(market: Arc<Market>) -> Router {
    Router::new()
        .route("/v1/prices", get(prices))
        .route("/v1/prices/{ledger}/history", get(history))
        .with_state(market)
}

#[derive(Serialize)]
struct PricesResponse {
    /// When the prices were refreshed
    timestamp: u64,
    prices: BTreeMap<String, TokenPrice>,
}

#[derive(Serialize)]
struct TokenPrice {
    usd: f64,
}

#[derive(Deserialize)]
struct HistoryQuery {
    range: Option<String>,
}

#[derive(Serialize)]
struct HistoryResponse {
    ledger: String,
    range: &'static str,
    interval: &'static str,
    /// [timestamp, usd], oldest first
    points: Vec<(u64, f64)>,
}

// The latest price of every token OpenChat lists which has one
async fn prices(State(market): State<Arc<Market>>) -> Response {
    let Some(latest) = market.latest(now()) else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "Prices are stale");
    };
    let prices = latest
        .prices
        .iter()
        .map(|(ledger, usd)| (ledger.to_string(), TokenPrice { usd: *usd }))
        .collect();
    ok(
        PricesResponse {
            timestamp: latest.timestamp,
            prices,
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
    let (range, period) = match query.range.as_deref() {
        Some("1d") => ("1d", Period::Day),
        Some("7d") => ("7d", Period::Week),
        _ => return error(StatusCode::BAD_REQUEST, "Unsupported range: use 1d or 7d"),
    };
    let Ok(canister_id) = Principal::from_text(&ledger) else {
        return error(StatusCode::NOT_FOUND, "Unknown ledger");
    };

    match market.history(canister_id.into(), period, now()) {
        MarketHistory::Points(points) => ok(
            HistoryResponse {
                ledger,
                range,
                interval: "1h",
                points: points.into_iter().map(|p| (p.time, p.price)).collect(),
            },
            300,
        ),
        MarketHistory::UnknownLedger => error(StatusCode::NOT_FOUND, "Unknown ledger"),
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
