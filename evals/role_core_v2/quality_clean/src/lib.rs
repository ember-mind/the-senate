pub fn classify(value: Option<i32>) -> &'static str {
    match value {
        None => "missing",
        Some(value) if value > 10 => "large",
        Some(value) if value > 0 => "positive",
        Some(_) => "non-positive",
    }
}

#[cfg(test)]
mod tests {
    use super::classify;

    #[test]
    fn covers_each_category() {
        assert_eq!(classify(None), "missing");
        assert_eq!(classify(Some(12)), "large");
        assert_eq!(classify(Some(2)), "positive");
        assert_eq!(classify(Some(0)), "non-positive");
    }
}
