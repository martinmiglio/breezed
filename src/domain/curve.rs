//! Fan curve model: validated points and linear interpolation.

use crate::domain::types::{DomainError, FanPercent, TempC};

/// One point on the fan curve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CurvePoint {
    /// Temperature at which this point applies.
    pub temp_c: TempC,
    /// Fan duty cycle at this temperature.
    pub fan_pct: FanPercent,
}

/// A curve known to be non-empty and strictly ascending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Curve(Vec<CurvePoint>);

impl Curve {
    /// Validate and normalize curve points.
    ///
    /// # Errors
    ///
    /// Rejects an empty curve or adjacent points that are not strictly
    /// ascending.
    pub fn new(points: impl IntoIterator<Item = CurvePoint>) -> Result<Self, DomainError> {
        let points: Vec<CurvePoint> = points.into_iter().collect();
        validate_curve(&points)?;
        Ok(Self(points))
    }

    /// The validated points, in ascending temperature order.
    pub fn points(&self) -> &[CurvePoint] {
        &self.0
    }

    /// Number of points.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// A curve always has at least one point.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Validate that `points` is non-empty and strictly ascending by temperature.
///
/// # Errors
///
/// Returns [`DomainError`] describing the first violation.
fn validate_curve(points: &[CurvePoint]) -> Result<(), DomainError> {
    if points.is_empty() {
        return Err(DomainError::new("curve must contain at least one point"));
    }
    for (i, pair) in points.windows(2).enumerate() {
        let (a, b) = (pair[0], pair[1]);
        if a.temp_c >= b.temp_c {
            return Err(DomainError::new(format!(
                "curve temp_c must be strictly ascending: \
                 points[{i}].temp_c={} >= points[{}].temp_c={}",
                a.temp_c.0,
                i + 1,
                b.temp_c.0
            )));
        }
    }
    Ok(())
}

/// Map a temperature to a fan target.
///
/// At or above the top point returns `Ok(None)` (the AUTO signal); below the
/// first point clamps to the first fan percentage. Otherwise linear
/// interpolation, rounded with ties-to-even.
///
/// # Errors
///
/// Returns [`DomainError`] for an empty or non-ascending curve.
#[allow(clippy::cast_possible_truncation)]
pub fn interpolate(curve: &[CurvePoint], temp_c: TempC) -> Result<Option<i32>, DomainError> {
    let Some(last) = curve.last() else {
        return Err(DomainError::new("curve must contain at least one point"));
    };
    let first = curve[0];
    if temp_c >= last.temp_c {
        return Ok(None);
    }
    if temp_c <= first.temp_c {
        return Ok(Some(first.fan_pct.get()));
    }
    let index = curve.partition_point(|point| point.temp_c < temp_c);
    if index == 0 || index == curve.len() {
        return Err(DomainError::new("curve not strictly ascending"));
    }
    let a = curve[index - 1];
    let b = curve[index];
    if a.temp_c >= b.temp_c {
        return Err(DomainError::new("curve not strictly ascending"));
    }
    let span = f64::from(b.temp_c.0 - a.temp_c.0);
    let frac = f64::from(temp_c.0 - a.temp_c.0) / span;
    let value = f64::from(a.fan_pct.get()) + f64::from(b.fan_pct.get() - a.fan_pct.get()) * frac;
    let rounded = value.round_ties_even();
    Ok(Some(rounded as i32))
}

/// Default speed policy: [`interpolate`].
#[derive(Debug, Clone, Copy, Default)]
pub struct CurvePolicy;

impl crate::domain::ports::SpeedPolicy for CurvePolicy {
    fn target(&self, curve: &[CurvePoint], temp_c: TempC) -> Result<Option<i32>, DomainError> {
        interpolate(curve, temp_c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn curve(points: &[(i32, i32)]) -> Vec<CurvePoint> {
        points
            .iter()
            .map(|&(temp_c, fan_pct)| CurvePoint {
                temp_c: TempC(temp_c),
                fan_pct: FanPercent::from_validated(fan_pct),
            })
            .collect()
    }

    fn default_curve() -> Vec<CurvePoint> {
        curve(&[(45, 6), (60, 8), (68, 12), (74, 18)])
    }

    #[test]
    fn below_first_point_clamps() {
        let validated = Curve::new(default_curve()).unwrap();
        assert_eq!(interpolate(validated.points(), TempC(30)), Ok(Some(6)));
    }

    #[test]
    fn exactly_on_point_returns_that_points_pct() {
        let validated = Curve::new(default_curve()).unwrap();
        assert_eq!(interpolate(validated.points(), TempC(60)), Ok(Some(8)));
        assert_eq!(interpolate(validated.points(), TempC(45)), Ok(Some(6)));
    }

    #[test]
    fn interpolates_linearly() {
        let validated = Curve::new(default_curve()).unwrap();
        assert_eq!(interpolate(validated.points(), TempC(52)), Ok(Some(7)));
        assert_eq!(interpolate(validated.points(), TempC(64)), Ok(Some(10)));
    }

    #[test]
    fn half_step_rounds_to_even() {
        let validated = Curve::new(curve(&[(0, 7), (10, 8)])).unwrap();
        // 7.5 rounds to the even neighbour, 8.
        assert_eq!(interpolate(validated.points(), TempC(5)), Ok(Some(8)));
        let even_down = Curve::new(curve(&[(0, 8), (10, 9)])).unwrap();
        // 8.5 rounds to the even neighbour, 8.
        assert_eq!(interpolate(even_down.points(), TempC(5)), Ok(Some(8)));
    }

    #[test]
    fn at_or_above_top_returns_none() {
        let validated = Curve::new(default_curve()).unwrap();
        assert_eq!(interpolate(validated.points(), TempC(74)), Ok(None));
        assert_eq!(interpolate(validated.points(), TempC(80)), Ok(None));
    }

    #[test]
    fn empty_curve_raises() {
        let error = interpolate(&[], TempC(50)).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("curve must contain at least one point")
        );
    }

    #[test]
    fn single_point_curve() {
        let validated = Curve::new(curve(&[(50, 10)])).unwrap();
        assert_eq!(interpolate(validated.points(), TempC(40)), Ok(Some(10)));
        assert_eq!(interpolate(validated.points(), TempC(50)), Ok(None));
        assert_eq!(interpolate(validated.points(), TempC(60)), Ok(None));
    }

    #[test]
    fn non_monotonic_curve_rejected() {
        let descending = curve(&[(45, 6), (60, 8), (55, 12)]);
        assert!(
            validate_curve(&descending)
                .unwrap_err()
                .to_string()
                .contains("strictly ascending")
        );
        let equal_adjacent = curve(&[(45, 6), (60, 8), (60, 12)]);
        assert!(
            validate_curve(&equal_adjacent)
                .unwrap_err()
                .to_string()
                .contains("strictly ascending")
        );
    }

    #[test]
    fn equal_adjacent_reports_both_indices() {
        let error = validate_curve(&curve(&[(45, 6), (60, 8), (60, 12)])).unwrap_err();
        assert_eq!(
            error.to_string(),
            "curve temp_c must be strictly ascending: \
             points[1].temp_c=60 >= points[2].temp_c=60"
        );
    }

    #[test]
    fn curve_new_preserves_the_input_points() {
        let source = curve(&[(45, 6), (60, 8)]);
        let result = Curve::new(source.clone()).unwrap();
        assert_eq!(result.points(), source);
        assert_eq!(result.len(), 2);
        assert!(!result.is_empty());
    }
}
