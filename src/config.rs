use anyhow::{Context, Result, anyhow, bail};
use oc_bots_sdk::mainnet::{
    mainnet_ic_url, mainnet_oc_public_key, mainnet_storage_index_canister_id,
};
use oc_bots_sdk::types::CanisterId;
use std::collections::HashMap;
use std::env;
use std::path::PathBuf;

/// From environment variables
pub struct Config {
    /// The PEM of the bot's identity, secp256k1 or Ed25519
    pub identity_pem: String,
    pub ic_url: String,
    /// OpenChat's key, which signs its commands
    pub oc_public_key: String,
    /// Where the charts are uploaded
    pub storage_index: CanisterId,
    /// A CoinGecko API key, for its higher rate limit. Without one, its public API is used.
    pub coingecko_api_key: Option<String>,
    /// Local ledgers to serve mainnet ledgers' prices for, so a local OpenChat shows real prices
    pub ledger_aliases: HashMap<CanisterId, CanisterId>,
    pub port: u16,
    /// PriceAlertBot's identity, as PEM text. Without one, PriceAlertBot is off.
    pub alert_bot_identity_pem: Option<String>,
    /// Where PriceAlertBot keeps its alerts, so they outlast restarts
    pub alerts_file: PathBuf,
    /// A port to serve PriceAlertBot on as well, at its root, eg. locally, where it needs an origin
    /// of its own to be registered
    pub alert_bot_port: Option<u16>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let Some(identity_pem) = pem("OC_IDENTITY_PEM", "OC_IDENTITY_PEM_FILE")? else {
            bail!("Set OC_IDENTITY_PEM or OC_IDENTITY_PEM_FILE");
        };
        let storage_index = match var("OC_STORAGE_INDEX") {
            Some(text) => canister_id(&text)?,
            None => mainnet_storage_index_canister_id(),
        };

        Ok(Config {
            identity_pem,
            ic_url: var("OC_IC_URL").unwrap_or_else(mainnet_ic_url),
            // Locally it's read from Candid's text, which writes its line breaks as \n
            oc_public_key: var("OC_PUBLIC_KEY")
                .map(|key| key.replace("\\n", "\n"))
                .unwrap_or_else(mainnet_oc_public_key),
            storage_index,
            coingecko_api_key: var("COINGECKO_API_KEY"),
            ledger_aliases: var("LEDGER_ALIASES")
                .map_or(Ok(HashMap::new()), |text| ledger_aliases(&text))?,
            port: var("PORT")
                .map_or(Ok(3000), |p| p.parse())
                .context("Invalid PORT")?,
            alert_bot_identity_pem: pem("ALERT_BOT_IDENTITY_PEM", "ALERT_BOT_IDENTITY_PEM_FILE")?,
            alerts_file: var("ALERTS_FILE")
                .unwrap_or_else(|| "price-alerts.json".to_string())
                .into(),
            alert_bot_port: var("ALERT_BOT_PORT")
                .map(|p| p.parse())
                .transpose()
                .context("Invalid ALERT_BOT_PORT")?,
        })
    }
}

// A PEM given as text, or in a file
fn pem(text_var: &str, file_var: &str) -> Result<Option<String>> {
    match (var(text_var), var(file_var)) {
        (Some(pem), _) => Ok(Some(pem)),
        (None, Some(file)) => std::fs::read_to_string(&file)
            .map(Some)
            .with_context(|| format!("Failed to read {file_var} {file}")),
        (None, None) => Ok(None),
    }
}

fn var(name: &str) -> Option<String> {
    env::var(name).ok().filter(|v| !v.trim().is_empty())
}

// eg. "local1=mainnet1,local2=mainnet2"
fn ledger_aliases(text: &str) -> Result<HashMap<CanisterId, CanisterId>> {
    text.split(',')
        .filter(|pair| !pair.trim().is_empty())
        .map(|pair| {
            let (local, mainnet) = pair.split_once('=').ok_or_else(|| {
                anyhow!("LEDGER_ALIASES should be local=mainnet pairs, not {pair}")
            })?;
            Ok((canister_id(local)?, canister_id(mainnet)?))
        })
        .collect()
}

fn canister_id(text: &str) -> Result<CanisterId> {
    ic_agent::export::Principal::from_text(text.trim())
        .map(CanisterId::from)
        .map_err(|e| anyhow!("Invalid canister id {text}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ledger_aliases_are_local_mainnet_pairs() {
        let aliases =
            ledger_aliases("ryjl3-tyaaa-aaaaa-aaaba-cai=2ouva-viaaa-aaaaq-aaamq-cai, ").unwrap();
        assert_eq!(
            aliases,
            HashMap::from([(
                canister_id("ryjl3-tyaaa-aaaaa-aaaba-cai").unwrap(),
                canister_id("2ouva-viaaa-aaaaq-aaamq-cai").unwrap()
            )])
        );
        assert!(ledger_aliases("ryjl3-tyaaa-aaaaa-aaaba-cai").is_err());
        assert!(ledger_aliases("a=b").is_err());
    }
}
