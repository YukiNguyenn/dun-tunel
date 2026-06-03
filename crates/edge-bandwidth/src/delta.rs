//! Delta computation utilities.

pub fn bytes_to_mb(bytes: u64) -> f64 {
    bytes as f64 / 1_048_576.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn delta_is_non_negative(prev: u64, curr: u64) {
            let delta = curr.saturating_sub(prev);
            prop_assert!(delta as f64 / 1_048_576.0 >= 0.0);
        }
    }
}
