//! # Substrate Kit
//!
//! The board-agnostic Linux+dockerd substrate that services run on: container lifecycle, cgroups, mounts, and the
//! service-principal embodiment layer.
//!
//! Linux is the supported platform. The crate compiles elsewhere, but cgroup and mount-namespace resolution read
//! the Linux `/proc` filesystem. See the README for scope and status.

pub mod cgroup;
pub mod error;
pub mod lifecycle;
pub mod mount;
pub mod principal;
pub mod socket;

pub use error::SubstrateError;
pub use principal::{embody, ContainerRef, Embodiment};

/// Crate version, sourced from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!VERSION.is_empty());
    }
}
