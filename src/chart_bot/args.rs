use crate::prices::Period;

/// More lines than this are hard to tell apart
pub const MAX_TOKENS: usize = 5;
const MAX_SYMBOL_LENGTH: usize = 20;

/// What `/chart` was asked for. OpenChat passes everything typed after `/chart` as one string, as
/// it only does that for a command with a single string parameter, eg. `CHAT ICP ckBTC 30d`.
#[derive(Debug, PartialEq)]
pub struct ChartArgs {
    pub symbols: Vec<String>,
    pub period: Period,
}

const DEFAULT_PERIOD: Period = Period::Week;

fn parse_period(text: &str) -> Option<Period> {
    match text.to_ascii_lowercase().as_str() {
        "1d" | "24h" | "day" => Some(Period::Day),
        "7d" | "1w" | "week" => Some(Period::Week),
        "30d" | "1m" | "month" => Some(Period::Month),
        "90d" | "3m" => Some(Period::Quarter),
        "1y" | "12m" | "365d" | "year" => Some(Period::Year),
        _ => None,
    }
}

fn period_from_days(days: &str) -> Result<Period, String> {
    match days.parse::<u32>() {
        Ok(1) => Ok(Period::Day),
        Ok(7) => Ok(Period::Week),
        Ok(30) => Ok(Period::Month),
        Ok(90) => Ok(Period::Quarter),
        Ok(365) => Ok(Period::Year),
        _ => Err(format!(
            "Charts cover 1, 7, 30, 90 or 365 days, not {days}."
        )),
    }
}

pub fn parse(text: &str) -> Result<ChartArgs, String> {
    let mut words: Vec<&str> = text
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|w| !w.is_empty())
        .collect();
    let mut symbols: Vec<String> = Vec::new();
    let mut period = None;

    // A number after the tokens is a number of days, eg. `/chart ICP 7`. Anywhere else it's a
    // token, eg. `/chart 7`
    if let [_, .., last] = words[..]
        && last.chars().all(|c| c.is_ascii_digit())
    {
        period = Some(period_from_days(last)?);
        words.pop();
    }

    for word in words {
        if let Some(p) = parse_period(word) {
            if period.replace(p).is_some_and(|previous| previous != p) {
                return Err("Give at most one period, eg. 30d.".to_string());
            }
            continue;
        }

        // People often write $CHAT
        let symbol = word.strip_prefix('$').unwrap_or(word);
        if symbol.is_empty()
            || symbol.len() > MAX_SYMBOL_LENGTH
            || !symbol
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        {
            return Err(format!("\"{word}\" isn't a token symbol."));
        }
        if !symbols.iter().any(|s| s.eq_ignore_ascii_case(symbol)) {
            symbols.push(symbol.to_string());
        }
    }

    if symbols.is_empty() {
        return Err(usage());
    }
    if symbols.len() > MAX_TOKENS {
        return Err(format!("Chart at most {MAX_TOKENS} tokens at once."));
    }

    Ok(ChartArgs {
        symbols,
        period: period.unwrap_or(DEFAULT_PERIOD),
    })
}

pub fn usage() -> String {
    "Give one or more token symbols, then optionally a period (1d, 7d, 30d, 90d or 1y), \
    eg. `/chart CHAT ICP 30d`."
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn symbols(args: &ChartArgs) -> Vec<&str> {
        args.symbols.iter().map(|s| s.as_str()).collect()
    }

    #[test]
    fn one_token_defaults_to_a_week() {
        let args = parse("CHAT").unwrap();
        assert_eq!(symbols(&args), ["CHAT"]);
        assert_eq!(args.period, Period::Week);
    }

    #[test]
    fn several_tokens_and_a_period() {
        let args = parse("CHAT ICP ckBTC 30d").unwrap();
        assert_eq!(symbols(&args), ["CHAT", "ICP", "ckBTC"]);
        assert_eq!(args.period, Period::Month);
    }

    #[test]
    fn the_period_can_go_anywhere() {
        assert_eq!(parse("1y CHAT").unwrap().period, Period::Year);
        assert_eq!(parse("CHAT 1D ICP").unwrap().period, Period::Day);
    }

    #[test]
    fn periods_have_aliases() {
        for (text, period) in [
            ("24h", Period::Day),
            ("1w", Period::Week),
            ("1m", Period::Month),
            ("3m", Period::Quarter),
            ("90d", Period::Quarter),
            ("12m", Period::Year),
            ("365d", Period::Year),
        ] {
            assert_eq!(
                parse(&format!("ICP {text}")).unwrap().period,
                period,
                "{text}"
            );
        }
    }

    #[test]
    fn a_number_at_the_end_is_a_number_of_days() {
        for (text, period) in [
            ("1", Period::Day),
            ("7", Period::Week),
            ("30", Period::Month),
            ("90", Period::Quarter),
            ("365", Period::Year),
        ] {
            let args = parse(&format!("CHAT ICP {text}")).unwrap();
            assert_eq!(symbols(&args), ["CHAT", "ICP"], "{text}");
            assert_eq!(args.period, period, "{text}");
        }
    }

    #[test]
    fn a_number_of_days_we_dont_chart_is_an_error() {
        assert_eq!(
            parse("ICP 14"),
            Err("Charts cover 1, 7, 30, 90 or 365 days, not 14.".to_string())
        );
        assert!(parse("ICP 7d 30").is_err());
    }

    #[test]
    fn a_number_anywhere_else_is_a_token() {
        assert_eq!(symbols(&parse("7").unwrap()), ["7"]);
        let args = parse("7 ICP").unwrap();
        assert_eq!(symbols(&args), ["7", "ICP"]);
        assert_eq!(args.period, Period::Week);
        let args = parse("ICP $7").unwrap();
        assert_eq!(symbols(&args), ["ICP", "7"]);
        assert_eq!(args.period, Period::Week);
    }

    #[test]
    fn commas_dollar_signs_and_extra_spaces_are_ignored() {
        let args = parse("  $CHAT,ICP ,  ckBTC  ").unwrap();
        assert_eq!(symbols(&args), ["CHAT", "ICP", "ckBTC"]);
    }

    #[test]
    fn repeated_tokens_are_charted_once() {
        let args = parse("CHAT chat ICP CHAT").unwrap();
        assert_eq!(symbols(&args), ["CHAT", "ICP"]);
    }

    #[test]
    fn the_same_period_twice_is_fine_but_two_periods_are_not() {
        assert_eq!(parse("CHAT 7d 1w").unwrap().period, Period::Week);
        assert!(parse("CHAT 7d 30d").is_err());
    }

    #[test]
    fn at_least_one_token_is_needed() {
        assert_eq!(parse(""), Err(usage()));
        assert_eq!(parse("30d"), Err(usage()));
        assert_eq!(parse(" , "), Err(usage()));
    }

    #[test]
    fn at_most_five_tokens() {
        assert!(parse("A B C D E").is_ok());
        assert!(parse("A B C D E F").is_err());
    }

    #[test]
    fn symbols_are_checked() {
        assert!(parse("CH@T").is_err());
        assert!(parse("$").is_err());
        assert!(parse("ABCDEFGHIJKLMNOPQRSTU").is_err());
        assert!(parse("ck.BTC wstETH BTC-B").is_ok());
    }
}
