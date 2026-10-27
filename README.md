# price-service

Token prices for [OpenChat](https://oc.app). It's one long-running container that keeps them in
memory, so it asks the price APIs once however many people want a price. It serves:

- **[The price API](#price-api)**, which OpenChat's website reads every USD value from, and cashtags'
  charts (`$CHAT` in a message), for the tokens OpenChat lists and the largest other coins, eg. ADA.
- **[ChartBot](#chartbot)**, an OpenChat bot which replies to `/chart` with a price chart.
- **[PriceAlertBot](#pricealertbot)**, an OpenChat bot which tells you in a chat when a token's
  price goes above or below one you gave, eg. `/alert CHAT above 0.05`.

## Price API

What the website needs, as [`docs/api-v1.md`](./docs/api-v1.md) specifies. The website reads the service's
url from its build's `OC_PRICE_SERVICE_URL`.

- **`GET /v1/prices`:** the latest USD prices, ~30 KB (~9 KB compressed). `timestamp` is when they
  were refreshed.
  - **`prices`:** every token OpenChat's Registry lists which has a price, keyed by ledger.
  - **`coins`:** the 250 largest coins on CoinGecko, keyed by symbol in upper case. Symbols OpenChat
    lists are left out, so a symbol means one token, eg. `ICP` is only in `prices`. Each has its
    CoinGecko `id`, for its history.
  - **When coins are stale:** `coins` is left out, rather than failing the whole response.
  - **`source`:** where each price is from, `icpswap` or `coingecko`. Stablecoins have none, as
    they're priced at $1.
  - **`sources`:** each source's `name` and `url`, and whether its prices must be credited where
    they're shown (`attribution_required`). CoinGecko's must be, as its free API requires, eg.
    "Price data by CoinGecko" linking to its `url`, close to the price. See
    [the spec](./docs/api-v1.md#additions-where-prices-come-from-and-crediting-coingecko).

  ```json
  { "timestamp": 1792937761959,
    "prices": { "2ouva-viaaa-aaaaq-aaamq-cai": { "usd": 0.0308548, "source": "icpswap" } },
    "coins": { "ADA": { "id": "cardano", "name": "Cardano", "usd": 0.204927, "source": "coingecko" } },
    "sources": {
      "coingecko": { "name": "CoinGecko", "url": "https://www.coingecko.com", "attribution_required": true },
      "icpswap": { "name": "ICPSwap", "url": "https://www.icpswap.com", "attribution_required": false } } }
  ```

- **`GET /v1/prices/{ledger}/history?range=7d`:** one token's hourly closes, oldest first, as
  `[timestamp, usd]` points, for a chart. `range=1d` works too. ~6 KB for a week.

  ```json
  { "ledger": "2ouva-viaaa-aaaaq-aaamq-cai", "source": "icpswap", "range": "7d", "interval": "1h",
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

The chart is posted as an image, which gives each token's price at the start of the period and
now, and its change. A single token's chart also marks its high and low with their prices, unless
one is the latest price, which has its own dot. Its caption credits where the prices came from. If
a token can't be found, or there aren't enough prices for it, only whoever ran the command is told.

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
- **Credits:** the image's caption credits every source its prices came from, linked, eg. "Price
  data by ICPSwap and CoinGecko". It's in the caption, rather than the image, as an image can't
  link, and CoinGecko's free API requires a linked credit.

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
  - **Longer charts** (30d, 90d and 1y) of those tokens fetch their history once a day, as prices
    more than a week old don't change. Each chart joins it to the market's last week, which is kept
    up to date, thinned to the history's spacing, eg. a price a day for a year. So a coin's 1y chart
    costs one CoinGecko call a day, however often it's asked for.
  - Other tokens' prices are fetched again after 5 minutes for 1d, 15 minutes for 7d and an hour
    beyond.
  - Which token a symbol means is cached for an hour. Only a symbol outside OpenChat's tokens and the
    250 largest coins asks CoinGecko.
  - Without an API key, CoinGecko allows only a few calls a minute from an address. The service
    stays under 10 a minute, or 30 with a key, and says so if it's asked for more.
  - ICPSwap's charts are slow, often 3-20 seconds. So the service makes at most 4 requests to
    ICPSwap at once, and tries again once if it's refused for too many.

## PriceAlertBot

Tells you, in the chat where you asked, when a token's price goes above or below one you gave. It's
a bot of its own, with its own identity, served by the same container as ChartBot, from the same
prices. The code is in [`src/alert_bot`](./src/alert_bot).

- `/alert CHAT above 0.05` sets an alert. The bot tells only you that it's set, with CHAT's price
  now. An alert that's already met, eg. CHAT above $0.05 when it's $0.06, is refused.
- `/alerts` lists your alerts in the chat, numbered, each with its token's price now.
- `/remove_alert 2` removes the second.

When a price reaches its target, the bot posts in the chat, or the thread the alert was set in, eg.
"@alice CHAT is above $0.05: it's $0.0512 now.", and removes the alert, so it fires once.

- **Tokens:** those the price API has, ie. the tokens OpenChat lists and the 250 largest coins,
  matched by symbol as ChartBot matches them. A leading `$` on the symbol or the price is ignored,
  and `above` can be `over` or `>`, and `below` `under` or `<`. Like `/chart`, `/alert` has a single
  string parameter, which the bot splits into the three.
- **Checking:** each time the prices refresh, so every minute for OpenChat's tokens, from ICPSwap,
  and every 6 minutes for the coins, from CoinGecko. A price that crosses a target and comes back
  between two refreshes isn't caught. An alert whose price the service no longer has, eg. as its
  coin has left the 250 largest, isn't checked until it has, and `/alerts` says so.
- **Where:** groups and community channels. Its commands only reply to whoever ran them, so need no
  permissions. Posting the alerts needs one, to send text messages without a command, which OpenChat
  asks for when it's installed.
- **Limits:** 20 alerts per person per chat.
- **Storage:** the alerts are kept in a JSON file, `ALERTS_FILE`, rewritten on each change, so they
  outlast restarts and deploys. On AWS it's on the host, in `/var/lib/oc-price-service`.
  - A file that can't be parsed, eg. one cut short, is moved aside, to `.json.unreadable-<time>`,
    and the bot starts with no alerts. One that can't be read leaves PriceAlertBot off, but not the
    prices or ChartBot.
  - A fired alert is removed even if the file can't be written then, so it isn't posted again.
- **Clearing up:** when the bot's uninstalled from a chat, OpenChat tells it, at `/notify`, and the
  alerts set there before then are dropped. An alert OpenChat won't post, eg. as the bot can no
  longer post in the chat, is dropped too, rather than tried again each minute.
- **Retries:** an alert that couldn't reach OpenChat, or the chat's canister, eg. while it's
  upgraded, is tried again at each refresh, for up to a day. Deleting a group doesn't tell its
  bots, so its alerts go then.
- **Mentions:** the post mentions whoever set the alert, which OpenChat shows as their @username. But
  OpenChat doesn't yet pass a bot's mentions on (`c2c_bot_send_message` sends none), so it doesn't
  notify them as a person's mention would.
- **Its own endpoint:** OpenChat registers a bot only at an origin, eg. `https://example.com` but not
  `https://example.com/alerts`. So PriceAlertBot is served under `/alerts`, and given an origin of
  its own which maps to it: on AWS a CloudFront distribution of its own, and locally a port of its
  own, `ALERT_BOT_PORT`.

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

   PriceAlertBot needs an identity of its own, eg. `price_alert_bot`, which the service takes as
   `ALERT_BOT_IDENTITY_PEM`. Without one, PriceAlertBot is off.

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
   Give it the avatar [`docs/avatar.png`](./docs/avatar.png), drawn from
   [`docs/avatar.svg`](./docs/avatar.svg) in the charts' colours.
   - Register PriceAlertBot the same way, with its own identity's principal, and an endpoint which
     maps to the service's `/alerts`.

4. **Install it** in a group, community or direct chat, granting it the permission to send text
   and image messages.
   - Install PriceAlertBot in a group or community, granting it the permission to send text
     messages.

### Locally

With OpenChat running locally from a checkout of
[open-chat](https://github.com/open-chat-labs/open-chat) beside this one (see its README), run:

```bash
./scripts/run_local.sh
```

It makes `chart_bot` and `price_alert_bot` identities if there aren't any, and prints each bot's
principal and endpoint to register it with: ChartBot's on port 3002, and PriceAlertBot's on 3003. It
reads the local StorageIndex's id from the `open-chat` checkout, or from the checkout
`OPEN_CHAT_DIR` names. The alerts are kept in `target/local-price-alerts.json`.

To see an alert fire, set one, then run the script again with a token priced as another, eg.
`LEDGER_ALIASES=<local CHAT>=ryjl3-tyaaa-aaaaa-aaaba-cai` prices CHAT as ICP.

### On AWS

It runs beside the Discord bridge, on its EC2 instance `i-0198f9d466e519454`, behind a CloudFront
distribution of its own. The instance is an arm64 t4g.small running Amazon Linux 2023 in us-east-1,
managed through SSM.
- **Size:** in a local run it used about 7 MB of memory and next to no CPU.
- **One copy only:** its prices are kept in memory, and a second copy would make CoinGecko calls of
  its own, beyond the free key's.
- **Its own distribution:** OpenChat reads a bot's definition from its endpoint's root, so ChartBot
  can't share a distribution with the bridges.
- **Its url:** `https://d2r0806cxz8193.cloudfront.net`, the distribution `E2LYK4LO2EO26X`.
- **PriceAlertBot** has a distribution of its own, which maps its root to the service's `/alerts`:
  `https://d1dka25zgffloh.cloudfront.net`, the distribution `E1YB2RA3NUKRLO`.
- **The alerts** are kept on the instance, in `/var/lib/oc-price-service`. They outlast restarts
  and deploys, but not replacing the instance, unless that directory's copied over.

**To set it up**, once:

1. **An ECR repository,** `oc-price-service`.
2. **The config,** in SSM Parameter Store, which the host reads each time the service starts:
   - `/oc-price-service/OC_IDENTITY_PEM`: a SecureString, the bot's identity, from
     `dfx identity export chart_bot`.
   - `/oc-price-service/COINGECKO_API_KEY`: a SecureString, a CoinGecko Demo key.
   - `/oc-price-service/PUBLIC_URL`: a String, the CloudFront distribution's url, which the deploy
     checks.
   - `/oc-price-service/ALERT_BOT_IDENTITY_PEM`: a SecureString, PriceAlertBot's identity, from
     `dfx identity export price_alert_bot`. Without it, PriceAlertBot is off.
   - Any other variable under [Configuration](#configuration) can be added the same way.

   [`scripts/set_aws_params.sh`](./scripts/set_aws_params.sh) sets them. It takes the identities
   from dfx, leaving PriceAlertBot's out if there's no `price_alert_bot` identity, and asks for the
   key. Pass it the url once the distribution exists:

   ```bash
   ./scripts/set_aws_params.sh https://<distribution>.cloudfront.net
   ```
3. **A CloudWatch log group,** `/oc/price-service`.
4. **The instance's role** also needs:
   - `ssm:GetParametersByPath` on `parameter/oc-price-service`, and `ssm:GetParameter` on
     `parameter/oc-price-service/*`. Parameters encrypted with a key of your own need
     `kms:Decrypt` on it too.
   - To pull from the `oc-price-service` ECR repository: `ecr:BatchGetImage` and
     `ecr:GetDownloadUrlForLayer`.
   - `logs:CreateLogStream` and `logs:PutLogEvents` on `/oc/price-service`.
5. **Port 8082** in the instance's security group, admitted only from CloudFront's origin-facing
   prefix list, `com.amazonaws.global.cloudfront.origin-facing`. 8080 and 8081 are the bridges'.
   - One rule admits all three ports. The prefix list counts as about 55 rules against the group's
     limit of 60, so a second rule using it wouldn't fit.
6. **The service,** a systemd unit and the script it runs before each start, from
   [`deploy/`](./deploy). Install them on the host, eg. through SSM Session Manager:

   ```bash
   install -m 0755 oc-price-service-prepare /usr/local/bin/
   install -m 0644 oc-price-service.service /etc/systemd/system/
   systemctl daemon-reload && systemctl enable oc-price-service
   ```

   It starts on the first deploy, once there's an image. Each time it starts, it:
   - pulls `oc-price-service:latest`;
   - writes the config to `/run/oc-price-service`;
   - runs the container on port 8082, as `nobody`, with its logs going to CloudWatch, and
     `/var/lib/oc-price-service` mounted for the alerts.

   Install them again whenever they change, eg. PriceAlertBot needed both changed.

   If it stops, it's restarted after 30 seconds. After 5 starts in an hour it's left stopped, as each
   start calls CoinGecko.
7. **A CloudFront distribution:**
   - **Origin:** the instance's public DNS name, over HTTP on port 8082. Give it a 60-second
     response timeout, as a chart on a cold cache can take that long.
   - **`/v1/*`, the website's prices:** GET and HEAD, with a cache policy of its own, which:
     - takes its TTLs from the service's `Cache-Control`: minimum 0, default 60 and maximum 300
       seconds;
     - includes the query string `range` in the cache key. `CachingOptimized` leaves query strings
       out, which would mix up the 1d and 7d histories;
     - enables gzip and Brotli.
   - **The default behaviour, for ChartBot:** all methods, with the `CachingDisabled` cache policy
     and the `AllViewerExceptHostHeader` origin request policy, so OpenChat's requests reach the bot
     as they were sent.
   - **Errors** are sent with `Cache-Control: no-store`. So that CloudFront can't hold on to a 503
     for its default 10 seconds, give 503 an error caching minimum TTL of 0.
   - **Viewers:** HTTPS only, as for the bridges.
   - **Edge locations:** all of them, as the website's users are worldwide. The bridges' use only
     North America's and Europe's.
   - **PriceAlertBot's distribution** is the same, with the same origin given the origin path
     `/alerts`, and only the default behaviour, ChartBot's.
8. **The deploy role,** `GitHubActions-DeployPriceService`, which GitHub can assume only from `main`:
   - **Trust:** the OIDC provider `token.actions.githubusercontent.com`, with `aud`
     `sts.amazonaws.com` and `sub`
     `repo:open-chat-labs@89913586/price-service@1433257699:ref:refs/heads/main`.
     - The repository uses GitHub's immutable subjects, which name the organisation and the
       repository with their ids, unlike open-chat-bots'. So a repository given the same name later
       couldn't deploy.
   - **Permissions:**
     - pushing to the `oc-price-service` ECR repository, plus `ecr:GetAuthorizationToken`;
     - `ssm:SendCommand` on the instance with the `AWS-RunShellScript` document, and
       `ssm:GetCommandInvocation`;
     - `ssm:GetParameter` on `/oc-price-service/PUBLIC_URL`.
   - `AWS-RunShellScript` runs any command on the host as root, so this role can do anything there,
     as the bridges' deploy roles can.
9. **Deploy it,** as below.
10. **Register ChartBot** on OpenChat, with the distribution's url as its endpoint. Then build the
    website with `OC_PRICE_SERVICE_URL` set to the same url.
11. **Register PriceAlertBot**, with its distribution's url as its endpoint.

**Deploys** are done by the *Deploy* GitHub Actions workflow,
[`deploy.yaml`](./.github/workflows/deploy.yaml), run by hand from `main`:
- It builds the image on an arm64 runner, for the arm64 host, and pushes it to ECR, tagged `latest`
  and with the commit.
- It restarts the service through SSM. The config in Parameter Store stays as it is.
- The deploy fails if the service doesn't start, doesn't stay up for 30 seconds, or doesn't serve
  prices. It then checks ChartBot's definition through the distribution.
- A pull request that changes the Dockerfile, the toolchain, the dependencies or the workflow only
  builds the image.

## Configuration

| Variable | |
|---|---|
| `OC_IDENTITY_PEM` or `OC_IDENTITY_PEM_FILE` | ChartBot's identity, as PEM text, eg. from `dfx identity export`, or a file |
| `ALERT_BOT_IDENTITY_PEM` or `ALERT_BOT_IDENTITY_PEM_FILE` | PriceAlertBot's identity, the same way. Without one, or with an empty file, PriceAlertBot is off |
| `ALERTS_FILE` | Where PriceAlertBot keeps its alerts. Defaults to `price-alerts.json` |
| `COINGECKO_API_KEY` | Optional, but worth having in production. A CoinGecko Demo API key, which is free, for a higher rate limit than its public API's. The coins' refresh uses about 7,200 of its 10,000 calls a month. Its prices must be credited where they're shown, which the API's `sources` says. |
| `PORT` | Defaults to 3000, or 8080 in the container |
| `OC_IC_URL`, `OC_PUBLIC_KEY`, `OC_STORAGE_INDEX` | Only to use an OpenChat other than mainnet's |
| `LEDGER_ALIASES` | Only locally: `local=mainnet` ledger pairs, separated by commas, to price local ledgers as mainnet ones |
| `ALERT_BOT_PORT` | Only locally: a port to serve PriceAlertBot on as well, at its root, as an origin of its own |
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

The Rust SDK is a git dependency on [open-chat-bots](https://github.com/open-chat-labs/open-chat-bots)'
`main`, as it isn't on crates.io. `Cargo.lock` pins its commit. To move to the latest:

```bash
cargo update -p oc_bots_sdk -p oc_bots_sdk_offchain
```

`Cargo.lock` was seeded from open-chat-bots' lockfile, which pins `dataurl` 0.1.2, a version the SDK
needs and crates.io has yanked, so keep it.

## License

AGPL-3.0, as OpenChat's other repositories.
