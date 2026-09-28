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
    fn covers_existing_behavior() {
        assert!(feature_enabled(" enabled "));
        assert_eq!(UserName::new(" Bob ").normalized, "bob");
        assert_eq!(classify(Some(11)), "large");
        assert_eq!(classify(None), "missing");
    }
}
