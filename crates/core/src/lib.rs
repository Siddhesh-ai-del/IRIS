//! `ferro-core` — provider-agnostic agent core: domain types, tool registry,
//! permission gate, and the headless loop. This crate must stay TUI-free.

/// Library version, mirroring the workspace package version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod config;
pub mod types;

#[cfg(test)]
mod tests {
    use super::VERSION;

    #[test]
    fn version_is_semver() {
        let mut parts = VERSION.split('.');
        assert!(parts.next().is_some_and(|p| p.parse::<u32>().is_ok()));
        assert!(parts.next().is_some_and(|p| p.parse::<u32>().is_ok()));
        assert!(parts.next().is_some_and(|p| p.parse::<u32>().is_ok()));
        assert_eq!(parts.next(), None);
    }
}
