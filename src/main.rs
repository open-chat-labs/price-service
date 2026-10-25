//! Token prices for OpenChat, kept in memory. It serves the prices the website shows, and ChartBot,
//! which replies to `/chart` with a price chart of one or more crypto tokens. See the README.

use crate::chart_bot::Chart;
use crate::config::Config;
use crate::prices::{CoinGecko, IcpSwap, Market, Prices};
use anyhow::{Result, anyhow, bail};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN;
use axum::http::{HeaderValue, StatusCode};
use axum::response::Json;
use axum::routing::{get, post};
use axum::{Extension, Router};
use ic_agent::identity::{BasicIdentity, Secp256k1Identity};
use ic_agent::{Agent, Identity};
use oc_bots_sdk::api::command::{CommandHandlerRegistry, CommandResponse};
use oc_bots_sdk::api::definition::BotDefinition;
use oc_bots_sdk::mainnet::mainnet_ic_url;
use oc_bots_sdk::oc_api::client::ClientFactory;
use oc_bots_sdk_offchain::middleware::tower::{ExtractJwtLayer, OpenChatJwt};
use oc_bots_sdk_offchain::{AgentRuntime, env};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tracing::info;

mod api;
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
    let identity = identity_from_pem(&config.identity_pem)?;
    info!(principal = %identity.sender().map_err(|e| anyhow!(e))?, "Price service starting");

    let agent = build_agent(config.ic_url, identity).await?;
    let oc_client_factory = Arc::new(ClientFactory::new(AgentRuntime::new(
        agent,
        tokio::runtime::Runtime::new()?,
    )));

    // Prices are mainnet's, whichever OpenChat the bot runs against, so OpenChat's tokens are read
    // from mainnet's Registry, anonymously
    let mainnet = Agent::builder().with_url(mainnet_ic_url()).build()?;
    let http = prices::http_client()?;
    let icpswap = Arc::new(IcpSwap::new(http.clone()));
    let coingecko = Arc::new(CoinGecko::new(http, config.coingecko_api_key));
    let market = Arc::new(Market::new(config.ledger_aliases));
    prices::spawn_refresh(market.clone(), icpswap.clone(), coingecko.clone(), mainnet);

    let prices = Arc::new(Prices::new(market.clone(), coingecko, icpswap));
    let commands = CommandHandlerRegistry::new(oc_client_factory)
        .register(Chart::new(prices, config.storage_index));

    let app_state = AppState {
        oc_public_key: config.oc_public_key,
        commands,
    };

    let bot = Router::new()
        .route("/execute_command", post(execute_command))
        .route_layer(ExtractJwtLayer::new())
        // OpenChat reads the bot's definition from its endpoint
        .route("/", get(bot_definition))
        .route("/bot_definition", get(bot_definition))
        .with_state(Arc::new(app_state));
    let routes = with_layers(bot.merge(api::router(market)));

    let listener =
        tokio::net::TcpListener::bind(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), config.port))
            .await?;
    info!(port = config.port, "Price service ready");
    axum::serve(listener, routes).await?;
    Ok(())
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

async fn execute_command(
    State(state): State<Arc<AppState>>,
    Extension(OpenChatJwt(jwt)): Extension<OpenChatJwt>,
) -> (StatusCode, Bytes) {
    match state
        .commands
        .execute(&jwt, &state.oc_public_key, env::now())
        .await
    {
        CommandResponse::Success(r) => {
            (StatusCode::OK, Bytes::from(serde_json::to_vec(&r).unwrap()))
        }
        CommandResponse::BadRequest(r) => (
            StatusCode::BAD_REQUEST,
            Bytes::from(serde_json::to_vec(&r).unwrap()),
        ),
        CommandResponse::InternalError(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Bytes::from(serde_json::to_vec(&err).unwrap()),
        ),
        CommandResponse::TooManyRequests => (StatusCode::TOO_MANY_REQUESTS, Bytes::new()),
    }
}

async fn bot_definition(State(state): State<Arc<AppState>>, _body: String) -> Json<BotDefinition> {
    Json(BotDefinition {
        description:
            "Charts the prices of crypto tokens. Try /chart CHAT, or /chart CHAT ICP ckBTC \
            30d to compare several over 30 days."
                .to_string(),
        commands: state.commands.definitions(),
        autonomous_config: None,
        default_subscriptions: None,
        restricted_locations: None,
    })
}

struct AppState {
    oc_public_key: String,
    commands: CommandHandlerRegistry<AgentRuntime>,
}

async fn build_agent(ic_url: String, identity: Box<dyn Identity>) -> Result<Agent> {
    let is_mainnet = ["ic0.app", "icp0.io", "icp-api.io"]
        .iter()
        .any(|host| ic_url.contains(host));

    let agent = Agent::builder()
        .with_url(ic_url)
        .with_boxed_identity(identity)
        .with_ingress_expiry(Duration::from_secs(60))
        .build()?;

    // Mainnet's root key is built into the agent
    if !is_mainnet {
        agent.fetch_root_key().await?;
    }
    Ok(agent)
}

fn identity_from_pem(pem: &str) -> Result<Box<dyn Identity>> {
    if let Ok(identity) = Secp256k1Identity::from_pem(pem.as_bytes()) {
        Ok(Box::new(identity))
    } else if let Ok(identity) = BasicIdentity::from_pem(pem.as_bytes()) {
        Ok(Box::new(identity))
    } else {
        bail!("The bot's PEM is neither a secp256k1 nor an Ed25519 key")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prices::{CoinMarket, ListedToken, PricePoint, TokenPrice, now};
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, Response, header};
    use ic_agent::export::Principal;
    use std::collections::HashMap;
    use tower::ServiceExt;

    const CHAT: &str = "2ouva-viaaa-aaaaq-aaamq-cai";
    const ICP: &str = "ryjl3-tyaaa-aaaaa-aaaba-cai";
    const UNLISTED: &str = "73mez-iiaaa-aaaaq-aaasq-cai";

    fn ledger(text: &str) -> oc_bots_sdk::types::CanisterId {
        Principal::from_text(text).unwrap().into()
    }

    // CHAT is listed and priced, with an hour's close, and ICP is listed but has no closes yet
    fn market() -> Arc<Market> {
        let market = Arc::new(Market::new(HashMap::new()));
        market.set_listed(
            [(CHAT, "CHAT"), (ICP, "ICP")]
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
            serde_json::json!({ CHAT: { "usd": 0.0281335 } })
        );
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
            serde_json::json!({ "ADA": { "id": "cardano", "name": "Cardano", "usd": 0.204964 } })
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
}
