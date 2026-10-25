# price-service

Token prices for [OpenChat](https://oc.app). It's one long-running container that keeps them in
memory, so it asks the price APIs once however many people want a price. It serves:

- **[The price API](#price-api)**, which OpenChat's website reads every USD value from, and cashtags'
  charts (`$CHAT` in a message), for the tokens OpenChat lists and the largest other coins, eg. ADA.
- **[ChartBot](#chartbot)**, an OpenChat bot which replies to `/chart` with a price chart.

## Price API

What the website needs, as [`docs/api-v1.md`](./docs/api-v1.md) specifies. The website reads the service's
url from its build's `OC_PRICE_SERVICE_URL`.

- **`GET /v1/prices`:** the latest USD prices, ~25 KB (~9 KB compressed). `timestamp` is when they
  were refreshed.
  - **`prices`:** every token OpenChat's Registry lists which has a price, keyed by ledger.
  - **`coins`:** the 250 largest coins on CoinGecko, keyed by symbol in upper case. Symbols OpenChat
    lists are left out, so a symbol means one token, eg. `ICP` is only in `prices`. Each has its
    CoinGecko `id`, for its history.
  - **When coins are stale:** `coins` is left out, rather than failing the whole response.

  ```json
  { "timestamp": 1792937761959,
    "prices": { "2ouva-viaaa-aaaaq-aaamq-cai": { "usd": 0.0308548 } },
    "coins": { "ADA": { "id": "cardano", "name": "Cardano", "usd": 0.204927 } } }
  ```

- **`GET /v1/prices/{ledger}/history?range=7d`:** one token's hourly closes, oldest first, as
  `[timestamp, usd]` points, for a chart. `range=1d` works too. ~6 KB for a week.

  ```json
  { "ledger": "2ouva-viaaa-aaaaq-aaamq-cai", "range": "7d", "interval": "1h",
    "points": [[1792332992198, 0.0327586], [1792335560000, 0.0331021], [1792937761959, 0.0308548]] }
  ```

  - **`GET /v1/coins/{id}/history?range=7d`** is the same for one of the coins, by its CoinGecko
    id, with `id` in place of `ledger`.
  - **The first point** is the price at the start of the range: the last close before it.
  - **The last point** is the latest price, the one `/v1/prices` gives, so the two agree.
  - **Gaps:** an hour without trades has no point.
  - **Edge cases:** a token which never traded has no points. One with no trades in the range has
    just its start and its latest price.
- **Statuses:**
  - **400** for a range other than `1d` or `7d`.
  - **404** for a ledger the Registry doesn't list, a coin that isn't among the largest, or any
    other path.
  - **503** when prices are more than an hour old, or haven't loaded yet. The website then keeps
    the prices it has.
  - Errors are `{ "error": "<message>" }`.
- **Headers:**
  - Every response, errors included, has `Access-Control-Allow-Origin: *`, as the website calls
    the service from the browser.
  - Responses are compressed with gzip or brotli.
  - `/v1/prices` is cached for a minute and history for 5 minutes (`Cache-Control: public`).
    Errors aren't cached.
- **Where the prices come from:** everything is kept in memory and refreshed in the background, so
  no request waits on ICPSwap.
  - **OpenChat's tokens:** the Registry, hourly.
  - **Latest prices:** ICPSwap's `/token/all`, every minute.
  - **Hourly closes:** each token's, every 15 minutes. They're fetched two at a time, busiest
    first, which gets through all ~140 tokens in about 4-10 minutes after a start.
  - **Coins:** CoinGecko's 250 largest, with each one's hourly prices over the last week (its
    "sparkline"), every 6 minutes in a single call. That's about 7,200 calls a month, inside a free
    Demo key's 10,000.
  - A token's history is a 503 until its closes have loaded.
- **Stablecoins** are priced at $1, as their ICPSwap pools have next to no liquidity: ckUSDC,
  ckUSDT, and OneSec's USDC and USDT.
- **Outliers:**
  - **Closes:** a close more than 3x above or below the median of the closes around it is dropped,
    as a single trade at a silly price which the next trades undid.
  - **Latest price:** ICPSwap's latest price can't be told apart from a real move, so it isn't
    filtered that way. It's only replaced by the latest close when it's more than 10x away from it.
- **Locally:** a local OpenChat's ledgers have their own ids. `LEDGER_ALIASES` maps local ledgers
  to mainnet ones, which they then get the prices of, eg. `<local ckBTC>=mxzaz-hqaaa-aaaar-qaada-cai`.

## ChartBot

Replies to `/chart` with a price chart of one or more crypto tokens:

- `/chart CHAT` charts CHAT's price in USD over the last 7 days.
- `/chart CHAT ICP ckBTC 30d` charts how each changed over the last 30 days, as a percentage of its
  price at the start, so they share an axis.

The period is optional and goes anywhere: `1d`, `7d` (the default), `30d`, `90d` or `1y`, or eg.
`24h`, `1w`, `1m`, `3m`, `12m`. Up to 5 tokens can be charted at once. Symbols are matched ignoring
case, and a leading `$` is ignored.

The chart is posted as an image. It and its caption give each token's price at the start of the
period and now, and its change, eg. `CHAT $0.2229 → $0.2345 (+5.2% 7d)`. If a token can't be
found, or there aren't enough prices for it, only whoever ran the command is told.

![/chart CHAT ICP ckBTC 30d](./docs/example.png)

- **Commands:** `/chart` has a single string parameter, so OpenChat passes everything typed after
  `/chart`, eg. `CHAT ICP 30d`, and the bot picks out the symbols and the period. With more than one
  parameter, OpenChat would split what's typed by spaces, one word per parameter.
- **Drawing:** the chart is drawn with [plotters](https://crates.io/crates/plotters) as an SVG, then
  rasterized to a PNG with [resvg](https://crates.io/crates/resvg), with the DejaVu Sans font built
  in, so it needs nothing installed on the host. Its times are in UTC.
- **Uploading:** the PNG is uploaded to OpenChat's storage, as the bot, with the
  [Rust SDK](https://github.com/open-chat-labs/open-chat-bots)'s `Client::upload_image`, then sent as
  an image message.
  - OpenChat gives a bot an upload allowance of 1 GB when it's registered. A chart is about
    40-90 KB, so that's roughly 15,000 charts, after which uploads fail.
  - A group's or community's files are only deleted with it.

## Where ChartBot's prices come from

ChartBot can chart any token. The code is in [`src/prices`](./src/prices), the
API in [`src/api.rs`](./src/api.rs), and the bot in [`src/chart_bot`](./src/chart_bot).

- **Which token a symbol means:** a token OpenChat lists in its Registry, matched ignoring case,
  else the coin with that symbol and the largest market cap on CoinGecko.
  - So `CHAT` is OpenChat's CHAT, where CoinGecko's largest `CHAT` is another chain's.
  - A symbol OpenChat lists, eg. `PEPE` or `USDC`, means OpenChat's token rather than another
    chain's.
- **Sources:**
  - **Tokens OpenChat lists** come from [ICPSwap's API](https://github.com/ICPSwap-Labs/docs),
    which has nearly all of them: hourly up to 30 days, and daily beyond. KongSwap has shut down.
  - **Other tokens** come from [CoinGecko's API](https://docs.coingecko.com): every 5 minutes for a
    day, hourly up to 90 days, and daily beyond.
  - Each candle's close is used, as ICPSwap's other prices can be far off it from single trades.
  - The price at the start of a period is the last one before it, as a token which trades rarely
    may have gaps.
- **Caching and limits:** everything is cached in memory, so a restart starts cold.
  - Tokens OpenChat lists, and the 250 largest coins, are charted from the price API's prices for
    1d and 7d. The coins also resolve symbols without a call.
  - Other prices are cached for 5 minutes for 1d, 15 minutes for 7d and an hour beyond.
  - Which token a symbol means is cached for an hour. Only a symbol outside OpenChat's tokens and the
    250 largest coins asks CoinGecko.
  - Without an API key, CoinGecko allows only a few calls a minute from an address. The service
    stays under 10 a minute, or 30 with a key, and says so if it's asked for more.
  - ICPSwap's charts are slow, often 3-20 seconds. So the service makes at most 4 requests to
    ICPSwap at once, and tries again once if it's refused for too many.

## Running it

It's a long-running server, so the prices it caches are shared between requests.

1. **An identity for the bot,** made with dfx:

   ```bash
   dfx identity new chart_bot
   ```

   Its principal, to register the bot with:

   ```bash
   dfx identity get-principal --identity chart_bot
   ```

   The service takes the identity as `OC_IDENTITY_PEM`, which dfx exports:

   ```bash
   dfx identity export chart_bot
   ```

2. **Run it:**

   ```bash
   OC_IDENTITY_PEM="$(dfx identity export chart_bot)" cargo run --release
   ```

   Or as a container:

   ```bash
   docker build -t price-service .
   docker run -p 8080:8080 -e OC_IDENTITY_PEM="$(dfx identity export chart_bot)" price-service
   ```

   OpenChat's website calls the bot from the browser, so it must be reachable over HTTPS at its
   public url.

3. **Register the bot on OpenChat:** run `/register_bot` in OpenChat, with the service's public url
   as its endpoint and the identity's principal. OpenChat reads its definition from the endpoint.

4. **Install it** in a group, community or direct chat, granting it the permission to send text
   and image messages.

### Locally

With OpenChat running locally from a checkout of
[open-chat](https://github.com/open-chat-labs/open-chat) beside this one (see its README), run:

```bash
./scripts/run_local.sh
```

It makes a `chart_bot` identity if there isn't one, and prints the bot's principal and endpoint to
register it with. It reads the local StorageIndex's id from the `open-chat` checkout, or from the
checkout `OPEN_CHAT_DIR` names.

### On AWS

It isn't deployed yet. It's meant to run as a single container on a host that stays up, eg. EC2
or ECS, rather than Lambda, so its cache is shared. It needs:

- HTTPS in front of it, eg. CloudFront.
  - Its `Cache-Control` headers let the CDN answer most of the website's requests.
  - It should forward `range`, the one query parameter.
  - The bot's `POST /execute_command` mustn't be cached.
- The environment variables below, with the identity kept secret, eg. in SSM Parameter Store.

## Configuration

| Variable | |
|---|---|
| `OC_IDENTITY_PEM` or `OC_IDENTITY_PEM_FILE` | The bot's identity, as PEM text, eg. from `dfx identity export`, or a file |
| `COINGECKO_API_KEY` | Optional, but worth having in production. A CoinGecko Demo API key, which is free, for a higher rate limit than its public API's. The coins' refresh uses about 7,200 of its 10,000 calls a month. |
| `PORT` | Defaults to 3000, or 8080 in the container |
| `OC_IC_URL`, `OC_PUBLIC_KEY`, `OC_STORAGE_INDEX` | Only to use an OpenChat other than mainnet's |
| `LEDGER_ALIASES` | Only locally: `local=mainnet` ledger pairs, separated by commas, to price local ledgers as mainnet ones |
| `RUST_LOG` | Log level, eg. `info` (the default) |

## Development

```bash
cargo test
```

Tests marked `#[ignore]` call the live price APIs:

```bash
cargo test -- --ignored
```

To look at the charts the tests draw, set `CHART_OUTPUT_DIR` to a directory to save them to.

The Rust SDK is a git dependency on
[open-chat-bots#243](https://github.com/open-chat-labs/open-chat-bots/pull/243), for its upload
support, until that's merged. `Cargo.lock` was seeded from open-chat-bots' lockfile, which pins
`dataurl` 0.1.2, a version the SDK needs and crates.io has yanked, so keep it.

## License

AGPL-3.0, as OpenChat's other repositories.
