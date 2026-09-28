//! Compile-time embedded `world/` browser assets (see `build.rs`). There is
//! no filesystem access at runtime, so there is no path to traverse: a
//! request path either matches one compiled-in entry exactly or it is
//! treated as not found.

include!(concat!(env!("OUT_DIR"), "/world_assets.rs"));

/// Looks up one embedded asset by its URL path (e.g. `/js/aquila.js`).
/// Returns its content type and bytes, or `None` when nothing was embedded
/// at that path.
#[must_use]
pub fn lookup(path: &str) -> Option<(&'static str, &'static [u8])> {
    ASSETS
        .iter()
        .find(|(candidate, _, _)| *candidate == path)
        .map(|(_, content_type, bytes)| (*content_type, *bytes))
}

/// The embedded `index.html`, if one was present under `world/` at build
/// time.
#[must_use]
pub fn index() -> Option<(&'static str, &'static [u8])> {
    lookup("/index.html")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_is_embedded() {
        assert!(index().is_some(), "world/index.html should be embedded");
    }

    #[test]
    fn unknown_path_is_absent() {
        assert!(lookup("/definitely-not-here.js").is_none());
    }

    #[test]
    fn traversal_attempts_never_match_an_embedded_asset() {
        assert!(lookup("/../Cargo.toml").is_none());
        assert!(lookup("../Cargo.toml").is_none());
        assert!(lookup("/../../etc/passwd").is_none());
    }
}
