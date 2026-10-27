//! What every bot here serves OpenChat: its definition, and its commands, run from the JWT OpenChat
//! sends, which only a bot whose identity OpenChat knows can act on

use anyhow::{Result, anyhow, bail};
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{get, post};
use ic_agent::identity::{BasicIdentity, Secp256k1Identity};
use ic_agent::{Agent, Identity};
use oc_bots_sdk::api::command::{CommandHandlerRegistry, CommandResponse};
use oc_bots_sdk::api::definition::BotDefinition;
use oc_bots_sdk::oc_api::client::ClientFactory;
use oc_bots_sdk::types::CanisterId;
use oc_bots_sdk_offchain::middleware::tower::{ExtractJwtLayer, OpenChatJwt};
use oc_bots_sdk_offchain::{AgentRuntime, env};
use std::sync::Arc;
use std::time::Duration;

/// A bot's identity on OpenChat, and the client it acts with
pub struct BotIdentity {
    pub principal: CanisterId,
    pub client_factory: Arc<ClientFactory<AgentRuntime>>,
}

impl BotIdentity {
    pub async fn new(pem: &str, ic_url: String) -> Result<Self> {
        let identity = identity_from_pem(pem)?;
        let principal = identity.sender().map_err(|e| anyhow!(e))?.into();
        let agent = build_agent(ic_url, identity).await?;
        let client_factory = Arc::new(ClientFactory::new(AgentRuntime::new(
            agent,
            tokio::runtime::Runtime::new()?,
        )));
        Ok(BotIdentity {
            principal,
            client_factory,
        })
    }
}

/// The bot's definition, which OpenChat reads from its endpoint, and its commands
pub fn routes(
    definition: BotDefinition,
    oc_public_key: String,
    commands: CommandHandlerRegistry<AgentRuntime>,
) -> Router {
    let definition = Arc::new(definition);
    let commands = Router::new()
        .route("/execute_command", post(execute_command))
        .route_layer(ExtractJwtLayer::new())
        .with_state(Arc::new(Commands {
            oc_public_key,
            registry: commands,
        }));
    Router::new()
        .route("/", get(bot_definition))
        .route("/bot_definition", get(bot_definition))
        .with_state(definition)
        .merge(commands)
}

struct Commands {
    oc_public_key: String,
    registry: CommandHandlerRegistry<AgentRuntime>,
}

async fn bot_definition(State(definition): State<Arc<BotDefinition>>) -> Json<BotDefinition> {
    Json((*definition).clone())
}

async fn execute_command(
    State(commands): State<Arc<Commands>>,
    Extension(OpenChatJwt(jwt)): Extension<OpenChatJwt>,
) -> (StatusCode, Bytes) {
    match commands
        .registry
        .execute(&jwt, &commands.oc_public_key, env::now())
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
