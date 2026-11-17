//! `principal` — service-principal embodiment.
//!
//! Binds a running container/process on the box (plain OS-level facts —
//! container id, uid, optional cgroup/mount-namespace handles) to an
//! already-defined [`identity_kit::ServicePrincipal`].
//!
//! **Hard constraint:** identity-kit *owns* the grant data model; substrate-kit
//! only *embodies* it. This module must never construct a `Capabilities`
//! bundle or a principal definition of its own — `embody` only takes an
//! already-built `ServicePrincipal` and pairs it with the OS facts of a
//! running container. See `identity_kit::authz::ServicePrincipal` for the
//! type this resolves to, verbatim.
//!
//! This slice is the descriptor + binding only — no live dockerd call.
//! Resolving a real `ContainerRef` from a running container is
//! `lifecycle`'s job, in a later slice.

use identity_kit::{Scope, ServicePrincipal};

/// The OS-level facts of a running container/process on the box.
///
/// Plain descriptor. `cgroup`/`mount_ns` are opaque path handles filled in
/// once `cgroup`/`mount` modules do real resolution; unset here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerRef {
    /// dockerd container id (or a PID-derived id for a bare process).
    pub container_id: String,
    /// The uid the container/process runs as on the host.
    pub uid: u32,
    /// Resolved cgroup path, if known.
    pub cgroup: Option<String>,
    /// Resolved mount-namespace handle (e.g. `/proc/<pid>/ns/mnt`), if
    /// known.
    pub mount_ns: Option<String>,
}

impl ContainerRef {
    /// A bare container reference: just the id and the uid it runs as.
    pub fn new(container_id: impl Into<String>, uid: u32) -> Self {
        Self {
            container_id: container_id.into(),
            uid,
            cgroup: None,
            mount_ns: None,
        }
    }

    /// Attach a resolved cgroup path.
    pub fn with_cgroup(mut self, cgroup: impl Into<String>) -> Self {
        self.cgroup = Some(cgroup.into());
        self
    }

    /// Attach a resolved mount-namespace handle.
    pub fn with_mount_ns(mut self, mount_ns: impl Into<String>) -> Self {
        self.mount_ns = Some(mount_ns.into());
        self
    }
}

/// A running container/process bound to the [`ServicePrincipal`]
/// identity-kit defined for it.
///
/// `Embodiment` never constructs or mutates the principal it wraps — it
/// only pairs an already-built `identity_kit::ServicePrincipal` with the
/// [`ContainerRef`] facts of the container/process embodying it.
#[derive(Debug, Clone)]
pub struct Embodiment {
    container: ContainerRef,
    principal: ServicePrincipal,
}

/// Bind a running container/process to an already-defined
/// `ServicePrincipal`.
///
/// Maps the OS-level embodiment facts (uid/container) onto the principal
/// identity-kit owns. Does not construct capabilities or a principal
/// definition — `principal` must already exist; this only binds it.
pub fn embody(container: &ContainerRef, principal: ServicePrincipal) -> Embodiment {
    Embodiment {
        container: container.clone(),
        principal,
    }
}

impl Embodiment {
    /// The `ServicePrincipal` this container/process embodies, verbatim as
    /// identity-kit defined it.
    pub fn principal(&self) -> &ServicePrincipal {
        &self.principal
    }

    /// The OS-level facts of the running container/process.
    pub fn container(&self) -> &ContainerRef {
        &self.container
    }

    /// Whether the embodied principal holds the given scope. Delegates
    /// through to `identity_kit::ServicePrincipal::has`, which in turn
    /// delegates to `Capabilities::includes` — substrate-kit performs no
    /// capability logic of its own.
    pub fn has(&self, scope: &Scope) -> bool {
        self.principal.has(scope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use identity_kit::Capabilities;

    fn scope(s: &str) -> Scope {
        Scope(s.to_string())
    }

    #[test]
    fn embody_binds_container_to_the_given_principal() {
        let caps = Capabilities::new(vec![scope("run:container")]);
        let principal = ServicePrincipal::new(None, caps);
        let principal_id = principal.id();

        let container = ContainerRef::new("c-1234", 1000)
            .with_cgroup("/sys/fs/cgroup/substrate/c-1234")
            .with_mount_ns("/proc/4242/ns/mnt");

        let embodiment = embody(&container, principal);

        assert_eq!(embodiment.principal().id(), principal_id);
        assert_eq!(embodiment.container(), &container);
    }

    #[test]
    fn embodiment_has_delegates_through_to_service_principal() {
        let caps = Capabilities::new(vec![scope("pair:device")]);
        let principal = ServicePrincipal::new(None, caps);
        let container = ContainerRef::new("c-5678", 1001);

        let embodiment = embody(&container, principal);

        assert!(embodiment.has(&scope("pair:device")));
        assert!(!embodiment.has(&scope("admin:everything")));
    }

    #[test]
    fn embody_never_constructs_capabilities_of_its_own() {
        // `embody`'s signature takes an already-built ServicePrincipal —
        // it has no path to construct Capabilities itself. This round-
        // trips a principal built entirely through identity-kit's own
        // constructors and confirms the capability set survives embodiment
        // unmodified.
        let caps = Capabilities::new(vec![scope("read:account"), scope("write:family")]);
        let principal = ServicePrincipal::new(None, caps.clone());
        let container = ContainerRef::new("c-9", 1002);

        let embodiment = embody(&container, principal);

        assert_eq!(embodiment.principal().capabilities().scopes, caps.scopes);
    }

    /// Hard-constraint guard test (not merely behavioural): substrate-kit
    /// never defines its own principal shape — `Embodiment::principal`
    /// must resolve to `identity_kit::ServicePrincipal` verbatim. If a
    /// future edit swapped in a substrate-kit-local principal type, this
    /// helper's parameter type stops compiling and the PR has to face the
    /// hard constraint explicitly rather than slip past review.
    #[test]
    fn embodiment_principal_is_identity_kit_service_principal_verbatim() {
        fn assert_is_identity_kit_principal(_: &identity_kit::ServicePrincipal) {}

        let principal = ServicePrincipal::new(None, Capabilities::default());
        let container = ContainerRef::new("c-guard", 0);
        let embodiment = embody(&container, principal);

        assert_is_identity_kit_principal(embodiment.principal());
    }
}
