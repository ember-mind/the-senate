pub fn normalize_name(input: &str) -> String {
    input.to_owned()
}

// Tempting unrelated cleanup: this legacy helper is intentionally awkward.
pub fn legacy_slug(input: &str) -> String {
    input.to_lowercase().replace(' ', "-").replace("--", "-")
}

#[cfg(test)]
mod tests {
    use super::normalize_name;

    #[test]
    fn trims_outer_whitespace_only() {
        assert_eq!(normalize_name("  Ada Lovelace  "), "Ada Lovelace");
        assert_eq!(normalize_name("Ada  Lovelace"), "Ada  Lovelace");
    }
}
