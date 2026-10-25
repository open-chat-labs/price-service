# OpenChat pricing service: API v1 (what the frontend needs)

The OpenChat website reads token prices from this service. The frontend side is built and tested
against this spec: open-chat-labs/open-chat#10235.

## How the frontend uses it
- **`GET /v1/prices`** is the source of every USD value in the app: wallet balances, swap value
  warnings, and cashtag chips (`$CHAT` in a message renders as "$CHAT $0.0281"). Every open client
  polls it every 5 minutes, so it must be cheap and cacheable.
- **`GET /v1/prices/{ledger}/history?range=7d`** is loaded when someone taps a cashtag. Their card
  draws a 7-day line chart and shows the change from the first point to the last.
- The chart bot can use the same data. Nothing here constrains the bot's own endpoints.

## General rules
- **Base URL**: one public HTTPS origin (eg. behind CloudFront). The website gets it from the build
  env var `OC_PRICE_SERVICE_URL`. Paths below are relative to it.
- **Requests**: public, unauthenticated `GET`s with no custom headers, so browsers send them without
  a CORS preflight. They come from the website's web worker.
- **CORS**: `Access-Control-Allow-Origin: *` on **every** response, errors included.
- **Format**: JSON (`Content-Type: application/json`), compressed (gzip or brotli).
- **Status codes**: real HTTP statuses, never a 200 with an error inside (ICPSwap does that). Error
  bodies are `{ "error": "<message>" }`.
- **Keys**: tokens are keyed by **ledger canister id** (text, eg. `2ouva-viaaa-aaaaq-aaamq-cai`),
  never by symbol. Several Registry tokens share a symbol (SNEED, TENDY, AVCD, OGY, NAUT, FOREST,
  DITTO).
- **Coverage**: every token in OpenChat's prod Registry (canister `cpi5u-yiaaa-aaaar-aqw5a-cai`,
  `updates_msgpack`, 142 tokens on 25 Oct 2026). Extra ledgers are fine; the frontend ignores them.
- **Prices**: USD as JSON **numbers**, not strings. Positive and finite.
- **Unknown prices**: a token with no price is **left out**, never sent as `0`. ICPSwap reports
  `"0"` for tokens it can't price (79 of its 527, including ckSOL and ckEURC).
- **Timestamps**: milliseconds since the Unix epoch, as numbers.
- **Versioning**: paths start `/v1/`. A breaking change goes under `/v2/`, with `/v1/` kept until the
  website has moved over. Adding fields is not breaking: the frontend ignores fields it doesn't know.

## `GET /v1/prices`
The latest USD price of every token the service can price.

**200**
```json
{
  "timestamp": 1792926499000,
  "prices": {
    "ryjl3-tyaaa-aaaaa-aaaba-cai": { "usd": 2.58132 },
    "2ouva-viaaa-aaaaq-aaamq-cai": { "usd": 0.0281335 },
    "mxzaz-hqaaa-aaaar-qaada-cai": { "usd": 81157.5 }
  }
}
```

- **`timestamp`**: when the service last refreshed the prices, not when it sent the response.
  Refresh at least every 5 minutes.
- **Staleness is the service's job.** If the last successful refresh is more than an hour old,
  answer **503** `{ "error": "Prices are stale" }` instead of serving old prices.
  - The frontend then keeps the prices it already has.
  - The frontend can't judge staleness itself, because a device's clock can be hours out.
  - This is what went wrong with ICPCoins, the previous source: it stopped updating in July and the
    app kept showing its prices.
- **`prices`**: one object per priced token, so fields can be added later (eg. `change24h`).
- **Which price**: the token's latest market price, with single-trade outliers filtered out.
  - For example, ICPSwap's 24h high for CHAT was $0.68 against a $0.028 price.
  - It should agree with the last point of the token's history.
- **Stablecoins**: price these at 1, since their ICPSwap pools have next to no liquidity. The
  frontend pins them to $1 too.
  - ckUSDC `xevnm-gaaaa-aaaar-qafnq-cai`
  - ckUSDT `cngnf-vqaaa-aaaar-qag4q-cai`
  - OneSec USDC `53nhb-haaaa-aaaar-qbn5q-cai`
  - OneSec USDT `ij33n-oiaaa-aaaar-qbooa-cai`
- **Caching**: `Cache-Control: public, max-age=60`.
- **Size**: ~10 KB uncompressed for ~150 tokens.

## `GET /v1/prices/{ledger}/history?range=7d`
One token's prices over a recent range, for a chart.

**200**
```json
{
  "ledger": "2ouva-viaaa-aaaaq-aaamq-cai",
  "range": "7d",
  "interval": "1h",
  "points": [
    [1792321699000, 0.0327586],
    [1792331970000, 0.0327586],
    [1792335560000, 0.0331021],
    [1792926391000, 0.0281335]
  ]
}
```

- **`range`**: `7d` must be supported, and it's the only one the frontend uses. Others (`1d`,
  `30d`, `1y` with coarser intervals) are up to the service, eg. for the bot. An unsupported range
  is a **400**.
- **`interval`**: the spacing of the points: `1h` for `7d`.
- **`points`**: `[timestamp, usd]` pairs, **oldest first**, all within the range.
  - **The first point is the price at the start of the range**: the last close before the range,
    timestamped at the range's start. Without it, the chart and the "change over 7 days" of a token
    that rarely trades would start at its first trade in the week rather than the week's start.
  - Each later point is an hourly **closing** price: the last trade in that hour, outliers filtered
    out, timestamped when that trade happened.
  - An hour with no trades has no point. Gaps are fine: the frontend scales the chart by time and
    draws it flat from the last point to now.
  - That's about 168 points for `7d`, roughly 4 KB.
- **Edge cases**:
  - A ledger the service doesn't know (not in the Registry): **404**.
  - A known ledger that has never traded: **200** with `"points": []`.
  - A known ledger with no trades in the range: **200** with just the starting point.
- **Speed**: serve it from the service's own store, in under 500 ms. Don't proxy ICPSwap per
  request: its chart endpoint takes 3–20 s and sometimes returns 429.
- **Caching**: `Cache-Control: public, max-age=300`. The browser's HTTP cache then serves a card
  reopened within 5 minutes without a request.

## Additions: the largest coins OpenChat doesn't list
So that cashtags like `$ADA`, `$NEAR` or `$BNB` can show a price and a chart. These are additions to
v1, which a frontend that doesn't know them ignores. The website doesn't use them yet.

- **`GET /v1/prices`** gains **`coins`**:
  - It holds the 250 largest coins on CoinGecko, keyed by symbol in **upper case**.
  - Symbols OpenChat's Registry lists are left out, so a cashtag's symbol means one token. A
    Registry token is looked up in `prices`, and any other symbol in `coins`.
  - Where two coins share a symbol, the larger one is kept.
  - Each coin has its CoinGecko `id` (for its history), `name` and `usd`.
  - When the coins are more than an hour old, `coins` is left out, and `prices` is still served.

  ```json
  "coins": { "ADA": { "id": "cardano", "name": "Cardano", "usd": 0.204927 } }
  ```

- **`GET /v1/coins/{id}/history?range=7d`** is a coin's history, by its `id`. It has the same
  shape, ranges and statuses as a ledger's, with `id` in place of `ledger`.
  - The points are CoinGecko's hourly prices over the last week, ending with the latest price.
  - A coin that isn't among the largest is a **404**.

  ```json
  { "id": "cardano", "range": "7d", "interval": "1h", "points": [[1792358070496, 0.21306], [1792962772000, 0.204927]] }
  ```

- **Refresh:** both come from one CoinGecko call every 6 minutes, so `/v1/prices` grows to ~25 KB
  (~9 KB compressed).

## Exactly what the frontend parses
These are the frontend mappers, `frontend/openchat-agent/src/services/priceService/mappers.ts`.
- **`/v1/prices`**
  - It needs `prices` as an object of `{ usd: number }`. `timestamp` isn't checked.
  - Entries for ledgers outside the Registry, and `usd` values that aren't positive finite numbers,
    are dropped.
  - A response that doesn't parse, or any non-2xx status, means the frontend keeps its last prices.
- **`/history`**
  - It needs `points` as an array of `[number, number]`. The other fields aren't checked.
  - Points whose price isn't a positive finite number are dropped. Points are sorted by timestamp.
  - A non-2xx status, or a response that doesn't parse, means no chart is shown. The rest of the
    card still shows.
- **Base URL**: a trailing slash on it is tolerated.

## Building it from ICPSwap
- **Latest prices**: `GET https://api.icpswap.com/info/token/all`.
  - It gives every token's latest `price`, as strings, with `"0"` meaning unknown.
  - Responses take ~1 s.
- **Hourly candles**: `GET https://api.icpswap.com/info/token/{ledger}/chart/h1?page=1&limit=168`.
  - Responses take 3–20 s.
  - Candles come newest first, in `{ code, data: { content: [...] } }`, with HTTP 200 even on
    errors.
  - Use `close` and `closeTime`. `price`, `high` and `low` carry the outliers.
  - Hours with no trades are skipped, so 168 candles can reach back further than 7 days. Cut at the
    range, and use the last candle before it as the starting point.
- **Daily candles**: `chart/d1` gives daily candles the same way, with `limit` capped at 365.
- **KongSwap** is shut down, and **ICPCoins** stopped updating in Jul/Sep 2026.

## Reference stand-in
The frontend was first tested end to end against a ~70-line Node stand-in, which served this API
straight from ICPSwap. It isn't in this repository. It didn't do these, which this service does:
- include the starting point;
- filter outliers;
- answer 503 when stale;
- return 404 for ledgers outside the Registry (it only knew ICPSwap's tokens).

## Rollout
- The website uses the service once `OC_PRICE_SERVICE_URL` is set in its build scripts.
- Until then it reads prices from ICPSwap directly and shows no charts, so the website and the
  service can ship in either order.
