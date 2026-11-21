//! Error type for Substrate Kit operations.

use thiserror::Error;

///
/// `#[non_exhaustive]`: new variants are additive from here on. (Adding the
/// attribute, and the `InvalidSpec` variant below, is the one breaking change
/// of the 0.1.0 release for anyone matching exhaustively; pre-1.0.)
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SubstrateError {
    /// A capability that is not implemented in this release; the payload names what is missing.
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),

    /// An I/O operation failed, for example reading a `/proc` file.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// A dockerd Engine API call (via `bollard`) failed. Purely a
    /// mechanics-layer failure — this crate never inspects the underlying
    /// `bollard::errors::Error` to make an authorization decision, only to
    /// distinguish transient/not-found conditions (see `lifecycle`'s
    /// `is_not_found` helper) from real failures.
    #[error("dockerd: {0}")]
    Docker(#[from] bollard::errors::Error),

    /// An OS-level fact (cgroup path, mount-namespace handle, uid) could
    /// not be resolved from a running container/process — e.g. malformed
    /// `/proc` content, an unsupported cgroup hierarchy, or a container
    /// that isn't actually running. Distinct from `Io`, which covers the
    /// read itself failing (including permission-denied reading another
    /// user's `/proc/<pid>/ns/*`, a real and expected boundary — see
    /// `mount::resolve_mount_namespace`'s doc).
    #[error("could not resolve {0}")]
    Resolve(String),

    /// A caller-supplied spec (`ContainerSpec`, `VolumeMount`, `PortMapping`)
    /// was rejected before any dockerd call was made — a malformed volume
    /// name, a relative mount target, a duplicate port publish, etc.
    #[error("invalid spec: {0}")]
    InvalidSpec(String),

    /// A poll (e.g. waiting for a container to reach and stay at Running)
    /// exceeded its deadline.
    #[error("timed out: {0}")]
    Timeout(String),
}
