//! `cgroup` — Namespace and cgroup resource-isolation primitives.
//!
//! **This slice's concrete target:** resolving the real cgroup path of an
//! already-running process on the host, by pid — filling
//! `principal::ContainerRef::cgroup`, which is unconditionally `None` until
//! something actually resolves it. `lifecycle::Lifecycle::container_ref`
//! is the caller that wires this in for a real dockerd container.
//!
//! **Scope boundary (hard constraint, same as every substrate-kit
//! module):** this is OS mechanics only. It resolves *what a cgroup path
//! is*, never *why* a cgroup limit is set to a given value or *who* should
//! be allowed to see/change it — that's an authorization/policy question
//! for identity-kit to answer, not this crate. See `principal.rs`'s
//! module doc for the full hard-constraint statement.
//!
//! **Known limitation, not an oversight:** this resolver only understands
//! the cgroup v2 unified hierarchy (`/proc/<pid>/cgroup`'s `0::<path>`
//! line), matching the target hosts (dockerd with the systemd
//! cgroup driver defaults to cgroup v2 on any reasonably current kernel).
//! A cgroup v1 or hybrid host — no `0::` line present — resolves to
//! [`SubstrateError::Resolve`] rather than guessing at a v1 controller
//! hierarchy; broader support is real follow-up work if a v1 host ever
//! needs to run this substrate.
//!
//! Full namespace/cgroup *isolation* primitives (creating/limiting a
//! cgroup, not just reading one back) remain deferred; resource limits on a
//! container are not yet part of `lifecycle::ContainerSpec` either.

use crate::error::SubstrateError;

/// The cgroup v2 unified hierarchy's standard mount point. Universal on
/// every distro this substrate targets — not configurable here because
/// none of its target hosts mounts cgroup v2 anywhere else.
pub const CGROUP_V2_MOUNT: &str = "/sys/fs/cgroup";

/// Resolve the real, absolute cgroup v2 path for a running process on the
/// host, by pid.
///
/// Reads `/proc/<pid>/cgroup` — world-readable for any process on a normal
/// Linux host, unlike `/proc/<pid>/ns/*` (see `mount::resolve_mount_namespace`'s
/// doc for that contrast) — so this does not need elevated privilege to
/// resolve another user's (e.g. a root-owned dockerd container's) cgroup.
pub fn resolve_cgroup_path(pid: u32) -> Result<String, SubstrateError> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))?;
    let relative = parse_unified_cgroup_path(&raw).ok_or_else(|| {
        SubstrateError::Resolve(format!(
            "no cgroup v2 unified (`0::`) entry in /proc/{pid}/cgroup — \
             cgroup v1/hybrid hosts are not resolved by this function"
        ))
    })?;
    Ok(format!("{CGROUP_V2_MOUNT}{relative}"))
}

/// Extract the path component of `/proc/<pid>/cgroup`'s cgroup v2 unified
/// hierarchy line (`0::<path>`). Cgroup v1/hybrid hosts emit additional
/// numbered per-controller lines (`N:<controllers>:<path>`) that this
/// deliberately ignores — see this module's doc for why.
fn parse_unified_cgroup_path(raw: &str) -> Option<String> {
    raw.lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_unified_cgroup_path_extracts_the_v2_line() {
        let raw = "0::/system.slice/docker-abc123.scope\n";
        assert_eq!(
            parse_unified_cgroup_path(raw),
            Some("/system.slice/docker-abc123.scope".to_string())
        );
    }

    #[test]
    fn parse_unified_cgroup_path_is_none_without_a_v2_entry() {
        // A cgroup v1/hybrid host's shape — numbered per-controller lines,
        // no `0::` unified entry.
        let raw = "12:memory:/docker/abc123\n11:cpu,cpuacct:/docker/abc123\n";
        assert_eq!(parse_unified_cgroup_path(raw), None);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn resolve_cgroup_path_resolves_this_process_own_cgroup_for_real() {
        // A real `/proc` read against this test process itself — no
        // Docker needed for this sanity check. The container-based, real
        // dockerd resolution is exercised by `lifecycle`'s live tests via
        // `Lifecycle::container_ref`.
        let pid = std::process::id();
        let path = resolve_cgroup_path(pid).expect(
            "resolve own cgroup path (fails on a cgroup v1/hybrid host — see this module's doc)",
        );
        assert!(
            path.starts_with(CGROUP_V2_MOUNT),
            "expected a path under {CGROUP_V2_MOUNT}, got: {path}"
        );
    }

    #[test]
    fn resolve_cgroup_path_fails_cleanly_for_a_nonexistent_pid() {
        let bogus_pid = u32::MAX;
        let err = resolve_cgroup_path(bogus_pid).expect_err("nonexistent pid must fail");
        assert!(matches!(err, SubstrateError::Io(_)));
    }
}
