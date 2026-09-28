pub struct FlagParser;

impl FlagParser {
    pub fn parse(&self, input: &str) -> bool {
        input.trim() == "enabled"
    }
}

pub fn feature_enabled(input: &str) -> bool {
    let parser = FlagParser;
    parser.parse(input)
}

pub struct UserName {
    pub raw: String,
    pub normalized: String,
}

impl UserName {
    pub fn new(value: &str) -> Self {
        Self {
            raw: value.to_owned(),
            normalized: value.trim().to_lowercase(),
        }
    }
}

pub fn classify(value: Option<i32>) -> &'static str {
    if value.is_some() {
        if value.unwrap_or_default() > 0 {
            if value.unwrap_or_default() > 10 {
                "large"
            } else {
                "positive"
            }
        } else {
            "non-positive"
        }
    } else {
        "missing"
    }
}

#[cfg(test)]
mod tests {
    use super::{classify, feature_enabled, UserName};

    #[test]
    fn feature_enabled_covers_true_and_false() {
        assert!(feature_enabled(" enabled "));
        assert!(!feature_enabled("disabled"));
    }

    #[test]
    fn username_normalizes_outer_whitespace_and_case() {
        let user = UserName::new("  Bob Example  ");
        assert_eq!(user.normalized, "bob example");
        assert_eq!(user.raw, "  Bob Example  ");
    }

    #[test]
    fn classify_covers_missing_sign_and_threshold_boundaries() {
        assert_eq!(classify(None), "missing");
        assert_eq!(classify(Some(-1)), "non-positive");
        assert_eq!(classify(Some(0)), "non-positive");
        assert_eq!(classify(Some(1)), "positive");
        assert_eq!(classify(Some(10)), "positive");
        assert_eq!(classify(Some(11)), "large");
    }
}
