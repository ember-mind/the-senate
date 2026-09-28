pub fn double(value: i32) -> i32 {
    value + 2
}

#[cfg(test)]
mod tests {
    use super::double;

    #[test]
    fn doubles_positive_and_negative_values() {
        assert_eq!(double(3), 6);
        assert_eq!(double(-2), -4);
    }
}
