//! Token prices for OpenChat, kept in memory. It serves the prices the website shows, ChartBot,
//! which replies to `/chart` with a price chart of one or more crypto tokens, and PriceAlertBot,
//! which tells someone when a token's price goes above or below one they gave. See the README.

use crate::alert_bot::Alerts;
use crate::bot::BotIdentity;
use crate::chart_bot::Chart;
use crate::config::Config;
use crate::prices::{CoinGecko, IcpSwap, Market, Prices};
use anyhow::Result;
use axum::Router;
use axum::http::HeaderValue;
use axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN;
use ic_agent::Agent;
use oc_bots_sdk::api::command::CommandHandlerRegistry;
use oc_bots_sdk::mainnet::mainnet_ic_url;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tracing::{error, info};

mod alert_bot;
mod api;
mod bot;
mod chart_bot;
mod config;
mod prices;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = Config::from_env()?;
    let chart_bot = BotIdentity::new(&config.identity_pem, config.ic_url.clone()).await?;
    info!(principal = %chart_bot.principal, "Price service starting");

    // Prices are mainnet's, whichever OpenChat the bot runs against, so OpenChat's tokens are read
    // from mainnet's Registry, anonymously
    let mainnet = Agent::builder().with_url(mainnet_ic_url()).build()?;
    let http = prices::http_client()?;
    let icpswap = Arc::new(IcpSwap::new(http.clone()));
    let coingecko = Arc::new(CoinGecko::new(http, config.coingecko_api_key));
    let market = Arc::new(Market::new(config.ledger_aliases));
    prices::spawn_refresh(market.clone(), icpswap.clone(), coingecko.clone(), mainnet);

    let prices = Arc::new(Prices::new(market.clone(), coingecko, icpswap));
    let chart_commands =
        CommandHandlerRegistry::new(chart_bot.client_factory).register(Chart::new(prices.clone()));
    let chart_routes = bot::routes(
        chart_bot::definition(chart_commands.definitions()),
        config.oc_public_key.clone(),
        chart_commands,
    );

    // PriceAlertBot, given an identity of its own
    let mut alert_routes = None;
    if let Some(pem) = &config.alert_bot_identity_pem {
        let alert_bot = BotIdentity::new(pem, config.ic_url.clone()).await?;
        info!(principal = %alert_bot.principal, "PriceAlertBot starting");
        // Without its alerts it's left off, rather than stopping the prices and ChartBot too
        match Alerts::load(Some(config.alerts_file.clone())) {
            Ok(alerts) => {
                let alerts = Arc::new(alerts);
                alert_bot::spawn_checker(
                    alert_bot.client_factory.clone(),
                    market.clone(),
                    alerts.clone(),
                );
                alert_routes = Some(alert_bot::routes(
                    alert_bot.client_factory,
                    prices,
                    market.clone(),
                    alerts,
                    config.oc_public_key.clone(),
                ));
            }
            Err(e) => error!(error = ?e, "Failed to load the alerts, so PriceAlertBot is off"),
        }
    }
    // Locally, its own origin is a port of its own
    if let (Some(alert_routes), Some(port)) = (&alert_routes, config.alert_bot_port) {
        let listener = listen(port).await?;
        info!(port, "PriceAlertBot ready on its own port");
        tokio::spawn(axum::serve(listener, with_layers(alert_routes.clone())).into_future());
    }

    let listener = listen(config.port).await?;
    info!(port = config.port, "Price service ready");
    axum::serve(listener, app(chart_routes, alert_routes, market)).await?;
    Ok(())
}

// ChartBot at the root, with the prices under /v1, and PriceAlertBot under /alerts, which its own
// endpoint, a host of its own, maps to, as OpenChat only registers a bot at an origin
fn app(chart_bot: Router, alert_bot: Option<Router>, market: Arc<Market>) -> Router {
    let mut routes = chart_bot.merge(api::router(market));
    if let Some(alert_bot) = alert_bot {
        routes = routes.nest("/alerts", alert_bot);
    }
    with_layers(routes)
}

async fn listen(port: u16) -> Result<tokio::net::TcpListener> {
    Ok(tokio::net::TcpListener::bind(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), port)).await?)
}

// What every response gets. The website calls the service from the browser, for both the bot and
// the prices, so CORS allows any origin, and every response says so, errors included.
fn with_layers(routes: Router) -> Router {
    routes
        .fallback(api::not_found)
        .layer(CompressionLayer::new())
        .layer(CorsLayer::permissive())
        .layer(SetResponseHeaderLayer::overriding(
            ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prices::{CoinMarket, ListedToken, PricePoint, TokenPrice, now};
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, Response, StatusCode, header};
    use ic_agent::export::Principal;
    use oc_bots_sdk::oc_api::client::ClientFactory;
    use oc_bots_sdk_offchain::AgentRuntime;
    use std::collections::HashMap;
    use tower::ServiceExt;

    const CHAT: &str = "2ouva-viaaa-aaaaq-aaamq-cai";
    const ICP: &str = "ryjl3-tyaaa-aaaaa-aaaba-cai";
    const UNLISTED: &str = "73mez-iiaaa-aaaaq-aaasq-cai";
    const CKUSDC: &str = "xevnm-gaaaa-aaaar-qafnq-cai";

    fn ledger(text: &str) -> oc_bots_sdk::types::CanisterId {
        Principal::from_text(text).unwrap().into()
    }

    // CHAT is listed and priced, with an hour's close, ICP is listed but has no closes yet, and
    // ckUSDC is a stablecoin
    fn market() -> Arc<Market> {
        let market = Arc::new(Market::new(HashMap::new()));
        market.set_listed(
            [(CHAT, "CHAT"), (ICP, "ICP"), (CKUSDC, "ckUSDC")]
                .into_iter()
                .map(|(text, symbol)| ListedToken {
                    ledger: ledger(text),
                    symbol: symbol.to_string(),
                    name: symbol.to_string(),
                    enabled: true,
                })
                .collect(),
        );
        market.set_hourly(
            ledger(CHAT),
            Some(vec![PricePoint {
                time: now() - 60 * 60 * 1000,
                price: 0.029,
            }]),
            now(),
        );
        market.set_latest(
            vec![TokenPrice {
                ledger: ledger(CHAT),
                usd: 0.0281335,
                tvl_usd: 60_000.0,
            }],
            now(),
        );
        market
    }

    async fn get(market: Arc<Market>, uri: &str) -> Response<Body> {
        with_layers(api::router(market))
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn json(response: Response<Body>) -> serde_json::Value {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn header(response: &Response<Body>, name: header::HeaderName) -> &str {
        response.headers()[name].to_str().unwrap()
    }

    #[tokio::test]
    async fn prices_are_keyed_by_ledger_and_cached_for_a_minute() {
        let response = get(market(), "/v1/prices").await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(header(&response, header::CONTENT_TYPE), "application/json");
        assert_eq!(
            header(&response, header::CACHE_CONTROL),
            "public, max-age=60"
        );
        assert_eq!(header(&response, header::ACCESS_CONTROL_ALLOW_ORIGIN), "*");
        let body = json(response).await;
        assert!(body["timestamp"].as_u64().unwrap() > 1_700_000_000_000);
        assert_eq!(
            body["prices"],
            serde_json::json!({
                CHAT: { "usd": 0.0281335, "source": "icpswap" },
                CKUSDC: { "usd": 1.0 },
            })
        );
    }

    #[tokio::test]
    async fn prices_say_where_theyre_from_and_which_sources_must_be_credited() {
        let body = json(get(market(), "/v1/prices").await).await;
        assert_eq!(
            body["sources"],
            serde_json::json!({
                "coingecko": {
                    "name": "CoinGecko",
                    "url": "https://www.coingecko.com",
                    "attribution_required": true,
                },
                "icpswap": {
                    "name": "ICPSwap",
                    "url": "https://www.icpswap.com",
                    "attribution_required": false,
                },
            })
        );

        let body =
            json(get(market(), &format!("/v1/prices/{CKUSDC}/history?range=1d")).await).await;
        assert!(body.get("source").is_none());
        assert_eq!(body["points"][0][1], 1.0);
    }

    #[tokio::test]
    async fn prices_which_havent_loaded_are_a_503() {
        let response = get(Arc::new(Market::new(HashMap::new())), "/v1/prices").await;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(header(&response, header::ACCESS_CONTROL_ALLOW_ORIGIN), "*");
        assert_eq!(header(&response, header::CACHE_CONTROL), "no-store");
        assert_eq!(
            json(response).await,
            serde_json::json!({ "error": "Prices are stale" })
        );
    }

    #[tokio::test]
    async fn a_history_is_points_oldest_first_cached_for_5_minutes() {
        let response = get(market(), &format!("/v1/prices/{CHAT}/history?range=7d")).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header(&response, header::CACHE_CONTROL),
            "public, max-age=300"
        );
        let body = json(response).await;
        assert_eq!(body["ledger"], CHAT);
        assert_eq!(body["source"], "icpswap");
        assert_eq!(body["range"], "7d");
        assert_eq!(body["interval"], "1h");
        let points = body["points"].as_array().unwrap();
        assert_eq!(points.len(), 2);
        assert_eq!(points[0][1], 0.029);
        assert_eq!(points[1][1], 0.0281335);
        assert!(points[0][0].as_u64() < points[1][0].as_u64());
    }

    #[tokio::test]
    async fn history_errors() {
        for (uri, status, error) in [
            (
                format!("/v1/prices/{CHAT}/history?range=30d"),
                StatusCode::BAD_REQUEST,
                "Unsupported range: use 1d or 7d",
            ),
            (
                format!("/v1/prices/{CHAT}/history"),
                StatusCode::BAD_REQUEST,
                "Unsupported range: use 1d or 7d",
            ),
            (
                format!("/v1/prices/{UNLISTED}/history?range=7d"),
                StatusCode::NOT_FOUND,
                "Unknown ledger",
            ),
            (
                "/v1/prices/not-a-ledger/history?range=7d".to_string(),
                StatusCode::NOT_FOUND,
                "Unknown ledger",
            ),
            (
                format!("/v1/prices/{ICP}/history?range=7d"),
                StatusCode::SERVICE_UNAVAILABLE,
                "The token's prices haven't loaded yet",
            ),
            (
                "/v1/nothing".to_string(),
                StatusCode::NOT_FOUND,
                "Not found",
            ),
        ] {
            let response = get(market(), &uri).await;
            assert_eq!(response.status(), status, "{uri}");
            assert_eq!(
                header(&response, header::ACCESS_CONTROL_ALLOW_ORIGIN),
                "*",
                "{uri}"
            );
            assert_eq!(
                json(response).await,
                serde_json::json!({ "error": error }),
                "{uri}"
            );
        }
    }

    #[tokio::test]
    async fn the_largest_other_coins_are_priced_by_symbol_once_theyve_loaded() {
        let market = market();
        let body = json(get(market.clone(), "/v1/prices").await).await;
        assert!(body.get("coins").is_none());

        market.set_coins(
            vec![CoinMarket {
                id: "cardano".to_string(),
                symbol: "ada".to_string(),
                name: "Cardano".to_string(),
                usd: 0.204964,
                updated: now(),
                sparkline: vec![0.21, 0.206],
            }],
            now(),
        );
        let body = json(get(market.clone(), "/v1/prices").await).await;
        assert_eq!(
            body["coins"],
            serde_json::json!({
                "ADA": { "id": "cardano", "name": "Cardano", "usd": 0.204964, "source": "coingecko" }
            })
        );

        let response = get(market.clone(), "/v1/coins/cardano/history?range=7d").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header(&response, header::CACHE_CONTROL),
            "public, max-age=300"
        );
        let body = json(response).await;
        assert_eq!(body["id"], "cardano");
        assert!(body.get("ledger").is_none());
        assert_eq!(body["source"], "coingecko");
        assert_eq!(body["interval"], "1h");
        let prices: Vec<f64> = body["points"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p[1].as_f64().unwrap())
            .collect();
        assert_eq!(prices, [0.21, 0.206, 0.204964]);

        let response = get(market, "/v1/coins/dogecoin/history?range=7d").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            json(response).await,
            serde_json::json!({ "error": "Unknown coin" })
        );
    }

    #[tokio::test]
    async fn a_coins_history_before_the_coins_load_is_a_503() {
        let response = get(market(), "/v1/coins/cardano/history?range=7d").await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(header(&response, header::ACCESS_CONTROL_ALLOW_ORIGIN), "*");
    }

    #[tokio::test]
    async fn responses_are_compressed_and_allow_any_origin() {
        let response = with_layers(api::router(market()))
            .oneshot(
                Request::get("/v1/prices")
                    .header(header::ACCEPT_ENCODING, "gzip")
                    .header(header::ORIGIN, "https://oc.app")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(header(&response, header::CONTENT_ENCODING), "gzip");
        let origins: Vec<_> = response
            .headers()
            .get_all(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .iter()
            .collect();
        assert_eq!(origins, ["*"]);
    }
    // Built and dropped outside the requests' runtime, as the bot's client has a runtime of its own
    #[test]
    fn price_alert_bot_is_served_under_alerts_beside_the_prices() {
        let agent = Agent::builder()
            .with_url("http://127.0.0.1:1")
            .build()
            .unwrap();
        let client_factory = Arc::new(ClientFactory::new(AgentRuntime::new(
            agent,
            tokio::runtime::Runtime::new().unwrap(),
        )));
        let market = market();
        let http = prices::http_client().unwrap();
        let prices = Arc::new(Prices::new(
            market.clone(),
            Arc::new(CoinGecko::new(http.clone(), None)),
            Arc::new(IcpSwap::new(http)),
        ));
        let alert_bot = alert_bot::routes(
            client_factory,
            prices,
            market.clone(),
            Arc::new(Alerts::load(None).unwrap()),
            "OpenChat's key".to_string(),
        );
        let app = app(Router::new(), Some(alert_bot), market);
        let send = |request: Request<Body>| {
            let app = app.clone();
            async move { app.oneshot(request).await.unwrap() }
        };

        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let response = send(
                Request::get("/alerts/bot_definition")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            let definition = json(response).await;
            assert!(
                definition["description"]
                    .as_str()
                    .unwrap()
                    .contains("/alert CHAT above 0.05")
            );

            // Notifications are signed by OpenChat rather than sent with a JWT, so they reach the
            // bot, which refuses a bad signature, where a command without a JWT is a 401
            let notification = Request::post("/alerts/notify")
                .header("x-oc-signature", "bad")
                .body(Body::from("{}"))
                .unwrap();
            assert_eq!(send(notification).await.status(), StatusCode::BAD_REQUEST);
            let command = Request::post("/alerts/execute_command")
                .body(Body::empty())
                .unwrap();
            assert_eq!(send(command).await.status(), StatusCode::UNAUTHORIZED);

            let prices = send(Request::get("/v1/prices").body(Body::empty()).unwrap()).await;
            assert_eq!(prices.status(), StatusCode::OK);
        });
    }
}
