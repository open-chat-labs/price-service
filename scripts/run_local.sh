#!/bin/bash

# Runs the price service as ChartBot and PriceAlertBot against OpenChat running locally, from a
# checkout of open-chat beside this one, or the one OPEN_CHAT_DIR names. PriceAlertBot is served on
# the next port, as OpenChat registers a bot only at an origin of its own.

SCRIPT=$(readlink -f "$0")
SCRIPT_DIR=$(dirname "$SCRIPT")
cd $SCRIPT_DIR/..

IDENTITY=${1:-chart_bot} # The identity to use for the bot
PORT=${2:-3002} # The port the bot will listen on
ALERT_IDENTITY=${ALERT_IDENTITY:-price_alert_bot} # PriceAlertBot's identity
ALERT_BOT_PORT=${ALERT_BOT_PORT:-$((PORT + 1))} # The port PriceAlertBot will listen on
OPEN_CHAT_DIR=${OPEN_CHAT_DIR:-../open-chat}

# The local IC URL
IC_URL=http://127.0.0.1:8080

# Read the OpenChat public key from the website
OC_PUBLIC_KEY=$(curl -s http://localhost:5001/public-key) || exit 1

# The local StorageIndex, which the charts are uploaded to
STORAGE_INDEX=$(cd $OPEN_CHAT_DIR && dfx canister id storage_index) || exit 1

# Get the principal for an identity, creating it if it does not exist
principal() {
    dfx identity --identity $1 get-principal 2> /dev/null && return
    dfx identity new $1 --storage-mode=plaintext > /dev/null || exit 1
    dfx identity --identity $1 get-principal
}
PRINCIPAL=$(principal $IDENTITY) || exit 1
ALERT_PRINCIPAL=$(principal $ALERT_IDENTITY) || exit 1

# Build the service
cargo build --release || exit 1

echo ""
echo "Name: ChartBot"
echo "Principal: $PRINCIPAL"
echo "Endpoint: http://localhost:$PORT"
echo ""
echo "Name: PriceAlertBot"
echo "Principal: $ALERT_PRINCIPAL"
echo "Endpoint: http://localhost:$ALERT_BOT_PORT"
echo ""

# Run the service - the process won't exit until it's stopped
OC_IDENTITY_PEM="$(dfx identity export $IDENTITY)" \
ALERT_BOT_IDENTITY_PEM="$(dfx identity export $ALERT_IDENTITY)" \
ALERT_BOT_PORT=$ALERT_BOT_PORT \
ALERTS_FILE=target/local-price-alerts.json \
OC_IC_URL=$IC_URL \
OC_PUBLIC_KEY="$OC_PUBLIC_KEY" \
OC_STORAGE_INDEX=$STORAGE_INDEX \
PORT=$PORT \
cargo run --release
