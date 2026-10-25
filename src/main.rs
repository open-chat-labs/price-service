//! Token prices for OpenChat, kept in memory. It serves the prices the website shows, and ChartBot,
//! which replies to `/chart` with a price chart of one or more crypto tokens. See the README.

use crate::chart_bot::Chart;
use crate::config::Config;
use crate::prices::{IcpSwap, Market, Prices};
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
    let market = Arc::new(Market::new(config.ledger_aliases));
    prices::spawn_refresh(market.clone(), icpswap.clone(), mainnet);

    let prices = Arc::new(Prices::new(
        http,
        config.coingecko_api_key,
        market.clone(),
        icpswap,
    ));
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
