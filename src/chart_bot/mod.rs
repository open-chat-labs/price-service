use crate::prices::{PriceError, PricePoint, Prices, Provider, Token};
use args::ChartArgs;
use async_trait::async_trait;
use futures::future::join_all;
use oc_bots_sdk::api::command::{CommandHandler, EphemeralMessageBuilder, Message, SuccessResult};
use oc_bots_sdk::api::definition::*;
use oc_bots_sdk::oc_api::actions::send_message;
use oc_bots_sdk::oc_api::client::Client;
use oc_bots_sdk::types::{ActionContext, BotCommandContext, CanisterId, MessageContentInitial};
use oc_bots_sdk_offchain::AgentRuntime;
use render::Line;
use std::collections::HashSet;
use std::sync::{Arc, LazyLock};
use tracing::{error, warn};

mod args;
mod format;
mod render;

static DEFINITION: LazyLock<BotCommandDefinition> = LazyLock::new(Chart::definition);

const SOMETHING_WENT_WRONG: &str = "Something went wrong. Please try again.";

pub struct Chart {
    prices: Arc<Prices>,
    storage_index: CanisterId,
}

#[async_trait]
impl CommandHandler<AgentRuntime> for Chart {
    fn definition(&self) -> &BotCommandDefinition {
        &DEFINITION
    }

    async fn execute(
        &self,
        oc_client: Client<AgentRuntime, BotCommandContext>,
    ) -> Result<SuccessResult, String> {
        let context = oc_client.context();
        // Only a command run in a chat, rather than a community, has a message to reply with
        let Some(message_id) = context.message_id() else {
            return Err("/chart can only be run in a chat".to_string());
        };
        let text: String = context.command.maybe_arg("tokens").unwrap_or_default();

        match self.chart(&oc_client, &text).await {
            Ok(message) => Ok(SuccessResult {
                message: Some(message),
            }),
            // Only whoever ran the command sees what went wrong
            Err(text) => Ok(EphemeralMessageBuilder::new(
                MessageContentInitial::from_text(text),
                message_id,
            )
            .build()
            .into()),
        }
    }
}

impl Chart {
    pub fn new(prices: Arc<Prices>, storage_index: CanisterId) -> Self {
        Chart {
            prices,
            storage_index,
        }
    }

    // Posts the chart, or returns what went wrong
    async fn chart(
        &self,
        oc_client: &Client<AgentRuntime, BotCommandContext>,
        text: &str,
    ) -> Result<Message, String> {
        let ChartArgs { symbols, period } = args::parse(text)?;

        // One at a time, as the first lookup fetches the token lists the others use
        let mut tokens = Vec::new();
        let mut unknown = Vec::new();
        for symbol in &symbols {
            match self.prices.find(symbol).await.map_err(reply)? {
                Some(token) => tokens.push(token),
                None => unknown.push(symbol.as_str()),
            }
        }
        if !unknown.is_empty() {
            return Err(format!("Couldn't find {}.", list(&unknown)));
        }

        let histories = join_all(
            tokens
                .iter()
                .map(|token| self.prices.history(token, period)),
        )
        .await;
        let mut charted: Vec<(Token, Vec<PricePoint>)> = Vec::new();
        let mut no_data = Vec::new();
        for (token, history) in tokens.into_iter().zip(histories) {
            let points = history.map_err(reply)?;
            if points.len() < 2 {
                no_data.push(token.symbol);
            } else {
                charted.push((token, points));
            }
        }
        if !no_data.is_empty() {
            return Err(format!(
                "There aren't enough prices for {} over the past {period}.",
                list(&no_data)
            ));
        }

        let providers = providers(&charted);
        let caption = credits(&providers);
        let png = tokio::task::spawn_blocking(move || {
            let lines: Vec<Line> = charted
                .iter()
                .map(|(token, points)| Line {
                    symbol: &token.symbol,
                    name: &token.name,
                    points,
                })
                .collect();
            let names: Vec<&str> = providers.iter().map(|p| p.name()).collect();
            render::render(&lines, period, &names)
        })
        .await
        .map_err(|e| internal("Rendering panicked", e))?
        .map_err(|e| internal("Failed to render the chart", e))?;

        let mut image = oc_client
            .upload_image(self.storage_index, png, image::ImageFormat::Png)
            .await
            .map_err(|e| internal("Failed to upload the chart", e))?;
        image.caption = caption;

        let (message, response) = oc_client
            .send_message(MessageContentInitial::Image(image))
            .execute_async_then_return_message()
            .await;
        match response {
            Ok(send_message::Response::Success(_)) => {
                message.ok_or_else(|| SOMETHING_WENT_WRONG.to_string())
            }
            Ok(send_message::Response::Error(e)) => Err(internal("Failed to send the chart", e)),
            Err(e) => Err(internal("Failed to send the chart", e)),
        }
    }

    fn definition() -> BotCommandDefinition {
        BotCommandDefinition {
            name: "chart".to_string(),
            description: Some(
                "Chart the price of one or more tokens, eg. CHAT, or CHAT ICP ckBTC 30d"
                    .to_string(),
            ),
            placeholder: Some("Charting...".to_string()),
            // A single string parameter, so OpenChat passes everything typed after /chart, eg.
            // "CHAT ICP 30d"
            params: vec![BotCommandParam {
                name: "tokens".to_string(),
                description: Some(
                    "Token symbols, then optionally a period: 1d, 7d (the default), 30d, 90d or 1y"
                        .to_string(),
                ),
                placeholder: Some("CHAT ICP 30d".to_string()),
                required: true,
                param_type: BotCommandParamType::StringParam(StringParam {
                    min_length: 1,
                    max_length: 100,
                    choices: Vec::new(),
                    multi_line: false,
                }),
            }],
            permissions: BotPermissions::default().with_message(&HashSet::from([
                MessagePermission::Text,
                MessagePermission::Image,
            ])),
            default_role: None,
            direct_messages: Some(true),
        }
    }
}

// The image can't link, so the caption credits the sources which require a linked credit, eg.
// "Price data by [CoinGecko](https://www.coingecko.com)". The image shows everything else.
fn credits(providers: &[Provider]) -> Option<String> {
    let credits: Vec<String> = providers
        .iter()
        .filter(|p| p.attribution_required())
        .map(|p| format!("Price data by [{}]({})", p.name(), p.url()))
        .collect();
    (!credits.is_empty()).then(|| credits.join("\n"))
}

// Where the prices came from, each once
fn providers(charted: &[(Token, Vec<PricePoint>)]) -> Vec<Provider> {
    let mut providers = Vec::new();
    for (token, _) in charted {
        let provider = token.source.provider();
        if !providers.contains(&provider) {
            providers.push(provider);
        }
    }
    providers
}

// eg. "A", "A and B", "A, B and C"
fn list<S: AsRef<str>>(items: &[S]) -> String {
    match items {
        [] => String::new(),
        [only] => only.as_ref().to_string(),
        [rest @ .., last] => format!(
            "{} and {}",
            rest.iter()
                .map(|s| s.as_ref())
                .collect::<Vec<_>>()
                .join(", "),
            last.as_ref()
        ),
    }
}

fn reply(error: PriceError) -> String {
    match error {
        PriceError::RateLimited(source) => {
            format!("{source} is getting too many requests. Please try again in a minute.")
        }
        PriceError::Other(e) => {
            warn!(error = %e, "Failed to get prices");
            SOMETHING_WENT_WRONG.to_string()
        }
    }
}

fn internal<E: std::fmt::Debug>(what: &str, e: E) -> String {
    error!(error = ?e, "{what}");
    SOMETHING_WENT_WRONG.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prices::Source;
    use ic_agent::export::Principal;

    fn token(symbol: &str) -> Token {
        Token {
            symbol: symbol.to_string(),
            name: symbol.to_string(),
            source: Source::CoinGecko(symbol.to_lowercase()),
        }
    }

    fn points(prices: &[f64]) -> Vec<PricePoint> {
        prices
            .iter()
            .enumerate()
            .map(|(i, price)| PricePoint {
                time: i as u64,
                price: *price,
            })
            .collect()
    }

    fn icp_token(symbol: &str) -> Token {
        Token {
            source: Source::Icp(Principal::anonymous().into()),
            ..token(symbol)
        }
    }

    #[test]
    fn only_sources_which_require_it_are_credited_with_a_link() {
        assert_eq!(credits(&[Provider::IcpSwap]), None);
        assert_eq!(
            credits(&[Provider::IcpSwap, Provider::CoinGecko]).as_deref(),
            Some("Price data by [CoinGecko](https://www.coingecko.com)")
        );
    }

    #[test]
    fn each_source_is_named_once() {
        let charted = vec![
            (icp_token("CHAT"), points(&[1.0, 2.0])),
            (token("BTC"), points(&[1.0, 2.0])),
            (icp_token("ICP"), points(&[1.0, 2.0])),
        ];
        assert_eq!(
            providers(&charted),
            [Provider::IcpSwap, Provider::CoinGecko]
        );
    }

    #[test]
    fn lists_read_naturally() {
        assert_eq!(list(&["A"]), "A");
        assert_eq!(list(&["A", "B"]), "A and B");
        assert_eq!(list(&["A", "B", "C"]), "A, B and C");
    }
}
