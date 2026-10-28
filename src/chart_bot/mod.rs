use crate::prices::{self, Period, PriceError, PricePoint, Prices, Provider, Source, Token};
use args::ChartArgs;
use async_trait::async_trait;
use futures::future::join_all;
use oc_bots_sdk::api::command::{CommandHandler, EphemeralMessageBuilder, Message, SuccessResult};
use oc_bots_sdk::api::definition::*;
use oc_bots_sdk::oc_api::actions::send_message;
use oc_bots_sdk::oc_api::client::Client;
use oc_bots_sdk::types::{
    ActionContext, BotCommandContext, MessageContentInitial, PriceChartContent, PriceChartSeries,
    PriceChartSource, PriceChartToken, TimestampMillis,
};
use oc_bots_sdk_offchain::AgentRuntime;
use std::collections::HashSet;
use std::sync::{Arc, LazyLock};
use tracing::{error, warn};

mod args;
pub mod format;

static DEFINITION: LazyLock<BotCommandDefinition> = LazyLock::new(Chart::definition);

/// ChartBot's definition, which OpenChat reads from its endpoint
pub fn definition(commands: Vec<BotCommandDefinition>) -> BotDefinition {
    BotDefinition {
        description:
            "Charts the prices of crypto tokens. Try /chart CHAT, or /chart CHAT ICP ckBTC \
            30d to compare several over 30 days."
                .to_string(),
        commands,
        autonomous_config: None,
        default_subscriptions: None,
        restricted_locations: None,
    }
}

const SOMETHING_WENT_WRONG: &str = "Something went wrong. Please try again.";

// OpenChat refuses a chart with a longer symbol or name, limits the SDK only documents
const MAX_SYMBOL_CHARS: usize = 20;
const MAX_NAME_CHARS: usize = 50;

pub struct Chart {
    prices: Arc<Prices>,
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
    pub fn new(prices: Arc<Prices>) -> Self {
        Chart { prices }
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
            let points = drawable(history.map_err(reply)?);
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

        let chart = price_chart(&charted, period, prices::now());
        let (message, response) = oc_client
            .send_message(MessageContentInitial::PriceChart(chart))
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

// The chart OpenChat draws, from the start of the period to `now`. Readers can see the price at any
// time by hovering over it, or pressing and dragging.
fn price_chart(
    charted: &[(Token, Vec<PricePoint>)],
    period: Period,
    now: TimestampMillis,
) -> PriceChartContent {
    // OpenChat refuses a point outside the chart's time. Each history's period ended a moment
    // before now, when it was fetched, and a source's clock can run a little ahead of ours.
    let start = charted
        .iter()
        .map(|(_, points)| points[0].time)
        .fold(now.saturating_sub(period.millis()), TimestampMillis::min);
    let end = charted
        .iter()
        .map(|(_, points)| points[points.len() - 1].time)
        .fold(now, TimestampMillis::max);
    // An equal share each of the points OpenChat allows
    let max_points = PriceChartContent::MAX_POINTS / charted.len();

    let symbols: Vec<&str> = charted.iter().map(|(t, _)| t.symbol.as_str()).collect();
    PriceChartContent {
        title: title(&symbols, period),
        start,
        end,
        series: charted
            .iter()
            .map(|(token, points)| PriceChartSeries {
                token: match &token.source {
                    Source::Icp(ledger) => PriceChartToken::Ledger(*ledger),
                    Source::CoinGecko(id) => PriceChartToken::Coin(id.clone()),
                },
                symbol: truncate(&token.symbol, MAX_SYMBOL_CHARS),
                name: truncate(&token.name, MAX_NAME_CHARS),
                points: thin(points, max_points)
                    .into_iter()
                    .map(|p| (p.time, p.price))
                    .collect(),
            })
            .collect(),
        // OpenChat credits them under the chart, linked, eg. "Price data by ICPSwap and
        // CoinGecko", as CoinGecko requires
        sources: providers(charted)
            .into_iter()
            .map(|p| PriceChartSource {
                name: p.name().to_string(),
                url: p.url().to_string(),
            })
            .collect(),
    }
}

// eg. "CHAT · 30 days", or "CHAT, ICP and ckBTC · 1 year". It's the chart's text in notifications,
// previews and search, and OpenChat shows it above the chart.
fn title(symbols: &[&str], period: Period) -> String {
    let length = match period {
        // Rather than "year", as in "the past year"
        Period::Year => "1 year".to_string(),
        _ => period.to_string(),
    };
    let suffix = format!(" · {length}");
    let max = PriceChartContent::MAX_TITLE_CHARS - suffix.chars().count();
    format!("{}{suffix}", truncate(&list(symbols), max))
}

// The points OpenChat can draw, as it refuses a chart with a price that isn't above 0, or a time
// that isn't after the one before
fn drawable(points: Vec<PricePoint>) -> Vec<PricePoint> {
    let mut drawable: Vec<PricePoint> = Vec::with_capacity(points.len());
    for point in points {
        if point.price.is_finite()
            && point.price > 0.0
            && drawable.last().is_none_or(|p| point.time > p.time)
        {
            drawable.push(point);
        }
    }
    drawable
}

// At most `max` of the points, which is at least 4. With more, it keeps the first and the last, the
// prices at the start and now, and the high and the low of each stretch between, so the line keeps
// its shape, and a single token's high and low are marked where they were.
fn thin(points: &[PricePoint], max: usize) -> Vec<PricePoint> {
    let [first, middle @ .., last] = points else {
        return points.to_vec();
    };
    if points.len() <= max {
        return points.to_vec();
    }

    let stretches = (max - 2) / 2;
    let mut thinned = vec![*first];
    for stretch in middle.chunks(middle.len().div_ceil(stretches)) {
        let by_price = |a: &usize, b: &usize| stretch[*a].price.total_cmp(&stretch[*b].price);
        let low = (0..stretch.len()).min_by(by_price).unwrap();
        let high = (0..stretch.len()).max_by(by_price).unwrap();
        // In time order, and once if they're the same point
        thinned.push(stretch[low.min(high)]);
        if low != high {
            thinned.push(stretch[low.max(high)]);
        }
    }
    thinned.push(*last);
    thinned
}

// Up to `max` characters, ending in an ellipsis if it's cut short
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        text.chars().take(max - 1).chain(['…']).collect()
    }
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
    use ic_agent::export::Principal;

    const NOW: TimestampMillis = 1_800_000_000_000;
    const HOUR: TimestampMillis = 60 * 60 * 1000;

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

    // Hourly, ending now
    fn hourly(prices: &[f64]) -> Vec<PricePoint> {
        let start = NOW - (prices.len() as u64 - 1) * HOUR;
        prices
            .iter()
            .enumerate()
            .map(|(i, price)| PricePoint {
                time: start + i as u64 * HOUR,
                price: *price,
            })
            .collect()
    }

    // A wavy line, so each point differs from its neighbours
    fn wavy(count: usize) -> Vec<f64> {
        (0..count).map(|i| 2.0 + (i as f64 / 7.0).sin()).collect()
    }

    fn times(points: &[PricePoint]) -> Vec<TimestampMillis> {
        points.iter().map(|p| p.time).collect()
    }

    #[test]
    fn every_source_is_credited_with_a_link() {
        let charted = vec![
            (icp_token("CHAT"), hourly(&[1.0, 2.0])),
            (token("BTC"), hourly(&[1.0, 2.0])),
        ];
        let chart = price_chart(&charted, Period::Week, NOW);
        let sources: Vec<(&str, &str)> = chart
            .sources
            .iter()
            .map(|s| (s.name.as_str(), s.url.as_str()))
            .collect();
        assert_eq!(
            sources,
            [
                ("ICPSwap", "https://www.icpswap.com"),
                ("CoinGecko", "https://www.coingecko.com")
            ]
        );
    }

    #[test]
    fn each_token_is_its_ledger_or_its_coingecko_id() {
        let charted = vec![
            (icp_token("CHAT"), hourly(&[1.0, 2.0])),
            (token("ADA"), hourly(&[1.0, 2.0])),
        ];
        let chart = price_chart(&charted, Period::Week, NOW);
        assert!(matches!(
            chart.series[0].token,
            PriceChartToken::Ledger(l) if l == Principal::anonymous().into()
        ));
        assert!(matches!(&chart.series[1].token, PriceChartToken::Coin(id) if id == "ada"));
        assert_eq!(chart.series[0].symbol, "CHAT");
        assert_eq!(chart.series[1].points, [(NOW - HOUR, 1.0), (NOW, 2.0)]);
    }

    #[test]
    fn a_chart_covers_its_period_up_to_now() {
        let charted = vec![(token("ADA"), hourly(&[1.0, 2.0]))];
        let chart = price_chart(&charted, Period::Week, NOW);
        assert_eq!(chart.start, NOW - Period::Week.millis());
        assert_eq!(chart.end, NOW);
    }

    #[test]
    fn a_chart_takes_in_every_point_fetched_a_moment_before_or_after_now() {
        let start = NOW - Period::Day.millis();
        let before = vec![
            PricePoint {
                time: start - 50,
                price: 1.0,
            },
            PricePoint {
                time: NOW - 50,
                price: 2.0,
            },
        ];
        let after = vec![
            PricePoint {
                time: start,
                price: 1.0,
            },
            PricePoint {
                time: NOW + 20,
                price: 2.0,
            },
        ];
        let charted = vec![(token("ADA"), before), (token("BTC"), after)];
        let chart = price_chart(&charted, Period::Day, NOW);
        assert_eq!((chart.start, chart.end), (start - 50, NOW + 20));
    }

    #[test]
    fn several_tokens_share_the_points_openchat_allows() {
        let charted: Vec<_> = ["A", "B", "C", "D", "E"]
            .iter()
            .map(|s| (token(s), hourly(&wavy(90 * 24))))
            .collect();
        let chart = price_chart(&charted, Period::Quarter, NOW);
        let total: usize = chart.series.iter().map(|s| s.points.len()).sum();
        assert!(total <= PriceChartContent::MAX_POINTS, "{total} points");
        assert!(chart.series.iter().all(|s| s.points.len() > 150));
    }

    #[test]
    fn titles_name_the_tokens_and_the_period() {
        assert_eq!(title(&["CHAT"], Period::Day), "CHAT · 24 hours");
        assert_eq!(
            title(&["CHAT", "ICP"], Period::Month),
            "CHAT and ICP · 30 days"
        );
        assert_eq!(
            title(&["CHAT", "ICP", "ckBTC"], Period::Year),
            "CHAT, ICP and ckBTC · 1 year"
        );
    }

    #[test]
    fn a_long_title_keeps_its_period() {
        let symbols = ["ABCDEFGHIJKLMNOPQRST"; 5];
        let title = title(&symbols, Period::Week);
        assert_eq!(title.chars().count(), PriceChartContent::MAX_TITLE_CHARS);
        assert!(title.ends_with("… · 7 days"), "{title}");
    }

    #[test]
    fn long_names_are_cut_short() {
        let long = Token {
            name: "A token with a name that's longer than OpenChat allows".to_string(),
            ..token("ABC")
        };
        let chart = price_chart(&[(long, hourly(&[1.0, 2.0]))], Period::Week, NOW);
        let name = &chart.series[0].name;
        assert_eq!(name.chars().count(), MAX_NAME_CHARS);
        assert!(name.ends_with("than OpenChat a…"), "{name}");
        assert_eq!(truncate("CHAT", 4), "CHAT");
    }

    #[test]
    fn only_prices_above_0_in_time_order_are_drawn() {
        let point = |time, price| PricePoint { time, price };
        let drawn = drawable(vec![
            point(1, 1.0),
            point(2, 0.0),
            point(3, -1.0),
            point(4, f64::NAN),
            point(5, f64::INFINITY),
            point(6, 2.0),
            point(6, 3.0),
            point(5, 4.0),
            point(7, 5.0),
        ]);
        assert_eq!(drawn, [point(1, 1.0), point(6, 2.0), point(7, 5.0)]);
    }

    #[test]
    fn few_enough_points_are_all_kept() {
        let points = hourly(&wavy(200));
        assert_eq!(thin(&points, 200), points);
    }

    #[test]
    fn thinned_points_keep_the_start_the_latest_and_the_high_and_low() {
        let mut prices = wavy(2_000);
        prices[777] = 10.0;
        prices[1_234] = 0.5;
        let points = hourly(&prices);
        let thinned = thin(&points, 200);

        assert!(
            thinned.len() <= 200 && thinned.len() > 190,
            "{}",
            thinned.len()
        );
        assert_eq!(thinned.first(), points.first());
        assert_eq!(thinned.last(), points.last());
        assert!(thinned.contains(&points[777]) && thinned.contains(&points[1_234]));
        assert!(thinned.iter().all(|p| points.contains(p)));
        assert!(times(&thinned).windows(2).all(|w| w[0] < w[1]));
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
