#!/bin/bash
# Sets the service's config in SSM Parameter Store, which the EC2 host reads each time the service
# starts. Run it with credentials for OpenChat's AWS account, eg. with AWS_PROFILE set:
#
#   ./scripts/set_aws_params.sh [public url]
#
# - The bots' identities come from dfx: ChartBot's is the identity IDENTITY names, chart_bot by
#   default, and PriceAlertBot's ALERT_IDENTITY's, price_alert_bot by default. PriceAlertBot's is
#   left out if there's no such identity, which leaves PriceAlertBot off.
# - It asks for the CoinGecko Demo key. Leave it blank to keep the one already set.
# - The public url is the CloudFront distribution's, which the deploy checks. Leave it out until the
#   distribution exists, then run this again with it.
#
# Running it again overwrites what it sets. The service picks up changes when it next starts, eg. on
# the next deploy.
set -euo pipefail

ACCOUNT=253040722768
REGION=us-east-1
PREFIX=/oc-price-service
IDENTITY=${IDENTITY:-chart_bot}
ALERT_IDENTITY=${ALERT_IDENTITY:-price_alert_bot}
PUBLIC_URL=${1:-}

account=$(aws sts get-caller-identity --query Account --output text)
if [ "$account" != "$ACCOUNT" ]; then
    echo "These AWS credentials are for the account $account, not OpenChat's, $ACCOUNT"
    exit 1
fi

# Values go through a file only this user can read, rather than on the command line
tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT
chmod 600 "$tmp"

# Sets the parameter to what's in the file
put() {
    local name=$1 type=$2
    local version
    version=$(aws ssm put-parameter --region "$REGION" --name "$PREFIX/$name" --type "$type" \
        --value "file://$tmp" --overwrite --query Version --output text)
    echo "Set $PREFIX/$name ($type), now at version $version"
}

dfx identity export "$IDENTITY" > "$tmp"
put OC_IDENTITY_PEM SecureString
echo "ChartBot's principal, to register it with: $(dfx identity get-principal --identity "$IDENTITY")"

if dfx identity get-principal --identity "$ALERT_IDENTITY" > /dev/null 2>&1; then
    dfx identity export "$ALERT_IDENTITY" > "$tmp"
    put ALERT_BOT_IDENTITY_PEM SecureString
    echo "PriceAlertBot's principal, to register it with: $(dfx identity get-principal --identity "$ALERT_IDENTITY")"
else
    echo "No dfx identity $ALERT_IDENTITY, so PriceAlertBot's is left as it is."
fi

read -rsp "CoinGecko Demo API key, or blank to keep the one already set: " key
echo
if [ -n "$key" ]; then
    printf '%s' "$key" > "$tmp"
    put COINGECKO_API_KEY SecureString
fi

if [ -n "$PUBLIC_URL" ]; then
    printf '%s' "${PUBLIC_URL%/}" > "$tmp"
    put PUBLIC_URL String
else
    echo "No public url given. Once the CloudFront distribution exists, run this again with its url."
fi
