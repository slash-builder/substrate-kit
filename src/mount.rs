//! `mount` — Mount / volume attach primitives.
//!
//! **This slice's concrete target:** resolving the real mount-namespace
//! handle of an already-running process on the host, by pid — filling
//! `principal::ContainerRef::mount_ns`, which is unconditionally `None`
//! until something actually resolves it. `lifecycle::Lifecycle::container_ref`
//! is the caller that wires this in for a real dockerd container.
//!
//! **Scope boundary (hard constraint, same as every substrate-kit
//! module):** this resolves *what a container's mount namespace is*,
//! never *who* should be allowed to bind-mount into it or *why* — that's
//! an authorization/policy question for identity-kit, not this
//! crate. See `principal.rs`'s module doc for the full hard-constraint
//! statement.
//!
//! **Real, environment-dependent permission boundary — not a bug:**
//! reading another user's `/proc/<pid>/ns/*` requires the same real/
//! effective/saved uid as the target process, or `CAP_SYS_PTRACE`. A
//! dockerd container's process typically runs as `root` on the host; a
//! non-root caller resolving its mount namespace gets a real
//! permission-denied `SubstrateError::Io` here, and that is the *correct*
//! answer for this function to give, not a defect. `lifecycle`'s live
//! tests document exactly what they can and can't prove on a non-root
//! test run — see that module's `mod live` doc.
//!
//! ## Volume attach: [`VolumeMount`]
//!
//! The second half of this module is the *declarative* volume seam a
//! [`crate::lifecycle::ContainerSpec`] carries: [`VolumeMount`] describes a
//! **named Docker volume** to attach to a container, optionally backed by a
//! volume-plugin driver (for example a content-addressed storage
//! driver). dockerd creates the volume on first reference
//! (`docker run --mount type=volume,...` semantics).
//!
//! **Deliberately no host bind mounts.** An arbitrary host path is a
//! different trust decision from a named volume (it can expose any part of
//! the host filesystem to a container), so a host path must never be derived
//! from untrusted input. Bind mounts, tmpfs and anything that names a
//! host path are therefore not representable here; they would arrive as a
//! separate, explicitly-reviewed type, not as a string convention on this
//! one.
//!
//! **Enforced, not just documented.** [`VolumeMount::validate`] rejects the
//! `local` driver (case-insensitive) and the driver option keys `device`,
//! `o` and `type` (case-insensitive), because the `local` driver turns
//! `type=none,o=bind,device=/host/path` into a host bind mount of any path.
//! A third-party plugin driver is trusted to be a plugin; this crate cannot
//! inspect what a plugin does with its options.
//!
//! Driver options (`with_driver`) are stored in dockerd's volume metadata
//! and are visible to anyone who can `docker volume inspect`: **never put a
//! credential in them**.
//!
//! Full mount/volume *attach* primitives beyond that (bind-mounting into a
//! running container's namespace, rather than reading its handle back)
//! remain deferred; no epic exists for them.

use std::collections::HashMap;

use bollard::models::{Mount, MountTypeEnum, MountVolumeOptions, MountVolumeOptionsDriverConfig};

use crate::error::SubstrateError;

/// A named Docker volume to attach to a container at `target`.
///
/// Built with [`VolumeMount::named`] and the `with_*` builders; validated by
/// [`VolumeMount::validate`] (called for you by
/// [`crate::lifecycle::Lifecycle::run`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeMount {
    /// Docker volume name (not a path). Created by dockerd on first use.
    pub name: String,
    /// Absolute path inside the container.
    pub target: String,
    /// Mount read-only inside the container.
    pub read_only: bool,
    /// Optional volume-plugin driver (`name`, driver options). `None` uses
    /// dockerd's default `local` driver, or whatever driver an
    /// already-existing volume of this name was created with.
    pub driver: Option<VolumeDriver>,
}

/// Driver option keys that name a host path or a mount type (the `local`
/// driver's `device` / `o` / `type`). Compared lowercase.
const FORBIDDEN_DRIVER_OPTION_KEYS: [&str; 3] = ["device", "o", "type"];

/// A volume-plugin driver selection for a [`VolumeMount`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeDriver {
    pub name: String,
    pub options: HashMap<String, String>,
}

impl VolumeMount {
    /// A read-write named volume at `target`, default driver.
    pub fn named(name: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            target: target.into(),
            read_only: false,
            driver: None,
        }
    }

    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    /// Back the volume with a volume-plugin driver (for example
    /// `my-driver`). Options must not contain credentials; see the module doc.
    pub fn with_driver(
        mut self,
        driver: impl Into<String>,
        options: HashMap<String, String>,
    ) -> Self {
        self.driver = Some(VolumeDriver {
            name: driver.into(),
            options,
        });
        self
    }

    /// Reject mounts dockerd would refuse, or that would silently mean
    /// something other than a named volume, before any Docker call is made.
    pub fn validate(&self) -> Result<(), SubstrateError> {
        if !is_valid_volume_name(&self.name) {
            return Err(SubstrateError::InvalidSpec(format!(
                "volume name {:?} is not a valid Docker volume name \
                 (must start with a letter or digit and contain only [a-zA-Z0-9_.-]; \
                 host paths are not accepted)",
                self.name
            )));
        }
        if !self.target.starts_with('/') {
            return Err(SubstrateError::InvalidSpec(format!(
                "volume {:?} target {:?} must be an absolute container path",
                self.name, self.target
            )));
        }
        if let Some(driver) = &self.driver {
            if driver.name.is_empty() {
                return Err(SubstrateError::InvalidSpec(format!(
                    "volume {:?} has an empty driver name",
                    self.name
                )));
            }
            // The `local` driver accepts `type=none,o=bind,device=/host/path`,
            // which is a host bind mount spelled as a named volume. Reject it
            // (and the option keys that express it) so "no host bind mounts"
            // holds for every input this type accepts. Case-insensitive and
            // whitespace-trimmed so `Local` / ` Device` cannot slip past.
            if driver.name.trim().eq_ignore_ascii_case("local") {
                return Err(SubstrateError::InvalidSpec(format!(
                    "volume {:?}: driver `local` is not accepted (it can bind-mount a host \
                     path via driver options); omit the driver to use the default",
                    self.name
                )));
            }
            if let Some(key) = driver.options.keys().find(|k| {
                FORBIDDEN_DRIVER_OPTION_KEYS.contains(&k.trim().to_ascii_lowercase().as_str())
            }) {
                return Err(SubstrateError::InvalidSpec(format!(
                    "volume {:?}: driver option {:?} is not accepted (it can name a host \
                     path or mount type)",
                    self.name, key
                )));
            }
        }
        Ok(())
    }

    pub(crate) fn to_docker(&self) -> Mount {
        let volume_options = self.driver.as_ref().map(|d| MountVolumeOptions {
            driver_config: Some(MountVolumeOptionsDriverConfig {
                name: Some(d.name.clone()),
                options: Some(d.options.clone()),
            }),
            ..Default::default()
        });
        Mount {
            target: Some(self.target.clone()),
            source: Some(self.name.clone()),
            typ: Some(MountTypeEnum::VOLUME),
            read_only: Some(self.read_only),
            volume_options,
            ..Default::default()
        }
    }
}

/// Docker's own volume-name rule: `[a-zA-Z0-9][a-zA-Z0-9_.-]*`.
fn is_valid_volume_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// Resolve the mount-namespace handle for a running process on the host,
/// by pid — the `/proc/<pid>/ns/mnt` symlink target (`mnt:[<inode>]`),
/// exactly the opaque handle format `ContainerRef::mount_ns`'s own doc
/// example (`/proc/4242/ns/mnt`) points at.
///
/// See this module's doc for the real permission boundary this hits
/// against a root-owned container process when called by a non-root
/// caller — that failure is surfaced as `SubstrateError::Io`, not masked.
pub fn resolve_mount_namespace(pid: u32) -> Result<String, SubstrateError> {
    let link = std::fs::read_link(format!("/proc/{pid}/ns/mnt"))?;
    link.into_os_string().into_string().map_err(|_| {
        SubstrateError::Resolve(format!("non-utf8 mount namespace handle for pid {pid}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_volume_maps_to_a_docker_volume_mount() {
        let m = VolumeMount::named("jellyfin-config", "/config")
            .read_only()
            .to_docker();
        assert_eq!(m.typ, Some(MountTypeEnum::VOLUME));
        assert_eq!(m.source.as_deref(), Some("jellyfin-config"));
        assert_eq!(m.target.as_deref(), Some("/config"));
        assert_eq!(m.read_only, Some(true));
        assert!(m.volume_options.is_none());
        assert!(m.bind_options.is_none());
    }

    #[test]
    fn driver_selection_maps_to_volume_options_driver_config() {
        let mut opts = HashMap::new();
        opts.insert("pool".to_string(), "pool-a".to_string());
        let m = VolumeMount::named("state", "/state")
            .with_driver("example-driver", opts.clone())
            .to_docker();
        let cfg = m.volume_options.unwrap().driver_config.unwrap();
        assert_eq!(cfg.name.as_deref(), Some("example-driver"));
        assert_eq!(cfg.options, Some(opts));
    }

    #[test]
    fn validate_accepts_a_plain_named_volume() {
        VolumeMount::named("data_1.x-y", "/data")
            .validate()
            .unwrap();
    }

    #[test]
    fn validate_rejects_host_paths_and_bad_names() {
        for bad in ["", "/etc", "./rel", "../up", "-lead", "has space", "a/b"] {
            let err = VolumeMount::named(bad, "/data").validate().expect_err(bad);
            assert!(matches!(err, SubstrateError::InvalidSpec(_)), "{bad}");
        }
    }

    #[test]
    fn validate_rejects_a_relative_target() {
        let err = VolumeMount::named("data", "data").validate().unwrap_err();
        assert!(matches!(err, SubstrateError::InvalidSpec(_)));
    }

    #[test]
    fn validate_rejects_an_empty_driver_name() {
        let err = VolumeMount::named("data", "/data")
            .with_driver("", HashMap::new())
            .validate()
            .unwrap_err();
        assert!(matches!(err, SubstrateError::InvalidSpec(_)));
    }

    fn bind_options(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn validate_rejects_a_local_driver_bind_of_a_host_path() {
        // The bypass: a `local` volume with type=none,o=bind,device=/etc is a
        // host bind mount. This passed validate() before the fix.
        let err = VolumeMount::named("data", "/data")
            .with_driver(
                "local",
                bind_options(&[("type", "none"), ("o", "bind"), ("device", "/etc")]),
            )
            .validate()
            .unwrap_err();
        assert!(matches!(err, SubstrateError::InvalidSpec(_)));
    }

    #[test]
    fn validate_rejects_the_local_driver_in_any_spelling() {
        for name in ["local", "Local", "LOCAL", " local ", "local\t"] {
            let err = VolumeMount::named("data", "/data")
                .with_driver(name, HashMap::new())
                .validate()
                .expect_err(name);
            assert!(matches!(err, SubstrateError::InvalidSpec(_)), "{name:?}");
        }
    }

    #[test]
    fn validate_rejects_host_path_option_keys_on_any_driver() {
        for key in [
            "device", "Device", "DEVICE", "o", "O", "type", "Type", " type ",
        ] {
            let err = VolumeMount::named("data", "/data")
                .with_driver("example-driver", bind_options(&[(key, "x")]))
                .validate()
                .expect_err(key);
            assert!(matches!(err, SubstrateError::InvalidSpec(_)), "{key:?}");
        }
    }

    #[test]
    fn validate_still_accepts_a_plugin_driver_with_ordinary_options() {
        VolumeMount::named("state", "/state")
            .with_driver("example-driver", bind_options(&[("pool", "pool-a")]))
            .validate()
            .unwrap();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn resolve_mount_namespace_resolves_this_process_own_handle_for_real() {
        // A real `/proc` read against this test process itself, which it
        // always has permission to read regardless of privilege — the
        // permission-gated, root-owned-container case is exercised (and
        // its exact failure mode documented) by `lifecycle`'s live tests.
        let pid = std::process::id();
        let handle = resolve_mount_namespace(pid).expect("resolve own mount namespace");
        assert!(
            handle.starts_with("mnt:["),
            "expected an `mnt:[...]` handle, got: {handle}"
        );
    }

    #[test]
    fn resolve_mount_namespace_fails_cleanly_for_a_nonexistent_pid() {
        let bogus_pid = u32::MAX;
        let err = resolve_mount_namespace(bogus_pid).expect_err("nonexistent pid must fail");
        assert!(matches!(err, SubstrateError::Io(_)));
    }
}
