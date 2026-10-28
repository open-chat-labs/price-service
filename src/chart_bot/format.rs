/// A USD price to 4 significant figures, or to the cent from $1, eg. $0.002345, $5.12, $67,123
pub fn usd(price: f64) -> String {
    if price >= 1000.0 {
        format!("${}", thousands(price.round() as u64))
    } else if price >= 1.0 {
        format!("${price:.2}")
    } else if price > 0.0 {
        format!("${:.*}", decimals_for_significant_figures(price, 4), price)
    } else {
        "$0".to_string()
    }
}

fn decimals_for_significant_figures(value: f64, figures: i32) -> usize {
    (figures - 1 - value.abs().log10().floor() as i32).clamp(0, 12) as usize
}

pub fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut result = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            result.push(',');
        }
        result.push(c);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices() {
        assert_eq!(usd(67123.4), "$67,123");
        assert_eq!(usd(1234567.0), "$1,234,567");
        assert_eq!(usd(5.123), "$5.12");
        assert_eq!(usd(1.0), "$1.00");
        assert_eq!(usd(0.23456), "$0.2346");
        assert_eq!(usd(0.002345), "$0.002345");
        assert_eq!(usd(0.00000123456), "$0.000001235");
        assert_eq!(usd(0.0), "$0");
    }
}
