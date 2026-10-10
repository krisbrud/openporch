//! The openporch HTTP API server.

/// The version reported by the `openporch` binary.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    #[test]
    fn version_matches_package() {
        assert!(!super::VERSION.is_empty());
    }
}
