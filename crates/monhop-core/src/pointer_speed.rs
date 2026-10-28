//! The user's pointer speed on the computers this one controls: whole steps either side of Same,
//! each a fifth of a doubling, so the range runs from half to twice the speed.

pub const MIN_STEP: i8 = -5;
pub const MAX_STEP: i8 = 5;
/// Same: the pointer moves on another computer as it does on the one the mouse is plugged into.
pub const SAME: i8 = 0;

/// The motion multiplier for `step`, clamped into range.
pub fn factor(step: i8) -> f64 {
    2f64.powf(f64::from(step.clamp(MIN_STEP, MAX_STEP)) / 5.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_is_unchanged_and_the_ends_halve_and_double() {
        assert_eq!(factor(SAME), 1.0);
        assert!((factor(MAX_STEP) - 2.0).abs() < 1e-12);
        assert!((factor(MIN_STEP) - 0.5).abs() < 1e-12);
    }

    #[test]
    fn every_step_is_faster_than_the_one_below_and_out_of_range_clamps() {
        for step in MIN_STEP..MAX_STEP {
            assert!(factor(step) < factor(step + 1));
        }
        assert_eq!(factor(i8::MAX), factor(MAX_STEP));
        assert_eq!(factor(i8::MIN), factor(MIN_STEP));
    }
}
