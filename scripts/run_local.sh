#!/bin/bash

# Runs the price service as ChartBot against OpenChat running locally, from a checkout of
# open-chat beside this one, or the one OPEN_CHAT_DIR names

SCRIPT=$(readlink -f "$0")
SCRIPT_DIR=$(dirname "$SCRIPT")
cd $SCRIPT_DIR/..

IDENTITY=${1:-chart_bot} # The identity to use for the bot
PORT=${2:-3002} # The port the bot will listen on
OPEN_CHAT_DIR=${OPEN_CHAT_DIR:-../open-chat}

# The local IC URL
IC_URL=http://127.0.0.1:8080

# Read the OpenChat public key from the website
OC_PUBLIC_KEY=$(curl -s http://localhost:5001/public-key) || exit 1

# The local StorageIndex, which the charts are uploaded to
STORAGE_INDEX=$(cd $OPEN_CHAT_DIR && dfx canister id storage_index) || exit 1

# Get the principal for the bot identity, creating it if it does not exist
PRINCIPAL=$(dfx identity --identity $IDENTITY get-principal 2> /dev/null)
if [ $? -ne 0 ]; then
    dfx identity new $IDENTITY --storage-mode=plaintext || exit 1
    PRINCIPAL=$(dfx identity --identity $IDENTITY get-principal) || exit 1
fi

# Build the service
cargo build --release || exit 1

echo ""
echo "Name: ChartBot"
echo "Principal: $PRINCIPAL"
echo "Endpoint: http://localhost:$PORT"
echo ""

# Run the service - the process won't exit until it's stopped
OC_IDENTITY_PEM="$(dfx identity export $IDENTITY)" \
OC_IC_URL=$IC_URL \
OC_PUBLIC_KEY="$OC_PUBLIC_KEY" \
OC_STORAGE_INDEX=$STORAGE_INDEX \
PORT=$PORT \
cargo run --release
