use super::PricePoint;

// Each point is compared with this many points either side of it
const WINDOW: usize = 3;
// A point this many times above or below its neighbours' median is an outlier
const MAX_RATIO: f64 = 3.0;

/// Drops the points far from those either side of them, eg. a single trade at a silly price, which
/// the next trades undo. The first and last points are kept, as without points on both sides there's
/// no telling a spike from a real move.
pub fn filter(points: Vec<PricePoint>) -> Vec<PricePoint> {
    let prices: Vec<f64> = points.iter().map(|p| p.price).collect();
    let len = prices.len();

    points
        .into_iter()
        .enumerate()
        .filter(|(i, point)| {
            let before = &prices[i.saturating_sub(WINDOW)..*i];
            let after = &prices[(i + 1).min(len)..(i + 1 + WINDOW).min(len)];
            if before.is_empty() || after.is_empty() {
                return true;
            }
            let mut neighbours: Vec<f64> = before.iter().chain(after).copied().collect();
            let ratio = point.price / median(&mut neighbours);
            (1.0 / MAX_RATIO..=MAX_RATIO).contains(&ratio)
        })
        .map(|(_, point)| point)
        .collect()
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn points(prices: &[f64]) -> Vec<PricePoint> {
        prices
            .iter()
            .enumerate()
            .map(|(i, price)| PricePoint {
                time: i as u64,
                price: *price,
            })
            .collect()
    }

    fn prices(points: &[PricePoint]) -> Vec<f64> {
        points.iter().map(|p| p.price).collect()
    }

    #[test]
    fn a_spike_which_reverts_is_dropped() {
        let filtered = filter(points(&[0.028, 0.029, 0.028, 0.68, 0.028, 0.027, 0.028]));
        assert_eq!(
            prices(&filtered),
            [0.028, 0.029, 0.028, 0.028, 0.027, 0.028]
        );
    }

    #[test]
    fn a_dip_which_reverts_is_dropped() {
        let filtered = filter(points(&[1.0, 1.1, 0.9, 0.01, 1.0, 1.0, 1.0]));
        assert_eq!(prices(&filtered), [1.0, 1.1, 0.9, 1.0, 1.0, 1.0]);
    }

    #[test]
    fn ordinary_moves_are_kept() {
        let series = [1.0, 1.5, 2.2, 2.0, 2.9, 2.5, 1.2, 1.4];
        assert_eq!(prices(&filter(points(&series))), series);
    }

    #[test]
    fn a_lasting_move_is_kept_after_at_most_its_first_point() {
        let filtered = filter(points(&[10.0, 10.0, 10.0, 10.0, 1.0, 1.0, 1.0, 1.0]));
        assert_eq!(prices(&filtered), [10.0, 10.0, 10.0, 10.0, 1.0, 1.0, 1.0]);
    }

    #[test]
    fn the_first_and_last_points_are_kept() {
        let series = [100.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 100.0];
        assert_eq!(prices(&filter(points(&series))), series);
        assert!(filter(Vec::new()).is_empty());
    }
}
