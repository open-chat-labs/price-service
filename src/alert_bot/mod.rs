//! PriceAlertBot, which confirms `/alert CHAT above 0.05` in the chat, then replies to that
//! confirmation when the token's price goes above or below the one given. Deleting the confirmation
//! cancels the alert. `/alerts` lists someone's alerts in the chat, and `/remove_alert` removes one. Prices come from the same market as ChartBot's and the website's,
//! and the alerts are checked each time it refreshes. See the README.

use crate::bot;
use crate::chart_bot::format;
use crate::prices::{Market, PriceError, Prices, Source, Token, now};
use args::AlertArgs;
use async_trait::async_trait;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use oc_bots_sdk::api::command::{
    CommandHandler, CommandHandlerRegistry, EphemeralMessageBuilder, Message, SuccessResult,
};
use oc_bots_sdk::api::definition::{
    AutonomousConfig, BotCommandDefinition, BotCommandParam, BotCommandParamType, BotDefinition,
    IntegerParam, StringParam,
};
use oc_bots_sdk::api::event_notification::{BotEvent, BotEventWrapper, BotLifecycleEvent};
use oc_bots_sdk::oc_api::actions::{ActionArgsBuilder, send_message};
use oc_bots_sdk::oc_api::client::{Client, ClientFactory};
use oc_bots_sdk::types::{
    ActionContext, ActionScope, AutonomousContext, BotCommandContext, BotPermissions, ChatEvent,
    EventIndex, InstallationLocationType, MessageContent, MessageContentInitial, MessagePermission,
    OCErrorCode, TimestampMillis,
};
use oc_bots_sdk_offchain::AgentRuntime;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock};
use store::{Alert, MAX_PER_USER_PER_CHAT};
use tracing::{error, info, warn};

mod args;
mod store;

pub use store::Alerts;

const SOMETHING_WENT_WRONG: &str = "Something went wrong. Please try again.";

/// PriceAlertBot's routes: its definition, commands and notifications, which OpenChat sends it when
/// it's uninstalled from a chat
pub fn routes(
    client_factory: Arc<ClientFactory<AgentRuntime>>,
    prices: Arc<Prices>,
    market: Arc<Market>,
    alerts: Arc<Alerts>,
    oc_public_key: String,
) -> Router {
    let state = Arc::new(BotState {
        prices,
        market,
        alerts: alerts.clone(),
    });
    let commands = CommandHandlerRegistry::new(client_factory)
        .register(SetAlert(state.clone()))
        .register(ListAlerts(state.clone()))
        .register(RemoveAlert(state));
    let notifications = Router::new()
        .route("/notify", post(notify))
        .with_state(Arc::new(Notifications {
            alerts,
            oc_public_key: oc_public_key.clone(),
        }));
    bot::routes(definition(commands.definitions()), oc_public_key, commands).merge(notifications)
}

/// PriceAlertBot's definition, which OpenChat reads from its endpoint. `/alert` sends text, to
/// confirm an alert, and the bot sends text on its own, to post alerts as they fire. The other
/// commands only reply privately, so need no permissions. It's for groups and channels: alerts by
/// direct message are left for later.
pub fn definition(commands: Vec<BotCommandDefinition>) -> BotDefinition {
    BotDefinition {
        description: "Replies here when a token's price goes above or below one you give. Try \
            /alert CHAT above 0.05. /alerts lists yours, and /remove_alert removes one."
            .to_string(),
        commands,
        autonomous_config: Some(AutonomousConfig {
            permissions: BotPermissions::default()
                .with_message(&HashSet::from([MessagePermission::Text])),
        }),
        default_subscriptions: None,
        restricted_locations: Some(HashSet::from([
            InstallationLocationType::Community,
            InstallationLocationType::Group,
        ])),
    }
}

/// How long an alert OpenChat can't post is tried again, at each refresh, eg. while its chat's
/// canister is upgraded. After that, the chat's taken to be gone, as deleting a group doesn't tell
/// its bots.
const GIVE_UP_AFTER: TimestampMillis = 24 * 60 * 60 * 1000;

/// Checks the alerts each time the market's prices refresh, posting each that's met in its chat,
/// then removing it, so that it fires once
pub fn spawn_checker(
    client_factory: Arc<ClientFactory<AgentRuntime>>,
    market: Arc<Market>,
    alerts: Arc<Alerts>,
) {
    let mut refreshed = market.refreshed();
    tokio::spawn(async move {
        // When each alert OpenChat couldn't post was first tried
        let mut failing_since = HashMap::new();
        while refreshed.changed().await.is_ok() {
            let now = now();
            let met = met(&alerts.checkable(), |source| market.price(source, now));
            // An alert no longer met, or removed, starts afresh if it fails again
            failing_since.retain(|id, _| met.iter().any(|(alert, _)| alert.id == *id));
            for (alert, price) in met {
                let posted = post_alert(&client_factory, &alert, price).await;
                if is_done(&alert, posted, &mut failing_since, now) {
                    alerts.remove(alert.id);
                }
            }
        }
    });
}

struct BotState {
    prices: Arc<Prices>,
    market: Arc<Market>,
    alerts: Arc<Alerts>,
}

// The alerts whose prices have reached their targets, each with its price
fn met(alerts: &[Alert], price: impl Fn(&Source) -> Option<f64>) -> Vec<(Alert, f64)> {
    alerts
        .iter()
        .filter_map(|alert| {
            let price = price(&alert.source)?;
            alert
                .direction
                .is_met(price, alert.target)
                .then(|| (alert.clone(), price))
        })
        .collect()
}

#[derive(Debug, PartialEq)]
enum Posted {
    Yes,
    /// eg. as the bot can no longer post in the chat, so trying again would fail too
    Refused,
    /// OpenChat, or the chat's canister, couldn't be reached
    Failed,
}

// Posts the alert in its chat, and its thread if it was set in one, as a reply to its confirmation,
// which shows whose it is
async fn post_alert(
    client_factory: &ClientFactory<AgentRuntime>,
    alert: &Alert,
    price: f64,
) -> Posted {
    let client = client_factory.build(AutonomousContext {
        api_gateway: alert.api_gateway,
        scope: ActionScope::Chat(alert.chat),
    });
    let response = client
        .send_message(MessageContentInitial::from_text(fired(alert, price)))
        .in_thread(alert.thread)
        .replies_to(alert.confirmation)
        .execute_async()
        .await;
    let posted = posted(&response);
    match posted {
        Posted::Yes => info!(id = alert.id, symbol = alert.symbol, "Posted an alert"),
        Posted::Refused => {
            warn!(id = alert.id, chat = ?alert.chat, ?response, "OpenChat refused an alert")
        }
        Posted::Failed => warn!(id = alert.id, ?response, "Failed to post an alert"),
    }
    posted
}

// OpenChat answers C2CError when it couldn't reach the chat's canister, eg. as it was being upgraded
fn posted<E>(response: &Result<send_message::Response, E>) -> Posted {
    match response {
        Ok(send_message::Response::Success(_)) => Posted::Yes,
        Ok(send_message::Response::Error(e)) if !e.matches_code(OCErrorCode::C2CError) => {
            Posted::Refused
        }
        _ => Posted::Failed,
    }
}

// Whether the alert's done with, so it's removed: once it's posted, or refused, so that it isn't
// tried every minute, or once it's failed for a day. Until then it's tried again at each refresh.
fn is_done(
    alert: &Alert,
    posted: Posted,
    failing_since: &mut HashMap<u64, TimestampMillis>,
    now: TimestampMillis,
) -> bool {
    if posted != Posted::Failed {
        failing_since.remove(&alert.id);
        return true;
    }
    let since = *failing_since.entry(alert.id).or_insert(now);
    if now.saturating_sub(since) < GIVE_UP_AFTER {
        return false;
    }
    warn!(id = alert.id, chat = ?alert.chat, "Gave up on posting an alert");
    failing_since.remove(&alert.id);
    true
}

// eg. "CHAT is above $0.05: it's $0.0512 now.", replying to the alert's confirmation, which shows
// whose it is. Without one, eg. as the service stopped before it could note it, it mentions them,
// which OpenChat shows as their username, though it doesn't notify them.
fn fired(alert: &Alert, price: f64) -> String {
    let text = format!(
        "{} is {} {}: it's {} now.",
        alert.symbol,
        alert.direction.word(),
        target_usd(alert.target),
        usd(price)
    );
    match alert.confirmation {
        Some(_) => text,
        None => format!("@UserId({}) {text}", alert.user),
    }
}

// A target as it was given, eg. $4.555 rather than ChartBot's $4.56, with the cents at least, eg.
// $5.00. Rounding it could say a price was above $0.03 when it was above $0.029995.
fn target_usd(target: f64) -> String {
    let text = target.to_string();
    let (whole, decimals) = text.split_once('.').unwrap_or((&text, ""));
    let whole = whole
        .parse::<u64>()
        .map_or_else(|_| whole.to_string(), format::thousands);
    format!("${whole}.{decimals:0<2}")
}

// A price as ChartBot gives it, without zeros past the cents, eg. $0.05 rather than $0.05000
fn usd(price: f64) -> String {
    let text = format::usd(price);
    match text.split_once('.') {
        Some((whole, decimals)) if decimals.len() > 2 => {
            format!("{whole}.{:0<2}", decimals.trim_end_matches('0'))
        }
        _ => text,
    }
}

// eg. "CHAT above $0.05"
fn described(alert: &Alert) -> String {
    format!(
        "{} {} {}",
        alert.symbol,
        alert.direction.word(),
        target_usd(alert.target)
    )
}

/// `/alert CHAT above 0.05`
struct SetAlert(Arc<BotState>);

static SET_ALERT: LazyLock<BotCommandDefinition> = LazyLock::new(|| BotCommandDefinition {
    name: "alert".to_string(),
    description: Some(
        "Replies here when a token's price goes above or below one you give, eg. CHAT above 0.05"
            .to_string(),
    ),
    placeholder: None,
    // A single string parameter, so OpenChat passes everything typed after /alert
    params: vec![BotCommandParam {
        name: "alert".to_string(),
        description: Some("A token, above or below, and a price in USD".to_string()),
        placeholder: Some("CHAT above 0.05".to_string()),
        required: true,
        param_type: BotCommandParamType::StringParam(StringParam {
            min_length: 1,
            max_length: 100,
            choices: Vec::new(),
            multi_line: false,
        }),
    }],
    // Its confirmation is a message in the chat, which the alert replies to when it fires
    permissions: BotPermissions::default().with_message(&HashSet::from([MessagePermission::Text])),
    default_role: None,
    direct_messages: Some(false),
});

#[async_trait]
impl CommandHandler<AgentRuntime> for SetAlert {
    fn definition(&self) -> &BotCommandDefinition {
        &SET_ALERT
    }

    // An alert that's set is confirmed in the chat. One that isn't is explained only to whoever
    // tried to set it.
    async fn execute(
        &self,
        oc_client: Client<AgentRuntime, BotCommandContext>,
    ) -> Result<SuccessResult, String> {
        let context = oc_client.context();
        let text: String = context.command.maybe_arg("alert").unwrap_or_default();
        let result = async {
            let args = args::parse(&text)?;
            let (alert, price) = self.alert(context, args).await?;
            self.set(&oc_client, alert, price).await
        }
        .await;
        match result {
            Ok(confirmation) => Ok(SuccessResult {
                message: Some(confirmation),
            }),
            Err(refusal) => privately(context, refusal),
        }
    }
}

impl SetAlert {
    // The alert, with the token's price now, or why it can't be set
    async fn alert(
        &self,
        context: &BotCommandContext,
        args: AlertArgs,
    ) -> Result<(Alert, f64), String> {
        let BotState { prices, market, .. } = &*self.0;
        let Some(chat) = context.scope.chat().copied() else {
            return Err("Alerts can only be set in a chat.".to_string());
        };

        let token = match prices.find(&args.symbol).await {
            Ok(Some(token)) => token,
            Ok(None) => return Err(format!("Couldn't find {}.", args.symbol)),
            Err(error) => return Err(reply(error)),
        };
        let price = price_now(&token, market, now())?;
        if args.direction.is_met(price, args.target) {
            return Err(format!(
                "{} is already {} {}: it's {}.",
                token.symbol,
                args.direction.word(),
                target_usd(args.target),
                usd(price)
            ));
        }

        let alert = Alert {
            id: 0,
            user: context.command.initiator,
            chat,
            thread: context.scope.thread(),
            confirmation: None,
            api_gateway: context.api_gateway,
            symbol: token.symbol,
            source: token.source,
            direction: args.direction,
            target: args.target,
            set_at: now(),
        };
        Ok((alert, price))
    }

    // Saves the alert, then confirms it in the chat, noting the confirmation for the alert to reply
    // to. It's saved first, so a confirmation is never posted for an alert that can't be kept, and
    // removed again if the confirmation can't be posted. It isn't checked until it's confirmed.
    async fn set(
        &self,
        oc_client: &Client<AgentRuntime, BotCommandContext>,
        alert: Alert,
        price: f64,
    ) -> Result<Message, String> {
        let alerts = &self.0.alerts;
        let alert = match alerts.add(alert) {
            Ok(Some(alert)) => alert,
            Ok(None) => {
                return Err(format!(
                    "You have {MAX_PER_USER_PER_CHAT} alerts in this chat, the most you can. \
                    Remove one with /remove_alert."
                ));
            }
            Err(e) => return Err(internal("Failed to save an alert", e)),
        };

        let (message, response) = oc_client
            .send_message(MessageContentInitial::from_text(confirmation(
                &alert, price,
            )))
            .execute_async_then_return_message()
            .await;
        match (message, confirmed_at(response)) {
            (Some(message), Ok(confirmation)) => {
                alerts.confirmed(alert.id, confirmation);
                Ok(message)
            }
            (_, result) => {
                alerts.remove(alert.id);
                Err(result
                    .err()
                    .unwrap_or_else(|| SOMETHING_WENT_WRONG.to_string()))
            }
        }
    }
}

// Where the confirmation was posted, or what to tell whoever set the alert if it wasn't
fn confirmed_at<E: std::fmt::Debug>(
    response: Result<send_message::Response, E>,
) -> Result<EventIndex, String> {
    match response {
        Ok(send_message::Response::Success(sent)) => Ok(sent.event_index),
        // A command's message is posted as whoever ran it, who must have accepted the chat's rules,
        // which they do by posting a message themselves
        Ok(send_message::Response::Error(e))
            if e.matches_code(OCErrorCode::ChatRulesNotAccepted) =>
        {
            Err(
                "Please accept this chat's rules, by sending a message here, then try again."
                    .to_string(),
            )
        }
        response => Err(internal("Failed to confirm an alert", response)),
    }
}

// eg. "I'll reply here when CHAT goes above $0.05. It's $0.0295 now."
fn confirmation(alert: &Alert, price: f64) -> String {
    format!(
        "I'll reply here when {} goes {} {}. It's {} now.",
        alert.symbol,
        alert.direction.word(),
        target_usd(alert.target),
        usd(price)
    )
}

// The token's price, which the alert's checked against at each refresh, or why there's none
fn price_now(token: &Token, market: &Market, now: TimestampMillis) -> Result<f64, String> {
    if let Some(price) = market.price(&token.source, now) {
        return Ok(price);
    }
    let loaded = match token.source {
        Source::Icp(_) => market.latest(now).is_some(),
        Source::CoinGecko(_) => market.coins(now).is_some(),
    };
    Err(match (loaded, &token.source) {
        (false, _) => {
            "I don't have prices right now. Please try again in a few minutes.".to_string()
        }
        (true, Source::Icp(_)) => format!("There's no price for {} to watch.", token.symbol),
        (true, Source::CoinGecko(_)) => format!(
            "I can't watch {}'s price: I follow OpenChat's tokens and the 250 largest coins.",
            token.symbol
        ),
    })
}

/// `/alerts`, which lists the user's alerts in the chat, numbered for `/remove_alert`
struct ListAlerts(Arc<BotState>);

static LIST_ALERTS: LazyLock<BotCommandDefinition> = LazyLock::new(|| BotCommandDefinition {
    name: "alerts".to_string(),
    description: Some("Lists your alerts in this chat".to_string()),
    placeholder: None,
    params: Vec::new(),
    permissions: BotPermissions::default(),
    default_role: None,
    direct_messages: Some(false),
});

#[async_trait]
impl CommandHandler<AgentRuntime> for ListAlerts {
    fn definition(&self) -> &BotCommandDefinition {
        &LIST_ALERTS
    }

    async fn execute(
        &self,
        oc_client: Client<AgentRuntime, BotCommandContext>,
    ) -> Result<SuccessResult, String> {
        let context = oc_client.context();
        let BotState { market, alerts, .. } = &*self.0;
        let mine = match context.scope.chat() {
            Some(chat) => alerts.of(context.command.initiator, *chat),
            None => Vec::new(),
        };
        let now = now();
        privately(context, listed(&mine, |source| market.price(source, now)))
    }
}

// eg. "1. CHAT above $0.05 (now $0.0295)", one a line. An alert whose price can't be had, eg. as
// its coin has left the 250 largest, isn't checked until it can.
fn listed(alerts: &[Alert], price: impl Fn(&Source) -> Option<f64>) -> String {
    if alerts.is_empty() {
        return "You have no alerts in this chat. Set one with /alert, eg. /alert CHAT above 0.05"
            .to_string();
    }
    alerts
        .iter()
        .enumerate()
        .map(|(i, alert)| match price(&alert.source) {
            Some(price) => format!("{}. {} (now {})", i + 1, described(alert), usd(price)),
            None => format!("{}. {} (no price now)", i + 1, described(alert)),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `/remove_alert 2`, which removes the user's alert numbered 2 by `/alerts`
struct RemoveAlert(Arc<BotState>);

static REMOVE_ALERT: LazyLock<BotCommandDefinition> = LazyLock::new(|| BotCommandDefinition {
    name: "remove_alert".to_string(),
    description: Some(
        "Removes one of your alerts in this chat, by its number in /alerts".to_string(),
    ),
    placeholder: None,
    params: vec![BotCommandParam {
        name: "number".to_string(),
        description: Some("Its number in /alerts".to_string()),
        placeholder: Some("1".to_string()),
        required: true,
        param_type: BotCommandParamType::IntegerParam(IntegerParam {
            min_value: 1,
            max_value: MAX_PER_USER_PER_CHAT as i64,
            choices: Vec::new(),
        }),
    }],
    permissions: BotPermissions::default(),
    default_role: None,
    direct_messages: Some(false),
});

#[async_trait]
impl CommandHandler<AgentRuntime> for RemoveAlert {
    fn definition(&self) -> &BotCommandDefinition {
        &REMOVE_ALERT
    }

    async fn execute(
        &self,
        oc_client: Client<AgentRuntime, BotCommandContext>,
    ) -> Result<SuccessResult, String> {
        let context = oc_client.context();
        let number: i64 = context.command.maybe_arg("number").unwrap_or_default();
        let reply = match (context.scope.chat(), usize::try_from(number)) {
            (Some(chat), Ok(n)) => {
                match self
                    .0
                    .alerts
                    .remove_nth(context.command.initiator, *chat, n)
                {
                    Some(alert) => format!("Removed your alert for {}.", described(&alert)),
                    None => {
                        format!("You have no alert {number} here. /alerts lists yours, numbered.")
                    }
                }
            }
            _ => "/alerts lists yours, numbered.".to_string(),
        };
        privately(context, reply)
    }
}

// A reply only whoever ran the command sees
fn privately(context: &BotCommandContext, text: String) -> Result<SuccessResult, String> {
    let Some(message_id) = context.message_id() else {
        return Err(text);
    };
    Ok(
        EphemeralMessageBuilder::new(MessageContentInitial::from_text(text), message_id)
            .build()
            .into(),
    )
}

struct Notifications {
    alerts: Arc<Alerts>,
    oc_public_key: String,
}

// A notification that isn't OpenChat's is refused with a 4xx, which isn't retried
async fn notify(
    State(state): State<Arc<Notifications>>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let Some(signature) = headers.get("x-oc-signature").and_then(|v| v.to_str().ok()) else {
        return StatusCode::BAD_REQUEST;
    };
    match BotEventWrapper::parse(&body, signature, &state.oc_public_key) {
        Ok(notification) => {
            notified(&state.alerts, notification);
            StatusCode::OK
        }
        Err(error) => {
            warn!(?error, "Refused a notification");
            StatusCode::BAD_REQUEST
        }
    }
}

// Drops a chat's alerts when the bot's uninstalled from it, those set before then, as OpenChat may
// send it again, eg. after the bot was installed again. Cancels an alert when its confirmation's
// deleted, which OpenChat tells the bot about, as the message is its own. Other notifications are
// ignored, and so is a confirmation being restored, as its alert's gone by then.
fn notified(alerts: &Alerts, notification: BotEventWrapper) {
    match notification.event {
        BotEvent::Lifecycle(BotLifecycleEvent::Uninstalled(uninstalled)) => {
            // Older notifications have no timestamp
            let at = match notification.timestamp {
                0 => TimestampMillis::MAX,
                at => at,
            };
            let dropped = alerts.remove_at(uninstalled.location, at);
            info!(location = ?uninstalled.location, dropped, "Uninstalled");
        }
        BotEvent::Chat(chat_event) => {
            if let ChatEvent::Message(message) = &chat_event.event
                && matches!(message.content, MessageContent::Deleted(_))
                && let Some(alert) = alerts.remove_confirmed(
                    chat_event.chat,
                    chat_event.thread,
                    chat_event.event_index,
                )
            {
                info!(
                    id = alert.id,
                    symbol = alert.symbol,
                    "Cancelled an alert whose confirmation was deleted"
                );
            }
        }
        _ => {}
    }
}

fn reply(error: PriceError) -> String {
    match error {
        PriceError::RateLimited(source) => {
            format!("{source} is getting too many requests. Please try again in a minute.")
        }
        PriceError::Other(e) => internal("Failed to find a token", e),
    }
}

fn internal<E: std::fmt::Debug>(what: &str, e: E) -> String {
    error!(error = ?e, "{what}");
    SOMETHING_WENT_WRONG.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prices::{CoinMarket, ListedToken, TokenPrice};
    use ic_agent::export::Principal;
    use oc_bots_sdk::api::event_notification::BotChatEvent;
    use oc_bots_sdk::types::{BotUninstalledEvent, DeletedBy, TextContent};
    use oc_bots_sdk::types::{CanisterId, Chat, InstallationLocation};
    use store::Direction;

    fn id(n: u8) -> CanisterId {
        Principal::from_slice(&[n; 10]).into()
    }

    fn alert(symbol: &str, direction: Direction, target: f64) -> Alert {
        Alert {
            id: 1,
            user: id(1),
            chat: Chat::Group(id(2)),
            thread: None,
            confirmation: None,
            api_gateway: id(3),
            symbol: symbol.to_string(),
            source: Source::CoinGecko(symbol.to_lowercase()),
            direction,
            target,
            set_at: 0,
        }
    }

    fn prices(source: &Source) -> Option<f64> {
        match source {
            Source::CoinGecko(id) if id == "chat" => Some(0.051),
            Source::CoinGecko(id) if id == "icp" => Some(4.0),
            _ => None,
        }
    }

    #[test]
    fn an_alert_is_met_once_its_price_reaches_the_target() {
        let alerts = [
            alert("CHAT", Direction::Above, 0.05),
            alert("CHAT", Direction::Below, 0.05),
            alert("ICP", Direction::Below, 4.0),
            alert("ADA", Direction::Above, 0.0),
        ];

        let met: Vec<(String, f64)> = met(&alerts, prices)
            .into_iter()
            .map(|(a, price)| (described(&a), price))
            .collect();

        // ADA's price isn't known, so it isn't met, however low its target
        assert_eq!(
            met,
            [
                ("CHAT above $0.05".to_string(), 0.051),
                ("ICP below $4.00".to_string(), 4.0)
            ]
        );
    }

    #[test]
    fn an_alert_is_confirmed_then_fires_as_a_reply_to_that() {
        let alert = alert("CHAT", Direction::Above, 0.05);
        assert_eq!(
            confirmation(&alert, 0.02946),
            "I'll reply here when CHAT goes above $0.05. It's $0.02946 now."
        );

        // The reply shows whose alert it is, so it needn't mention them, unless it has no
        // confirmation to reply to
        let confirmed = Alert {
            confirmation: Some(7),
            ..alert.clone()
        };
        assert_eq!(
            fired(&confirmed, 0.0512),
            "CHAT is above $0.05: it's $0.0512 now."
        );
        assert_eq!(
            fired(&alert, 0.0512),
            format!("@UserId({}) CHAT is above $0.05: it's $0.0512 now.", id(1))
        );
    }

    #[test]
    fn a_confirmation_which_couldnt_be_posted_is_explained() {
        assert_eq!(confirmed_at(sent()), Ok(1));
        assert!(
            confirmed_at(refused(OCErrorCode::ChatRulesNotAccepted))
                .unwrap_err()
                .contains("accept this chat's rules")
        );
        assert_eq!(
            confirmed_at(refused(OCErrorCode::InitiatorNotAuthorized)).unwrap_err(),
            SOMETHING_WENT_WRONG
        );
    }

    #[test]
    fn alerts_are_listed_numbered_with_their_prices_now() {
        let alerts = [
            alert("CHAT", Direction::Above, 0.05),
            alert("ADA", Direction::Below, 0.2),
        ];

        assert_eq!(
            listed(&alerts, prices),
            "1. CHAT above $0.05 (now $0.051)\n2. ADA below $0.20 (no price now)"
        );
        assert!(listed(&[], prices).starts_with("You have no alerts"));
    }

    #[test]
    fn prices_drop_the_zeros_past_the_cents() {
        assert_eq!(usd(0.05), "$0.05");
        assert_eq!(usd(0.0512), "$0.0512");
        assert_eq!(usd(0.2), "$0.20");
        assert_eq!(usd(0.002345), "$0.002345");
        assert_eq!(usd(5.0), "$5.00");
        assert_eq!(usd(67123.4), "$67,123");
    }

    #[test]
    fn targets_are_shown_as_they_were_given() {
        assert_eq!(target_usd(0.05), "$0.05");
        assert_eq!(target_usd(4.555), "$4.555");
        assert_eq!(target_usd(0.029995), "$0.029995");
        assert_eq!(target_usd(0.2), "$0.20");
        assert_eq!(target_usd(5.0), "$5.00");
        assert_eq!(target_usd(100_000.0), "$100,000.00");
    }

    fn sent() -> Result<send_message::Response, ()> {
        Ok(send_message::Response::Success(
            send_message::SuccessResult {
                message_id: 1.into(),
                event_index: 1,
                message_index: 1,
                timestamp: 1,
                expires_at: None,
            },
        ))
    }

    fn refused(code: OCErrorCode) -> Result<send_message::Response, ()> {
        Ok(send_message::Response::Error(code.with_message("refused")))
    }

    #[test]
    fn an_alert_which_couldnt_reach_its_chat_is_tried_again() {
        assert_eq!(posted(&sent()), Posted::Yes);
        assert_eq!(
            posted(&refused(OCErrorCode::InitiatorNotAuthorized)),
            Posted::Refused
        );
        // eg. the chat's canister was being upgraded
        assert_eq!(posted(&refused(OCErrorCode::C2CError)), Posted::Failed);
        assert_eq!(posted::<()>(&Err(())), Posted::Failed);
    }

    #[test]
    fn an_alert_is_done_with_once_posted_or_refused_or_after_a_day_of_failing() {
        let alert = alert("CHAT", Direction::Above, 0.05);
        let mut failing_since = HashMap::new();

        assert!(!is_done(&alert, Posted::Failed, &mut failing_since, 1000));
        assert!(!is_done(
            &alert,
            Posted::Failed,
            &mut failing_since,
            1000 + GIVE_UP_AFTER - 1
        ));
        assert!(is_done(
            &alert,
            Posted::Failed,
            &mut failing_since,
            1000 + GIVE_UP_AFTER
        ));
        assert!(failing_since.is_empty());

        assert!(!is_done(&alert, Posted::Failed, &mut failing_since, 1000));
        assert!(is_done(&alert, Posted::Yes, &mut failing_since, 2000));
        assert!(failing_since.is_empty());
        assert!(is_done(&alert, Posted::Refused, &mut failing_since, 3000));
    }

    fn token(symbol: &str, source: Source) -> Token {
        Token {
            symbol: symbol.to_string(),
            name: symbol.to_string(),
            source,
        }
    }

    #[test]
    fn an_alert_is_refused_without_a_price_to_watch() {
        let chat = id(10);
        let unpriced = id(11);
        let market = Market::new(HashMap::new());
        let chat_token = token("CHAT", Source::Icp(chat));
        let ada = token("ADA", Source::CoinGecko("cardano".to_string()));
        let pepe = token("PEPE", Source::CoinGecko("pepe".to_string()));

        let no_prices = "I don't have prices right now. Please try again in a few minutes.";
        assert_eq!(
            price_now(&chat_token, &market, now()).unwrap_err(),
            no_prices
        );
        assert_eq!(price_now(&ada, &market, now()).unwrap_err(), no_prices);

        market.set_listed(
            [(chat, "CHAT"), (unpriced, "UNPRICED")]
                .into_iter()
                .map(|(ledger, symbol)| ListedToken {
                    ledger,
                    symbol: symbol.to_string(),
                    name: symbol.to_string(),
                    enabled: true,
                })
                .collect(),
        );
        market.set_latest(
            vec![TokenPrice {
                ledger: chat,
                usd: 0.0295,
                tvl_usd: 60_000.0,
            }],
            now(),
        );
        market.set_coins(
            vec![CoinMarket {
                id: "cardano".to_string(),
                symbol: "ada".to_string(),
                name: "Cardano".to_string(),
                usd: 0.2,
                updated: now(),
                sparkline: Vec::new(),
            }],
            now(),
        );

        assert_eq!(price_now(&chat_token, &market, now()), Ok(0.0295));
        assert_eq!(price_now(&ada, &market, now()), Ok(0.2));
        assert_eq!(
            price_now(&token("UNPRICED", Source::Icp(unpriced)), &market, now()).unwrap_err(),
            "There's no price for UNPRICED to watch."
        );
        // a coin outside the 250 largest
        assert!(
            price_now(&pepe, &market, now())
                .unwrap_err()
                .starts_with("I can't watch PEPE's price")
        );
    }

    fn uninstalled(location: InstallationLocation, timestamp: TimestampMillis) -> BotEventWrapper {
        BotEventWrapper {
            api_gateway: id(3),
            event: BotEvent::Lifecycle(BotLifecycleEvent::Uninstalled(BotUninstalledEvent {
                uninstalled_by: id(1),
                location,
            })),
            timestamp,
            notification_index: 1,
        }
    }

    fn message_event(content: MessageContent, event_index: EventIndex) -> BotEventWrapper {
        BotEventWrapper {
            api_gateway: id(3),
            event: BotEvent::Chat(BotChatEvent {
                event: ChatEvent::Message(Box::new(oc_bots_sdk::types::Message {
                    message_index: 4,
                    message_id: 5.into(),
                    sender: id(9),
                    content,
                    sender_context: None,
                    replies_to: None,
                    reactions: Vec::new(),
                    tips: Vec::new(),
                    thread_summary: None,
                    edited: false,
                    forwarded: false,
                    block_level_markdown: false,
                    og_previews: Vec::new(),
                })),
                chat: Chat::Group(id(2)),
                thread: None,
                event_index,
                latest_event_index: event_index,
            }),
            timestamp: 1000,
            notification_index: 1,
        }
    }

    #[test]
    fn deleting_a_confirmation_cancels_its_alert() {
        let alerts = Alerts::load(None).unwrap();
        let alert = alerts
            .add(alert("CHAT", Direction::Above, 0.05))
            .unwrap()
            .unwrap();
        alerts.confirmed(alert.id, 7);
        let deleted = || {
            MessageContent::Deleted(DeletedBy {
                deleted_by: id(1),
                timestamp: 1000,
            })
        };

        // Another message deleted, or the confirmation restored, leaves it be
        notified(&alerts, message_event(deleted(), 8));
        let restored = MessageContent::Text(TextContent {
            text: "I'll reply here".to_string(),
        });
        notified(&alerts, message_event(restored, 7));
        assert_eq!(alerts.all().len(), 1);

        notified(&alerts, message_event(deleted(), 7));
        assert!(alerts.all().is_empty());
    }

    #[test]
    fn uninstalling_drops_the_alerts_set_before_then_in_its_chats() {
        let alerts = Alerts::load(None).unwrap();
        let group = id(2);
        alerts.add(alert("CHAT", Direction::Above, 0.05)).unwrap();
        let since = alerts
            .add(Alert {
                set_at: 2000,
                ..alert("ICP", Direction::Below, 4.0)
            })
            .unwrap()
            .unwrap();

        notified(
            &alerts,
            uninstalled(InstallationLocation::Group(id(9)), 1000),
        );
        assert_eq!(alerts.all().len(), 2);
        notified(
            &alerts,
            uninstalled(InstallationLocation::Group(group), 1000),
        );
        assert_eq!(alerts.all(), [since]);
        // a notification without a timestamp drops them all
        notified(&alerts, uninstalled(InstallationLocation::Group(group), 0));
        assert!(alerts.all().is_empty());
    }

    #[test]
    fn it_only_sends_text() {
        let definition = definition(vec![
            SET_ALERT.clone(),
            LIST_ALERTS.clone(),
            REMOVE_ALERT.clone(),
        ]);
        let text =
            BotPermissions::default().with_message(&HashSet::from([MessagePermission::Text]));

        // /alert confirms an alert in the chat, while the others reply privately
        for command in &definition.commands {
            let expected = match command.name.as_str() {
                "alert" => text.clone(),
                _ => BotPermissions::default(),
            };
            assert_eq!(command.permissions, expected, "/{}", command.name);
        }
        assert_eq!(definition.autonomous_config.unwrap().permissions, text);
    }
}
