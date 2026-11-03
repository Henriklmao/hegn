//! Small numeric helpers.

/// Cycle through `values`, wrapping at both ends.
///
/// Falls back to the first value when `current` is not in the list (for example
/// after a configuration changed under an open form).
pub fn cycle_value<T: Clone + PartialEq>(values: &[T], current: &T, delta: isize) -> T {
    if values.is_empty() {
        return current.clone();
    }
    let index = values
        .iter()
        .position(|value| value == current)
        .unwrap_or(0);
    let next = (index as isize + delta).rem_euclid(values.len() as isize) as usize;
    values[next].clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_value_wraps_and_falls_back_to_the_first_entry() {
        let values = ["a", "b", "c"];
        assert_eq!(cycle_value(&values, &"a", 1), "b");
        assert_eq!(cycle_value(&values, &"a", -1), "c");
        assert_eq!(cycle_value(&values, &"missing", 1), "b");
        assert_eq!(cycle_value(&values, &"b", 7), "c");
        assert_eq!(cycle_value::<&str>(&[], &"x", 1), "x");
    }
}
