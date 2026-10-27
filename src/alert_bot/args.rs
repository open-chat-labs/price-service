use super::store::Direction;

const MAX_SYMBOL_LENGTH: usize = 20;

/// What `/alert` was asked for, eg. `CHAT above 0.05`. OpenChat passes everything typed after
/// `/alert` as one string, as it does for a command with a single string parameter.
#[derive(Debug, PartialEq)]
pub struct AlertArgs {
    pub symbol: String,
    pub direction: Direction,
    /// In USD
    pub target: f64,
}

pub fn usage() -> String {
    "Give a token, above or below, and a price in USD, eg. /alert CHAT above 0.05".to_string()
}

pub fn parse(text: &str) -> Result<AlertArgs, String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let [symbol, direction, price] = words[..] else {
        return Err(usage());
    };

    // People often write $CHAT
    let symbol = symbol.strip_prefix('$').unwrap_or(symbol);
    if symbol.is_empty()
        || symbol.len() > MAX_SYMBOL_LENGTH
        || !symbol
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        return Err(format!("\"{symbol}\" isn't a token symbol."));
    }

    let direction = match direction.to_ascii_lowercase().as_str() {
        "above" | "over" | ">" | ">=" => Direction::Above,
        "below" | "under" | "<" | "<=" => Direction::Below,
        _ => return Err(usage()),
    };

    // A plain number: with commas, 0,05 could be 0.05 or 5, depending on where someone's from
    let target = price
        .strip_prefix('$')
        .unwrap_or(price)
        .parse::<f64>()
        .ok()
        .filter(|p| p.is_finite() && *p > 0.0)
        .ok_or_else(|| format!("\"{price}\" isn't a price. Give one in USD, eg. 0.05"))?;

    Ok(AlertArgs {
        symbol: symbol.to_string(),
        direction,
        target,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(symbol: &str, direction: Direction, target: f64) -> AlertArgs {
        AlertArgs {
            symbol: symbol.to_string(),
            direction,
            target,
        }
    }

    #[test]
    fn an_alert_is_a_token_above_or_below_a_price() {
        assert_eq!(
            parse("CHAT above 0.05"),
            Ok(args("CHAT", Direction::Above, 0.05))
        );
        assert_eq!(
            parse("  $icp  below  $5 "),
            Ok(args("icp", Direction::Below, 5.0))
        );
        assert_eq!(
            parse("ckBTC > 100000"),
            Ok(args("ckBTC", Direction::Above, 100_000.0))
        );
        assert_eq!(
            parse("ADA under .2"),
            Ok(args("ADA", Direction::Below, 0.2))
        );
    }

    #[test]
    fn anything_else_is_explained() {
        assert_eq!(parse(""), Err(usage()));
        assert_eq!(parse("CHAT 0.05"), Err(usage()));
        assert_eq!(parse("CHAT near 0.05"), Err(usage()));
        assert_eq!(parse("CHAT above 0.05 now"), Err(usage()));
        assert!(
            parse("CH@T above 0.05")
                .unwrap_err()
                .contains("isn't a token symbol")
        );
        assert!(
            parse("CHAT above 0,05")
                .unwrap_err()
                .contains("isn't a price")
        );
        assert!(parse("CHAT above 0").unwrap_err().contains("isn't a price"));
        assert!(
            parse("CHAT above inf")
                .unwrap_err()
                .contains("isn't a price")
        );
        assert!(
            parse("CHAT above -1")
                .unwrap_err()
                .contains("isn't a price")
        );
    }
}
