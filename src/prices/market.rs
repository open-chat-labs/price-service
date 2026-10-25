use super::icpswap::TokenPrice;
use super::registry::ListedToken;
use super::{Period, PricePoint, outliers, within_period};
use ic_agent::export::Principal;
use oc_bots_sdk::types::{CanisterId, TimestampMillis};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};

const MINUTE: u64 = 60 * 1000;
/// Prices aren't served once this long has passed since they were last refreshed
pub const STALE_AFTER: u64 = 60 * MINUTE;
/// How often each token's hourly closes are fetched again
const HOURLY_REFRESH_INTERVAL: u64 = 15 * MINUTE;
/// How long to wait before trying again to fetch a token's hourly closes, after failing to
const HOURLY_RETRY_INTERVAL: u64 = 2 * MINUTE;
// ICPSwap's latest price for a token is taken to be a glitch when it's this many times away from
// the token's latest hourly close, which was an actual trade
const MAX_LATEST_RATIO: f64 = 10.0;

/// Priced at $1, as their ICPSwap pools have next to no liquidity: ckUSDC, ckUSDT, and OneSec's USDC
/// and USDT
const STABLECOINS: [&str; 4] = [
    "xevnm-gaaaa-aaaar-qafnq-cai",
    "cngnf-vqaaa-aaaar-qag4q-cai",
    "53nhb-haaaa-aaaar-qbn5q-cai",
    "ij33n-oiaaa-aaaar-qbooa-cai",
];

/// What the service keeps up to date, in memory, about the tokens OpenChat lists: which they are,
/// their latest prices, and their hourly closes over the last week or so. Background tasks refresh
/// it, so reading it never waits for a price API.
pub struct Market {
    listed: RwLock<Option<Arc<Listed>>>,
    latest: RwLock<Option<Arc<Latest>>>,
    hourly: RwLock<HashMap<CanisterId, Hourly>>,
    in_flight: Mutex<HashSet<CanisterId>>,
    stablecoins: HashSet<CanisterId>,
    // Local ledgers standing in for mainnet ones, so a local OpenChat sees mainnet's prices
    aliases: HashMap<CanisterId, CanisterId>,
}

/// The tokens OpenChat lists, from its Registry
pub struct Listed {
    pub tokens: Vec<ListedToken>,
    ledgers: HashSet<CanisterId>,
}

/// The latest prices, in USD, of the tokens OpenChat lists which have one
pub struct Latest {
    /// When they were refreshed
    pub timestamp: TimestampMillis,
    pub prices: HashMap<CanisterId, f64>,
    // The listed ledgers, those with the most in their ICPSwap pools first, so that the busiest
    // tokens' hourly closes are fetched first
    by_liquidity: Vec<CanisterId>,
}

#[derive(Default)]
struct Hourly {
    // When they were fetched, and the closes, oldest first
    closes: Option<(TimestampMillis, Arc<Vec<PricePoint>>)>,
    last_attempt: TimestampMillis,
    last_failed: bool,
}

#[derive(Debug, PartialEq)]
pub enum History {
    UnknownLedger,
    NotLoaded,
    Stale,
    /// Oldest first, the first being the price at the start of the period, and the last the latest
    /// price
    Points(Vec<PricePoint>),
}

impl Market {
    pub fn new(aliases: HashMap<CanisterId, CanisterId>) -> Self {
        Market {
            listed: RwLock::default(),
            latest: RwLock::default(),
            hourly: RwLock::default(),
            in_flight: Mutex::default(),
            stablecoins: STABLECOINS
                .iter()
                .map(|text| Principal::from_text(text).unwrap().into())
                .collect(),
            aliases,
        }
    }

    pub fn listed(&self) -> Option<Arc<Listed>> {
        self.listed.read().unwrap().clone()
    }

    pub fn set_listed(&self, tokens: Vec<ListedToken>) {
        let ledgers = tokens.iter().map(|t| t.ledger).collect();
        *self.listed.write().unwrap() = Some(Arc::new(Listed { tokens, ledgers }));
    }

    /// The latest prices, keyed by ledger, including local aliases' if there are any, unless
    /// they're stale or haven't loaded yet
    pub fn latest(&self, now: TimestampMillis) -> Option<Arc<Latest>> {
        self.latest
            .read()
            .unwrap()
            .clone()
            .filter(|latest| now.saturating_sub(latest.timestamp) <= STALE_AFTER)
    }

    /// Keeps the prices of the tokens OpenChat lists. Does nothing until the list has loaded.
    pub fn set_latest(&self, mut prices: Vec<TokenPrice>, now: TimestampMillis) {
        let Some(listed) = self.listed() else {
            return;
        };
        prices.sort_by(|a, b| b.tvl_usd.total_cmp(&a.tvl_usd));

        let mut latest = HashMap::new();
        let mut by_liquidity = Vec::new();
        {
            let hourly = self.hourly.read().unwrap();
            for price in prices
                .into_iter()
                .filter(|p| listed.ledgers.contains(&p.ledger))
            {
                by_liquidity.push(price.ledger);
                let last_close = hourly
                    .get(&price.ledger)
                    .and_then(|h| h.closes.as_ref())
                    .and_then(|(_, closes)| closes.last())
                    .map(|p| p.price);
                let usd = match last_close {
                    Some(close) if !within_ratio(price.usd, close, MAX_LATEST_RATIO) => close,
                    _ => price.usd,
                };
                latest.insert(price.ledger, usd);
            }
        }
        // Those ICPSwap doesn't price go last, so their hourly closes are still fetched
        let priced: HashSet<CanisterId> = by_liquidity.iter().copied().collect();
        by_liquidity.extend(
            listed
                .tokens
                .iter()
                .map(|t| t.ledger)
                .filter(|l| !priced.contains(l)),
        );
        for stablecoin in self.stablecoins.intersection(&listed.ledgers) {
            latest.insert(*stablecoin, 1.0);
        }
        for (local, mainnet) in &self.aliases {
            if let Some(usd) = latest.get(mainnet).copied() {
                latest.insert(*local, usd);
            }
        }

        *self.latest.write().unwrap() = Some(Arc::new(Latest {
            timestamp: now,
            prices: latest,
            by_liquidity,
        }));
    }

    pub fn is_pinned(&self, ledger: CanisterId) -> bool {
        self.stablecoins.contains(&self.mainnet(ledger))
    }

    /// The token's prices over the period, from its hourly closes, ending with its latest price.
    /// Only goes back a week or so.
    pub fn history(&self, ledger: CanisterId, period: Period, now: TimestampMillis) -> History {
        let ledger = self.mainnet(ledger);
        let Some(listed) = self.listed() else {
            return History::NotLoaded;
        };
        if !listed.ledgers.contains(&ledger) {
            return History::UnknownLedger;
        }
        let start = now.saturating_sub(period.millis());
        let latest = self.latest(now);

        if self.stablecoins.contains(&ledger) {
            let end = latest.map_or(now, |l| l.timestamp);
            return History::Points(vec![
                PricePoint {
                    time: start,
                    price: 1.0,
                },
                PricePoint {
                    time: end,
                    price: 1.0,
                },
            ]);
        }

        let closes = match self
            .hourly
            .read()
            .unwrap()
            .get(&ledger)
            .and_then(|h| h.closes.clone())
        {
            None => return History::NotLoaded,
            Some((fetched, _)) if now.saturating_sub(fetched) > STALE_AFTER => {
                return History::Stale;
            }
            Some((_, closes)) => closes,
        };

        let mut points = closes.to_vec();
        if let Some(latest) = latest
            && let Some(price) = latest.prices.get(&ledger)
            && points.last().is_none_or(|p| p.time < latest.timestamp)
        {
            points.push(PricePoint {
                time: latest.timestamp,
                price: *price,
            });
        }
        History::Points(within_period(outliers::filter(points), period, now))
    }

    /// The token whose hourly closes are most due to be fetched, if any is: those never fetched
    /// first, the busiest first, then the longest since fetched. It's taken to be in flight until
    /// `set_hourly` is called for it.
    pub fn next_due(&self, now: TimestampMillis) -> Option<CanisterId> {
        let order = match self.latest.read().unwrap().clone() {
            Some(latest) => latest.by_liquidity.clone(),
            None => self.listed()?.tokens.iter().map(|t| t.ledger).collect(),
        };
        let hourly = self.hourly.read().unwrap();
        let mut in_flight = self.in_flight.lock().unwrap();

        let (_, next) = order
            .into_iter()
            .filter(|l| !self.stablecoins.contains(l) && !in_flight.contains(l))
            .filter_map(|ledger| match hourly.get(&ledger) {
                None => Some((0, ledger)),
                Some(h) => {
                    let wait = if h.last_failed {
                        HOURLY_RETRY_INTERVAL
                    } else {
                        HOURLY_REFRESH_INTERVAL
                    };
                    (now >= h.last_attempt + wait).then_some((h.last_attempt, ledger))
                }
            })
            .min_by_key(|(last_attempt, _)| *last_attempt)?;

        in_flight.insert(next);
        Some(next)
    }

    /// Keeps the token's hourly closes, oldest first, or that fetching them failed
    pub fn set_hourly(
        &self,
        ledger: CanisterId,
        closes: Option<Vec<PricePoint>>,
        now: TimestampMillis,
    ) {
        let mut hourly = self.hourly.write().unwrap();
        let state = hourly.entry(ledger).or_default();
        state.last_attempt = now;
        state.last_failed = closes.is_none();
        if let Some(closes) = closes {
            state.closes = Some((now, Arc::new(closes)));
        }
        self.in_flight.lock().unwrap().remove(&ledger);
    }

    fn mainnet(&self, ledger: CanisterId) -> CanisterId {
        self.aliases.get(&ledger).copied().unwrap_or(ledger)
    }
}

fn within_ratio(a: f64, b: f64, max_ratio: f64) -> bool {
    (1.0 / max_ratio..=max_ratio).contains(&(a / b))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 60 * MINUTE;
    const NOW: u64 = 1_792_926_499_000;

    fn ledger(text: &str) -> CanisterId {
        Principal::from_text(text).unwrap().into()
    }

    fn chat() -> CanisterId {
        ledger("2ouva-viaaa-aaaaq-aaamq-cai")
    }

    fn icp() -> CanisterId {
        ledger("ryjl3-tyaaa-aaaaa-aaaba-cai")
    }

    fn ckusdc() -> CanisterId {
        ledger("xevnm-gaaaa-aaaar-qafnq-cai")
    }

    fn unlisted() -> CanisterId {
        ledger("73mez-iiaaa-aaaaq-aaasq-cai")
    }

    fn listed(ledger: CanisterId, symbol: &str) -> ListedToken {
        ListedToken {
            ledger,
            symbol: symbol.to_string(),
            name: symbol.to_string(),
            enabled: true,
        }
    }

    fn price(ledger: CanisterId, usd: f64, tvl_usd: f64) -> TokenPrice {
        TokenPrice {
            ledger,
            usd,
            tvl_usd,
        }
    }

    fn point(time: TimestampMillis, price: f64) -> PricePoint {
        PricePoint { time, price }
    }

    fn market() -> Market {
        let market = Market::new(HashMap::new());
        market.set_listed(vec![
            listed(chat(), "CHAT"),
            listed(icp(), "ICP"),
            listed(ckusdc(), "ckUSDC"),
        ]);
        market
    }

    #[test]
    fn latest_prices_are_only_the_listed_tokens_with_stablecoins_at_1() {
        let market = market();
        market.set_latest(
            vec![
                price(chat(), 0.028, 60_000.0),
                price(icp(), 2.58, 3_000_000.0),
                price(ckusdc(), 0.97, 10.0),
                price(unlisted(), 0.05, 1.0),
            ],
            NOW,
        );

        let latest = market.latest(NOW).unwrap();
        assert_eq!(latest.timestamp, NOW);
        assert_eq!(
            latest.prices,
            HashMap::from([(chat(), 0.028), (icp(), 2.58), (ckusdc(), 1.0)])
        );
    }

    #[test]
    fn latest_prices_go_stale_after_an_hour() {
        let market = market();
        assert!(market.latest(NOW).is_none());

        market.set_latest(vec![price(chat(), 0.028, 1.0)], NOW);
        assert!(market.latest(NOW + HOUR).is_some());
        assert!(market.latest(NOW + HOUR + 1).is_none());
    }

    #[test]
    fn nothing_is_kept_before_the_tokens_are_listed() {
        let market = Market::new(HashMap::new());
        market.set_latest(vec![price(chat(), 0.028, 1.0)], NOW);
        assert!(market.latest(NOW).is_none());
        assert_eq!(
            market.history(chat(), Period::Week, NOW),
            History::NotLoaded
        );
    }

    #[test]
    fn a_latest_price_far_from_the_latest_close_is_replaced_by_it() {
        let market = market();
        market.set_hourly(chat(), Some(vec![point(NOW - HOUR, 0.028)]), NOW);
        market.set_hourly(icp(), Some(vec![point(NOW - HOUR, 2.5)]), NOW);
        market.set_latest(vec![price(chat(), 0.68, 1.0), price(icp(), 2.0, 1.0)], NOW);

        let latest = market.latest(NOW).unwrap();
        assert_eq!(latest.prices[&chat()], 0.028);
        assert_eq!(latest.prices[&icp()], 2.0);
    }

    #[test]
    fn aliases_get_their_mainnet_ledgers_prices_and_history() {
        let local = unlisted();
        let market = Market::new(HashMap::from([(local, chat())]));
        market.set_listed(vec![listed(chat(), "CHAT")]);
        market.set_hourly(chat(), Some(vec![point(NOW - 2 * HOUR, 0.03)]), NOW);
        market.set_latest(vec![price(chat(), 0.028, 1.0)], NOW);

        assert_eq!(market.latest(NOW).unwrap().prices[&local], 0.028);
        assert_eq!(
            market.history(local, Period::Day, NOW),
            market.history(chat(), Period::Day, NOW)
        );
    }

    #[test]
    fn history_starts_at_the_period_and_ends_with_the_latest_price() {
        let market = market();
        let week = Period::Week.millis();
        market.set_hourly(
            chat(),
            Some(vec![
                point(NOW - week - 3 * HOUR, 0.031),
                point(NOW - week - HOUR, 0.032),
                point(NOW - 50 * HOUR, 0.030),
                point(NOW - 49 * HOUR, 0.68),
                point(NOW - 48 * HOUR, 0.029),
                point(NOW - 2 * HOUR, 0.028),
            ]),
            NOW,
        );
        market.set_latest(vec![price(chat(), 0.0281, 1.0)], NOW);

        assert_eq!(
            market.history(chat(), Period::Week, NOW),
            History::Points(vec![
                point(NOW - week, 0.032),
                point(NOW - 50 * HOUR, 0.030),
                point(NOW - 48 * HOUR, 0.029),
                point(NOW - 2 * HOUR, 0.028),
                point(NOW, 0.0281),
            ])
        );
    }

    #[test]
    fn a_token_which_never_traded_has_no_history() {
        let market = market();
        market.set_hourly(icp(), Some(Vec::new()), NOW);
        assert_eq!(
            market.history(icp(), Period::Week, NOW),
            History::Points(Vec::new())
        );
    }

    #[test]
    fn a_token_with_no_trades_in_the_period_starts_and_ends_at_its_last_price() {
        let market = market();
        let week = Period::Week.millis();
        market.set_hourly(icp(), Some(vec![point(NOW - week - HOUR, 2.5)]), NOW);
        assert_eq!(
            market.history(icp(), Period::Week, NOW),
            History::Points(vec![point(NOW - week, 2.5)])
        );

        market.set_latest(vec![price(icp(), 2.5, 1.0)], NOW);
        assert_eq!(
            market.history(icp(), Period::Week, NOW),
            History::Points(vec![point(NOW - week, 2.5), point(NOW, 2.5)])
        );
    }

    #[test]
    fn history_is_unknown_for_unlisted_ledgers_and_not_loaded_until_fetched() {
        let market = market();
        assert_eq!(
            market.history(unlisted(), Period::Week, NOW),
            History::UnknownLedger
        );
        assert_eq!(
            market.history(chat(), Period::Week, NOW),
            History::NotLoaded
        );

        // A failed fetch leaves it not loaded
        market.set_hourly(chat(), None, NOW);
        assert_eq!(
            market.history(chat(), Period::Week, NOW),
            History::NotLoaded
        );
    }

    #[test]
    fn history_goes_stale_an_hour_after_it_was_fetched() {
        let market = market();
        market.set_hourly(chat(), Some(Vec::new()), NOW);
        assert_eq!(
            market.history(chat(), Period::Week, NOW + HOUR + 1),
            History::Stale
        );
    }

    #[test]
    fn stablecoins_history_is_flat_at_1() {
        let market = market();
        assert_eq!(
            market.history(ckusdc(), Period::Week, NOW),
            History::Points(vec![
                point(NOW - Period::Week.millis(), 1.0),
                point(NOW, 1.0)
            ])
        );
    }

    #[test]
    fn the_busiest_tokens_never_fetched_are_due_first_then_the_longest_since_fetched() {
        let market = market();
        market.set_latest(
            vec![
                price(chat(), 0.028, 60_000.0),
                price(icp(), 2.58, 3_000_000.0),
            ],
            NOW,
        );

        // Stablecoins are never fetched, and a token in flight isn't due again
        assert_eq!(market.next_due(NOW), Some(icp()));
        assert_eq!(market.next_due(NOW), Some(chat()));
        assert_eq!(market.next_due(NOW), None);

        market.set_hourly(icp(), Some(Vec::new()), NOW);
        market.set_hourly(chat(), Some(Vec::new()), NOW + 1);
        assert_eq!(market.next_due(NOW + 14 * MINUTE), None);
        assert_eq!(market.next_due(NOW + 15 * MINUTE + 1), Some(icp()));
        assert_eq!(market.next_due(NOW + 15 * MINUTE + 1), Some(chat()));
    }

    #[test]
    fn a_failed_fetch_is_tried_again_sooner() {
        let market = market();
        assert_eq!(market.next_due(NOW), Some(chat()));
        market.set_hourly(chat(), None, NOW);
        assert_ne!(market.next_due(NOW + MINUTE), Some(chat()));
        assert_eq!(market.next_due(NOW + 2 * MINUTE), Some(chat()));
    }
}
