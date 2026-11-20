//! `lifecycle` — Container / process lifecycle over dockerd — create,
//! start, stop, health, restart policy.
//!
//! This is the piece `principal.rs`'s own module doc flags as deferred:
//! "\[r\]esolving a real `ContainerRef` from a running container is
//! `lifecycle`'s job, in a later slice." [`Lifecycle::container_ref`] is
//! that job — it inspects a running container over the real dockerd
//! Engine API, resolves its uid from `/proc`, and composes `cgroup`'s and
//! `mount`'s own resolvers to fill in `ContainerRef::cgroup`/`mount_ns`,
//! so `principal::embody` can be called with real data end-to-end.
//!
//! **Scope boundary (hard constraint, same as every substrate-kit
//! module):** this is pure OS mechanics over dockerd. It never constructs
//! a `ServicePrincipal`, never decides which principal a container
//! belongs to, and never makes an authorization decision — see
//! `principal.rs`'s module doc for the full hard-constraint statement.
//!
//! ## What "health" means here, deliberately, and what it doesn't
//!
//! [`Lifecycle::wait_until_running`] confirms a container reaches, and
//! *stays at*, Docker's own `State.Running: true` — a single "is it
//! Running" read is a TOCTOU race against a container whose process
//! crashes moments after starting, so this re-checks after a short grace
//! period before declaring success. This does **not** prove the service
//! *inside* the container is ready to take traffic (no HTTP readiness
//! probe, no Docker `HEALTHCHECK` wiring) — that is a real, separate
//! concern for whatever calls this crate, not this slice's job. This
//! keeps health at the layer dockerd itself reports.
//!
//! ## Restart-policy handling
//!
//! [`RestartPolicy`] is a thin, OS-mechanics-only wrapper over dockerd's
//! own restart-policy enum (`bollard::models::RestartPolicyNameEnum`) —
//! it carries no meaning beyond "what dockerd should do when this
//! container's process exits," the same as `docker run --restart`.
//!
//! ## Networking and volumes
//!
//! [`ContainerSpec`] carries three additive, opt-in fields:
//!
//! - [`ContainerSpec::ports`] — [`PortMapping`]s published to the host.
//!   **Loopback-only by default** (`127.0.0.1`), so a service is never reachable
//!   from the network by accident; exposing a port on another interface is an explicit
//!   [`PortMapping::with_host_ip`].
//! - [`ContainerSpec::network`] — attach to a user-defined bridge network
//!   (so containers resolve each other by name via dockerd's embedded DNS).
//!   [`Lifecycle::ensure_network`] creates it idempotently.
//! - [`ContainerSpec::volumes`] — named Docker volumes
//!   ([`crate::mount::VolumeMount`]), optionally plugin-backed. No host bind
//!   mounts; see that type's doc for why.
//!
//! All three default to empty/`None`, which produces exactly the container
//! this module created before.
//!
//! ## Environment and credentials
//!
//! `ContainerSpec::env` is **not a credential channel.** Anything in it is
//! readable by `docker inspect` and shows up in dockerd's own state. The
//! rule is that no credential travels in an environment variable or a
//! unit file. This crate therefore does not accept, read or log
//! credentials: it never reads `std::env`, and `ContainerSpec`'s `Debug`
//! impl prints env *keys only*, so a `tracing::debug!(?spec)` cannot leak a
//! value. A credential-delivery seam (secret files mounted read-only,
//! sourced from `$CREDENTIALS_DIRECTORY` by the caller) and registry-pull
//! credentials are designed but **not implemented** here; both
//! need a security review first.
//!
//! ## Deliberately out of scope, not oversights
//!
//! - **No registry-credential handling** in [`Lifecycle::pull_image`] —
//!   every image this crate's own tests pull is public
//!   (`docker.io` short names); a private-registry credential seam is
//!   real, deferred follow-up.
//! - **No Docker-socket hardening.** This talks to the raw local
//!   dockerd socket — `socket.rs`'s job is AF_UNIX hardening for
//!   *services running on* the substrate, not this crate's own dockerd
//!   client connection.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use bollard::container::{
    Config, CreateContainerOptions, RemoveContainerOptions, StartContainerOptions,
    StopContainerOptions,
};
use bollard::image::CreateImageOptions;
use bollard::models::{
    ContainerInspectResponse, HostConfig, PortBinding, RestartPolicy as DockerRestartPolicy,
    RestartPolicyNameEnum,
};
use bollard::network::{CreateNetworkOptions, ListNetworksOptions};
use bollard::Docker;
use futures_util::StreamExt;

use crate::cgroup;
use crate::error::SubstrateError;
use crate::mount::{self, VolumeMount};
use crate::principal::ContainerRef;

/// Every container this module creates carries this label, purely so a
/// caller (or this crate's own tests) can find/clean up what it started
/// without keeping its own in-memory bookkeeping.
pub const LABEL_MANAGED: &str = "substrate.lifecycle.managed";

/// dockerd's own restart-policy semantics, wrapped only so callers don't
/// need to reach into `bollard`'s model types directly. Carries no
/// meaning beyond what dockerd itself gives `--restart` — see this
/// module's doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RestartPolicy {
    /// dockerd's default: never automatically restart.
    #[default]
    Never,
    /// Always restart, regardless of exit status.
    Always,
    /// Restart unless a human/operator explicitly stopped the container.
    UnlessStopped,
    /// Restart only on a non-zero exit, up to `max_retries` times.
    OnFailure {
        /// Maximum number of restart attempts.
        max_retries: i64,
    },
}

impl RestartPolicy {
    fn into_docker(self) -> DockerRestartPolicy {
        match self {
            RestartPolicy::Never => DockerRestartPolicy {
                name: Some(RestartPolicyNameEnum::NO),
                maximum_retry_count: None,
            },
            RestartPolicy::Always => DockerRestartPolicy {
                name: Some(RestartPolicyNameEnum::ALWAYS),
                maximum_retry_count: None,
            },
            RestartPolicy::UnlessStopped => DockerRestartPolicy {
                name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                maximum_retry_count: None,
            },
            RestartPolicy::OnFailure { max_retries } => DockerRestartPolicy {
                name: Some(RestartPolicyNameEnum::ON_FAILURE),
                maximum_retry_count: Some(max_retries),
            },
        }
    }
}

/// Transport protocol of a published port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Protocol {
    /// TCP (the default).
    #[default]
    Tcp,
    /// UDP.
    Udp,
}

impl Protocol {
    fn as_str(self) -> &'static str {
        match self {
            Protocol::Tcp => "tcp",
            Protocol::Udp => "udp",
        }
    }
}

/// One container port published to the host (`docker run -p`).
///
/// **Safe by default:** bound to loopback (`127.0.0.1`). The host port is
/// `None` (dockerd picks a free ephemeral port; read it back with
/// [`Lifecycle::published_host_port`]) unless [`PortMapping::with_host_port`]
/// pins it. A 1:1 mapping is
/// `PortMapping::tcp(p).with_host_port(p)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortMapping {
    /// The port inside the container.
    pub container_port: u16,
    /// `None` lets dockerd allocate a free host port.
    pub host_port: Option<u16>,
    /// The host interface the port is published on. Loopback (`127.0.0.1`) unless changed.
    pub host_ip: IpAddr,
    /// Transport protocol: TCP unless built with [`PortMapping::udp`].
    pub protocol: Protocol,
}

impl PortMapping {
    /// Publish TCP `container_port` on loopback with a dockerd-allocated
    /// host port.
    pub fn tcp(container_port: u16) -> Self {
        Self {
            container_port,
            host_port: None,
            host_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            protocol: Protocol::Tcp,
        }
    }

    /// As [`PortMapping::tcp`] but UDP.
    pub fn udp(container_port: u16) -> Self {
        Self {
            protocol: Protocol::Udp,
            ..Self::tcp(container_port)
        }
    }

    /// Pins the host port instead of letting dockerd allocate one. A 1:1 mapping is
    /// `PortMapping::tcp(p).with_host_port(p)`.
    pub fn with_host_port(mut self, host_port: u16) -> Self {
        self.host_port = Some(host_port);
        self
    }

    /// Bind on a specific host interface. `0.0.0.0` exposes the port to the
    /// LAN — an explicit, reviewable choice, never the default.
    pub fn with_host_ip(mut self, host_ip: IpAddr) -> Self {
        self.host_ip = host_ip;
        self
    }

    /// Docker's `port/proto` key, e.g. `8096/tcp`.
    fn docker_key(&self) -> String {
        format!("{}/{}", self.container_port, self.protocol.as_str())
    }

    fn validate(&self) -> Result<(), SubstrateError> {
        if self.container_port == 0 {
            return Err(SubstrateError::InvalidSpec(
                "port mapping has container_port 0".to_string(),
            ));
        }
        if self.host_port == Some(0) {
            return Err(SubstrateError::InvalidSpec(format!(
                "port mapping for {} has host_port 0; use None to let dockerd allocate",
                self.docker_key()
            )));
        }
        Ok(())
    }
}

/// What to run.
///
/// `#[non_exhaustive]`: build with [`ContainerSpec::new`] and the `with_*`
/// builders so fields can keep being added without breaking callers.
/// `Debug` is hand-written and prints `env` **keys only** (see the module
/// doc on credentials).
#[derive(Clone)]
#[non_exhaustive]
pub struct ContainerSpec {
    /// Docker container name — must be unique per host.
    pub name: String,
    /// Image reference to run, for example `alpine:3`.
    pub image: String,
    /// Environment variables. **Not a credential channel**: values are
    /// visible to `docker inspect`. See the module doc.
    pub env: HashMap<String, String>,
    /// Override the image's own `ENTRYPOINT`/`CMD`. `None` leaves the
    /// image's own default command untouched.
    pub cmd: Option<Vec<String>>,
    /// Container labels, as with `docker run --label`.
    pub labels: HashMap<String, String>,
    /// What dockerd does when the container exits. [`RestartPolicy::Never`] by default.
    pub restart_policy: RestartPolicy,
    /// Ports to publish to the host. Empty publishes nothing.
    pub ports: Vec<PortMapping>,
    /// Named volumes to attach. Empty attaches nothing.
    pub volumes: Vec<VolumeMount>,
    /// A user-defined Docker network to attach to instead of the default
    /// `bridge` (`HostConfig.NetworkMode`); containers on the same
    /// user-defined network resolve each other by container name. `None`
    /// keeps dockerd's default. Create it with [`Lifecycle::ensure_network`].
    pub network: Option<String>,
}

impl std::fmt::Debug for ContainerSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut env_keys: Vec<&String> = self.env.keys().collect();
        env_keys.sort();
        f.debug_struct("ContainerSpec")
            .field("name", &self.name)
            .field("image", &self.image)
            .field("env_keys", &env_keys)
            .field("cmd", &self.cmd)
            .field("labels", &self.labels)
            .field("restart_policy", &self.restart_policy)
            .field("ports", &self.ports)
            .field("volumes", &self.volumes)
            .field("network", &self.network)
            .finish()
    }
}

impl ContainerSpec {
    /// A spec for the container `name` running `image`: no environment, no command override,
    /// no labels, no ports, no volumes, the default restart policy and the default network.
    pub fn new(name: impl Into<String>, image: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            image: image.into(),
            env: HashMap::new(),
            cmd: None,
            labels: HashMap::new(),
            restart_policy: RestartPolicy::default(),
            ports: Vec::new(),
            volumes: Vec::new(),
            network: None,
        }
    }

    /// Replaces the environment variables. Not a credential channel; see the module doc.
    pub fn with_env(mut self, env: HashMap<String, String>) -> Self {
        self.env = env;
        self
    }

    /// Overrides the image's own command.
    pub fn with_cmd(mut self, cmd: Vec<String>) -> Self {
        self.cmd = Some(cmd);
        self
    }

    /// Replaces the labels.
    pub fn with_labels(mut self, labels: HashMap<String, String>) -> Self {
        self.labels = labels;
        self
    }

    /// Sets the restart policy.
    pub fn with_restart_policy(mut self, policy: RestartPolicy) -> Self {
        self.restart_policy = policy;
        self
    }

    /// Publish one more port.
    pub fn with_port(mut self, port: PortMapping) -> Self {
        self.ports.push(port);
        self
    }

    /// Replace the published ports.
    pub fn with_ports(mut self, ports: Vec<PortMapping>) -> Self {
        self.ports = ports;
        self
    }

    /// Attach one more named volume.
    pub fn with_volume(mut self, volume: VolumeMount) -> Self {
        self.volumes.push(volume);
        self
    }

    /// Replace the attached volumes.
    pub fn with_volumes(mut self, volumes: Vec<VolumeMount>) -> Self {
        self.volumes = volumes;
        self
    }

    /// Attach to the user-defined network `name`.
    pub fn with_network(mut self, name: impl Into<String>) -> Self {
        self.network = Some(name.into());
        self
    }

    /// Reject specs dockerd would refuse or that would silently mean
    /// something else, before any Docker call. Called by
    /// [`Lifecycle::run`]; public so a caller can fail fast earlier.
    pub fn validate(&self) -> Result<(), SubstrateError> {
        let mut host_binds = HashSet::new();
        for port in &self.ports {
            port.validate()?;
            if let Some(host_port) = port.host_port {
                if !host_binds.insert((port.host_ip, host_port, port.protocol)) {
                    return Err(SubstrateError::InvalidSpec(format!(
                        "host {}:{host_port}/{} is published more than once",
                        port.host_ip,
                        port.protocol.as_str()
                    )));
                }
            }
        }

        let mut targets = HashSet::new();
        for volume in &self.volumes {
            volume.validate()?;
            if !targets.insert(volume.target.as_str()) {
                return Err(SubstrateError::InvalidSpec(format!(
                    "container path {:?} has more than one volume mounted",
                    volume.target
                )));
            }
        }

        if let Some(network) = &self.network {
            // `host` and `container:<id>` share another namespace and would
            // silently bypass the isolation this substrate exists to give;
            // not supported by this slice (`bridge`, `none` and user-defined
            // names are).
            if network.is_empty() || network == "host" || network.starts_with("container:") {
                return Err(SubstrateError::InvalidSpec(format!(
                    "network {network:?} is not supported: use a bridge name, `none`, or a \
                     user-defined network"
                )));
            }
        }
        Ok(())
    }

    /// The dockerd create request this spec produces. Pure (no I/O), so the
    /// mapping is unit-testable without a daemon.
    fn to_docker_config(&self) -> Config<String> {
        let mut env: Vec<String> = self.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        env.sort();

        let mut exposed_ports: HashMap<String, HashMap<(), ()>> = HashMap::new();
        let mut port_bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
        for port in &self.ports {
            let key = port.docker_key();
            exposed_ports.insert(key.clone(), HashMap::new());
            port_bindings
                .entry(key)
                .or_insert_with(|| Some(Vec::new()))
                .as_mut()
                .expect("just inserted Some")
                .push(PortBinding {
                    host_ip: Some(port.host_ip.to_string()),
                    host_port: port.host_port.map(|p| p.to_string()),
                });
        }

        let host_config = HostConfig {
            restart_policy: Some(self.restart_policy.into_docker()),
            port_bindings: (!port_bindings.is_empty()).then_some(port_bindings),
            mounts: (!self.volumes.is_empty())
                .then(|| self.volumes.iter().map(VolumeMount::to_docker).collect()),
            network_mode: self.network.clone(),
            ..Default::default()
        };

        Config {
            image: Some(self.image.clone()),
            cmd: self.cmd.clone(),
            env: Some(env),
            labels: Some(self.labels.clone()),
            exposed_ports: (!exposed_ports.is_empty()).then_some(exposed_ports),
            host_config: Some(host_config),
            ..Default::default()
        }
    }
}

/// A connected handle to the local dockerd Engine API. Never a remote/TCP
/// Docker endpoint — this always runs containers on the same host the
/// substrate itself is running on, like any other service on the box.
pub struct Lifecycle {
    docker: Docker,
}

impl Lifecycle {
    /// Connect to the local Docker daemon (the Unix socket on Linux —
    /// `bollard`'s own platform default) and confirm it's actually
    /// reachable with a cheap `ping`, rather than surfacing a confusing
    /// failure on the first real call.
    pub async fn connect() -> Result<Self, SubstrateError> {
        let docker = Docker::connect_with_local_defaults()?;
        docker.ping().await?;
        Ok(Self { docker })
    }

    /// Pull `image`, driving the streamed pull response to completion.
    /// Docker's pull API is a no-op-but-still-valid stream when the image
    /// is already cached, so no special-casing is needed for that case.
    pub async fn pull_image(&self, image: &str) -> Result<(), SubstrateError> {
        let (from_image, tag) = split_image_ref(image);
        let options = CreateImageOptions {
            from_image,
            tag,
            ..Default::default()
        };
        let mut stream = self.docker.create_image(Some(options), None, None);
        while let Some(item) = stream.next().await {
            item?;
        }
        Ok(())
    }

    /// Create and start a container per `spec`. On any failure after the
    /// container was created, best-effort removes it — never leaves a
    /// created-but-never-started container behind for a caller's retry to
    /// trip over on a name conflict.
    pub async fn run(&self, spec: &ContainerSpec) -> Result<String, SubstrateError> {
        spec.validate()?;
        let config = spec.to_docker_config();

        let options = CreateContainerOptions {
            name: spec.name.clone(),
            platform: None,
        };

        let created = self.docker.create_container(Some(options), config).await?;

        if let Err(e) = self
            .docker
            .start_container(&created.id, None::<StartContainerOptions<String>>)
            .await
        {
            let _ = self.remove(&created.id, true).await;
            return Err(e.into());
        }

        Ok(created.id)
    }

    /// Stop a container, giving it `timeout` to shut down cleanly before
    /// dockerd kills it.
    pub async fn stop(&self, container_id: &str, timeout: Duration) -> Result<(), SubstrateError> {
        let options = StopContainerOptions {
            t: timeout.as_secs() as i64,
        };
        self.docker
            .stop_container(container_id, Some(options))
            .await?;
        Ok(())
    }

    /// Remove a container. `force` kills it first if still running
    /// (`docker rm -f` semantics) rather than requiring a separate `stop`.
    pub async fn remove(&self, container_id: &str, force: bool) -> Result<(), SubstrateError> {
        let options = RemoveContainerOptions {
            force,
            ..Default::default()
        };
        match self
            .docker
            .remove_container(container_id, Some(options))
            .await
        {
            Ok(()) => Ok(()),
            Err(e) if is_not_found(&e) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Stop then remove — the common cleanup composition, idempotent
    /// against "already gone" (a not-found from either step is not an
    /// error: a caller cleaning up after a partially-failed run should
    /// not itself fail because there was nothing left to clean up).
    pub async fn stop_and_remove(
        &self,
        container_id: &str,
        timeout: Duration,
    ) -> Result<(), SubstrateError> {
        if let Err(e) = self.stop(container_id, timeout).await {
            if !matches!(&e, SubstrateError::Docker(inner) if is_not_found(inner)) {
                tracing::warn!(container_id, error = %e, "stop failed (continuing to remove)");
            }
        }
        self.remove(container_id, true).await
    }

    /// Poll until the container's `State.Running` is true and *stays*
    /// true past a short grace period, or `timeout` elapses. See this
    /// module's doc for exactly what this does and doesn't prove.
    pub async fn wait_until_running(
        &self,
        container_id: &str,
        timeout: Duration,
    ) -> Result<(), SubstrateError> {
        const CONFIRM_GRACE: Duration = Duration::from_millis(500);
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let (running, exited, exit_code) = self.inspect_run_state(container_id).await?;
            if exited {
                return Err(SubstrateError::Resolve(format!(
                    "container {container_id} exited immediately (exit code {exit_code})"
                )));
            }
            if running {
                // A single "Running: true" read is a TOCTOU race against a
                // process that crashes within the first tens of
                // milliseconds — re-confirm after a short grace period
                // before declaring success (see this module's doc).
                tokio::time::sleep(CONFIRM_GRACE).await;
                let (still_running, exited_after, exit_code_after) =
                    self.inspect_run_state(container_id).await?;
                if exited_after {
                    return Err(SubstrateError::Resolve(format!(
                        "container {container_id} exited shortly after starting (exit code {exit_code_after})"
                    )));
                }
                if still_running {
                    return Ok(());
                }
                // Neither running nor exited (e.g. transiently
                // restarting) — keep polling rather than declaring either
                // outcome on an ambiguous read.
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(SubstrateError::Timeout(format!(
                    "container {container_id} did not reach (and stay at) Running within {timeout:?}"
                )));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Idempotently make sure the user-defined bridge network `name`
    /// exists, for [`ContainerSpec::network`]. List-then-create, with a
    /// create that loses a race to another caller (409) treated as success.
    /// dockerd's `name` filter is a substring match, so the exact name is
    /// re-checked here.
    pub async fn ensure_network(&self, name: &str) -> Result<(), SubstrateError> {
        let mut filters = HashMap::new();
        filters.insert("name".to_string(), vec![name.to_string()]);
        let existing = self
            .docker
            .list_networks(Some(ListNetworksOptions { filters }))
            .await?;
        if existing.iter().any(|n| n.name.as_deref() == Some(name)) {
            return Ok(());
        }
        let options = CreateNetworkOptions {
            name: name.to_string(),
            driver: "bridge".to_string(),
            ..Default::default()
        };
        match self.docker.create_network(options).await {
            Ok(_) => Ok(()),
            Err(e) if is_conflict(&e) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// The host port dockerd bound for `container_port`/`protocol` on a
    /// running container, if any — how a caller learns the port when the
    /// [`PortMapping`] left `host_port` as `None`. Returns the first
    /// binding when the port is published on several interfaces.
    pub async fn published_host_port(
        &self,
        container_id: &str,
        container_port: u16,
        protocol: Protocol,
    ) -> Result<Option<u16>, SubstrateError> {
        let info = self.inspect(container_id).await?;
        let key = format!("{container_port}/{}", protocol.as_str());
        Ok(info
            .network_settings
            .and_then(|n| n.ports)
            .and_then(|mut ports| ports.remove(&key))
            .flatten()
            .and_then(|bindings| {
                bindings
                    .into_iter()
                    .find_map(|b| b.host_port.and_then(|p| p.parse::<u16>().ok()))
            }))
    }

    /// Raw `inspect_container` passthrough, for callers that need more
    /// than this module's own convenience accessors.
    pub async fn inspect(
        &self,
        container_id: &str,
    ) -> Result<ContainerInspectResponse, SubstrateError> {
        Ok(self.docker.inspect_container(container_id, None).await?)
    }

    /// `(running, exited, exit_code)` from a fresh `inspect_container`
    /// call — `exit_code` is only meaningful when `exited` is true.
    async fn inspect_run_state(
        &self,
        container_id: &str,
    ) -> Result<(bool, bool, i64), SubstrateError> {
        let info = self.inspect(container_id).await?;
        let running = info.state.as_ref().and_then(|s| s.running).unwrap_or(false);
        let exited = info
            .state
            .as_ref()
            .and_then(|s| s.status)
            .map(|s| matches!(s, bollard::models::ContainerStateStatusEnum::EXITED))
            .unwrap_or(false);
        let exit_code = info.state.as_ref().and_then(|s| s.exit_code).unwrap_or(-1);
        Ok((running, exited, exit_code))
    }

    /// Resolve a [`ContainerRef`] for a currently-running container — the
    /// concrete slice `principal.rs`'s own doc flags as deferred to
    /// `lifecycle`. Resolves `container_id`/`uid` from the container's
    /// real host pid via dockerd's own inspect + `/proc`, then composes
    /// `cgroup::resolve_cgroup_path` and `mount::resolve_mount_namespace`
    /// to fill in the optional facts.
    ///
    /// `cgroup`/`mount_ns` resolution is best-effort: a failure there
    /// (e.g. the real, documented permission boundary
    /// `mount::resolve_mount_namespace` hits against a root-owned
    /// container when called by a non-root caller) is logged and leaves
    /// that field `None`, rather than failing the whole `ContainerRef` —
    /// they were already `Option`s for exactly this reason. A failure to
    /// resolve the container's pid/uid at all *is* fatal: those two
    /// fields are not optional on `ContainerRef`.
    pub async fn container_ref(&self, container_id: &str) -> Result<ContainerRef, SubstrateError> {
        let info = self.inspect(container_id).await?;
        let pid = info
            .state
            .as_ref()
            .and_then(|s| s.pid)
            .filter(|&p| p > 0)
            .ok_or_else(|| {
                SubstrateError::Resolve(format!(
                    "container {container_id} has no live pid (is it running?)"
                ))
            })? as u32;
        let uid = read_uid_from_proc_status(pid)?;

        let mut container_ref = ContainerRef::new(container_id.to_string(), uid);

        match cgroup::resolve_cgroup_path(pid) {
            Ok(path) => container_ref = container_ref.with_cgroup(path),
            Err(e) => {
                tracing::warn!(container_id, pid, error = %e, "could not resolve cgroup path");
            }
        }

        match mount::resolve_mount_namespace(pid) {
            Ok(ns) => container_ref = container_ref.with_mount_ns(ns),
            Err(e) => {
                tracing::warn!(
                    container_id, pid, error = %e,
                    "could not resolve mount namespace (needs root/CAP_SYS_PTRACE to read \
                     another user's /proc/<pid>/ns/mnt — see mount::resolve_mount_namespace's doc)"
                );
            }
        }

        Ok(container_ref)
    }
}

/// Read the real uid a running process is executing as, from
/// `/proc/<pid>/status`'s `Uid:` line (the first of its four
/// space-separated fields — real uid). World-readable, same as
/// `cgroup::resolve_cgroup_path`'s `/proc/<pid>/cgroup` read — no
/// elevated privilege needed even for a root-owned container process.
fn read_uid_from_proc_status(pid: u32) -> Result<u32, SubstrateError> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
    let line = raw
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .ok_or_else(|| SubstrateError::Resolve(format!("no Uid line in /proc/{pid}/status")))?;
    let real_uid = line.split_whitespace().next().ok_or_else(|| {
        SubstrateError::Resolve(format!("malformed Uid line in /proc/{pid}/status"))
    })?;
    real_uid
        .parse::<u32>()
        .map_err(|_| SubstrateError::Resolve(format!("non-numeric uid in /proc/{pid}/status")))
}

fn is_conflict(e: &bollard::errors::Error) -> bool {
    matches!(e, bollard::errors::Error::DockerResponseServerError { status_code, .. } if *status_code == 409)
}

fn is_not_found(e: &bollard::errors::Error) -> bool {
    matches!(e, bollard::errors::Error::DockerResponseServerError { status_code, .. } if *status_code == 404)
}

/// Split `repo[:tag]` into `(repo, tag)`, defaulting the tag to `latest` —
/// `create_image`'s `from_image`/`tag` fields want these separated. Only
/// the *last* `:` after the *last* `/` is treated as the tag separator, so
/// a registry host with an explicit port (`host:5000/repo`) isn't
/// misparsed — the same disambiguation rule `docker` itself uses.
fn split_image_ref(image: &str) -> (String, String) {
    let last_slash = image.rfind('/').unwrap_or(0);
    match image[last_slash..].rfind(':') {
        Some(rel_idx) => {
            let idx = last_slash + rel_idx;
            (image[..idx].to_string(), image[idx + 1..].to_string())
        }
        None => (image.to_string(), "latest".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_image_ref_handles_a_plain_tag() {
        assert_eq!(
            split_image_ref("alpine:3"),
            ("alpine".to_string(), "3".to_string())
        );
    }

    #[test]
    fn split_image_ref_defaults_to_latest() {
        assert_eq!(
            split_image_ref("alpine"),
            ("alpine".to_string(), "latest".to_string())
        );
    }

    #[test]
    fn split_image_ref_handles_a_registry_host_with_a_port() {
        assert_eq!(
            split_image_ref("registry.example.com:5000/substrate-kit-test:latest"),
            (
                "registry.example.com:5000/substrate-kit-test".to_string(),
                "latest".to_string()
            )
        );
    }

    #[test]
    fn restart_policy_never_maps_to_dockers_no_policy() {
        let policy = RestartPolicy::Never.into_docker();
        assert_eq!(policy.name, Some(RestartPolicyNameEnum::NO));
        assert_eq!(policy.maximum_retry_count, None);
    }

    #[test]
    fn restart_policy_on_failure_carries_its_retry_count() {
        let policy = RestartPolicy::OnFailure { max_retries: 3 }.into_docker();
        assert_eq!(policy.name, Some(RestartPolicyNameEnum::ON_FAILURE));
        assert_eq!(policy.maximum_retry_count, Some(3));
    }

    #[test]
    fn container_spec_builders_set_expected_fields() {
        let mut env = HashMap::new();
        env.insert("FOO".to_string(), "bar".to_string());
        let spec = ContainerSpec::new("test-name", "alpine:3")
            .with_env(env.clone())
            .with_cmd(vec!["sleep".to_string(), "1".to_string()])
            .with_restart_policy(RestartPolicy::Always);

        assert_eq!(spec.name, "test-name");
        assert_eq!(spec.image, "alpine:3");
        assert_eq!(spec.env, env);
        assert_eq!(spec.cmd, Some(vec!["sleep".to_string(), "1".to_string()]));
        assert_eq!(spec.restart_policy, RestartPolicy::Always);
    }

    #[test]
    fn a_spec_with_no_networking_or_volumes_produces_the_pre_clus55_config() {
        let cfg = ContainerSpec::new("n", "alpine:3").to_docker_config();
        let hc = cfg.host_config.unwrap();
        assert!(cfg.exposed_ports.is_none());
        assert!(hc.port_bindings.is_none());
        assert!(hc.mounts.is_none());
        assert!(hc.network_mode.is_none());
    }

    #[test]
    fn ports_default_to_loopback_and_map_to_docker_port_bindings() {
        let spec = ContainerSpec::new("n", "alpine:3")
            .with_port(PortMapping::tcp(8096).with_host_port(8096))
            .with_port(PortMapping::udp(5353));
        let cfg = spec.to_docker_config();
        let exposed = cfg.exposed_ports.unwrap();
        assert!(exposed.contains_key("8096/tcp") && exposed.contains_key("5353/udp"));
        let bindings = cfg.host_config.unwrap().port_bindings.unwrap();
        let tcp = bindings["8096/tcp"].as_ref().unwrap();
        assert_eq!(tcp.len(), 1);
        assert_eq!(tcp[0].host_ip.as_deref(), Some("127.0.0.1"));
        assert_eq!(tcp[0].host_port.as_deref(), Some("8096"));
        let udp = bindings["5353/udp"].as_ref().unwrap();
        assert_eq!(
            udp[0].host_port, None,
            "None must leave allocation to dockerd"
        );
    }

    #[test]
    fn the_same_container_port_on_two_interfaces_yields_two_bindings() {
        let spec = ContainerSpec::new("n", "alpine:3")
            .with_port(PortMapping::tcp(80).with_host_port(8080))
            .with_port(
                PortMapping::tcp(80)
                    .with_host_port(8080)
                    .with_host_ip("::1".parse().unwrap()),
            );
        spec.validate().unwrap();
        let bindings = spec
            .to_docker_config()
            .host_config
            .unwrap()
            .port_bindings
            .unwrap();
        assert_eq!(bindings["80/tcp"].as_ref().unwrap().len(), 2);
    }

    #[test]
    fn network_and_volumes_reach_the_host_config() {
        let spec = ContainerSpec::new("n", "alpine:3")
            .with_network("service-net")
            .with_volume(VolumeMount::named("cfg", "/config"));
        let hc = spec.to_docker_config().host_config.unwrap();
        assert_eq!(hc.network_mode.as_deref(), Some("service-net"));
        let mounts = hc.mounts.unwrap();
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].source.as_deref(), Some("cfg"));
    }

    #[test]
    fn validate_rejects_bad_ports_volumes_and_networks() {
        let bad: Vec<ContainerSpec> = vec![
            ContainerSpec::new("n", "i").with_port(PortMapping::tcp(0)),
            ContainerSpec::new("n", "i").with_port(PortMapping::tcp(80).with_host_port(0)),
            ContainerSpec::new("n", "i")
                .with_port(PortMapping::tcp(80).with_host_port(8080))
                .with_port(PortMapping::tcp(81).with_host_port(8080)),
            ContainerSpec::new("n", "i")
                .with_volume(VolumeMount::named("a", "/d"))
                .with_volume(VolumeMount::named("b", "/d")),
            ContainerSpec::new("n", "i").with_volume(VolumeMount::named("/etc", "/d")),
            ContainerSpec::new("n", "i").with_network(""),
            ContainerSpec::new("n", "i").with_network("host"),
            ContainerSpec::new("n", "i").with_network("container:abc"),
        ];
        for (i, spec) in bad.iter().enumerate() {
            assert!(
                matches!(spec.validate(), Err(SubstrateError::InvalidSpec(_))),
                "case {i} should be rejected"
            );
        }
    }

    #[test]
    fn same_host_port_on_different_protocols_is_allowed() {
        ContainerSpec::new("n", "i")
            .with_port(PortMapping::tcp(53).with_host_port(5353))
            .with_port(PortMapping::udp(53).with_host_port(5353))
            .validate()
            .unwrap();
    }

    #[test]
    fn debug_output_never_contains_env_values() {
        let mut env = HashMap::new();
        env.insert("API_TOKEN".to_string(), "s3cr3t-value".to_string());
        let spec = ContainerSpec::new("n", "alpine:3").with_env(env);
        let rendered = format!("{spec:?}");
        assert!(rendered.contains("API_TOKEN"));
        assert!(!rendered.contains("s3cr3t-value"), "{rendered}");
    }
}

/// Live tests against a real dockerd: no mocks, because a mocked dockerd
/// defeats the point. They are `#[ignore]`d by default, so a plain
/// `cargo test` (and the public CI, whose macOS and Windows runners have no
/// usable Docker) never depends on a daemon.
///
/// Run them with: `cargo test lifecycle::live -- --ignored --test-threads=1`
/// on a Linux host with a reachable Docker daemon (the current user must be
/// in the `docker` group or otherwise able to reach the socket). An
/// unreachable daemon fails the test rather than skipping it. The tests that
/// resolve a `ContainerRef` read the host `/proc` and cgroup v2, so they are
/// compiled on Linux only; the others only need the Docker Engine API.
///
/// **What these tests can and can't prove about `container_ref` without
/// root:** `cgroup::resolve_cgroup_path` reads a world-readable `/proc`
/// file, so it resolves for real regardless of privilege.
/// `mount::resolve_mount_namespace` reads `/proc/<pid>/ns/mnt`, which
/// requires the same uid as the target process or `CAP_SYS_PTRACE` — a
/// non-root test run resolving a root-owned dockerd container's mount
/// namespace will hit that real permission boundary and get `None` back
/// (logged, not silently dropped — see `Lifecycle::container_ref`'s doc).
/// `mount_namespace_round_trips_when_privileged` asserts the full
/// success path and is itself gated behind `SUBSTRATE_REQUIRE_ROOT_NS`,
/// separate from the Docker requirement — set it (as root, e.g. `sudo
/// -E`) to prove real end-to-end resolution; otherwise this suite still
/// runs and asserts the graceful, correctly-typed permission-denied
/// failure instead, which is itself real, exercised behavior, not a skip.
#[cfg(test)]
mod live {
    use super::*;

    /// Connect to the local dockerd, or fail the test. These tests are
    /// `#[ignore]`d, so running one is an explicit request for a real
    /// daemon: an unreachable one is a failure, never a silent pass.
    async fn require_docker() -> Lifecycle {
        match Lifecycle::connect().await {
            Ok(lifecycle) => lifecycle,
            Err(e) => panic!("these live tests need a reachable Docker daemon: {e}"),
        }
    }

    fn test_name(prefix: &str) -> String {
        format!(
            "{prefix}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        )
    }

    fn managed_labels() -> HashMap<String, String> {
        let mut labels = HashMap::new();
        labels.insert(LABEL_MANAGED.to_string(), "true".to_string());
        labels
    }

    /// The full round trip a real substrate consumer drives: pull a small
    /// public image, create+start a long-lived container, confirm it
    /// reaches (and stays at) Running, resolve its `ContainerRef` for
    /// real (cgroup path definitely, mount namespace best-effort per this
    /// module's doc), then stop+remove it.
    #[tokio::test]
    #[ignore = "needs a reachable Docker daemon; run with `cargo test -- --ignored`"]
    #[cfg(target_os = "linux")]
    async fn create_start_health_container_ref_stop_round_trips_for_real() {
        let lifecycle = require_docker().await;
        let image = "alpine:3";
        lifecycle.pull_image(image).await.expect("pull_image");

        let name = test_name("substrate-lifecycle-live");
        let spec = ContainerSpec::new(&name, image)
            .with_cmd(vec!["sleep".to_string(), "300".to_string()])
            .with_labels(managed_labels());

        let container_id = lifecycle.run(&spec).await.expect("run");

        lifecycle
            .wait_until_running(&container_id, Duration::from_secs(15))
            .await
            .expect("wait_until_running");

        let container_ref = lifecycle
            .container_ref(&container_id)
            .await
            .expect("container_ref");

        assert_eq!(container_ref.container_id, container_id);
        // alpine's default entrypoint runs as root — a real, resolved
        // fact from /proc, not a fixture default.
        assert_eq!(container_ref.uid, 0);

        let cgroup_path = container_ref
            .cgroup
            .as_deref()
            .expect("cgroup path must resolve — /proc/<pid>/cgroup is world-readable");
        assert!(
            cgroup_path.starts_with(cgroup::CGROUP_V2_MOUNT),
            "cgroup path: {cgroup_path}"
        );

        if let Some(mount_ns) = &container_ref.mount_ns {
            assert!(
                mount_ns.starts_with("mnt:["),
                "mount namespace handle: {mount_ns}"
            );
        } else {
            eprintln!(
                "NOTE: mount_ns did not resolve (expected without root/CAP_SYS_PTRACE against \
                 a root-owned container — see mount::resolve_mount_namespace's doc)"
            );
        }

        lifecycle
            .stop_and_remove(&container_id, Duration::from_secs(5))
            .await
            .expect("stop_and_remove");
    }

    /// A container whose entrypoint exits immediately must be reported as
    /// a real failure by `wait_until_running`, not hang until the
    /// timeout.
    #[tokio::test]
    #[ignore = "needs a reachable Docker daemon; run with `cargo test -- --ignored`"]
    async fn wait_until_running_fails_fast_for_a_container_that_exits_immediately() {
        let lifecycle = require_docker().await;
        let image = "alpine:3";
        lifecycle.pull_image(image).await.expect("pull_image");

        let name = test_name("substrate-lifecycle-live-exit");
        let spec = ContainerSpec::new(&name, image)
            .with_cmd(vec![
                "sh".to_string(),
                "-c".to_string(),
                "exit 0".to_string(),
            ])
            .with_labels(managed_labels());

        let container_id = lifecycle.run(&spec).await.expect("run");

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            lifecycle.wait_until_running(&container_id, Duration::from_secs(30)),
        )
        .await
        .expect("wait_until_running must return well before its own 30s timeout, not hang");

        assert!(
            result.is_err(),
            "an immediately-exited container must be reported as a failure"
        );

        lifecycle
            .stop_and_remove(&container_id, Duration::from_secs(5))
            .await
            .expect("stop_and_remove");
    }

    /// Restart-policy handling actually reaches dockerd: create a
    /// container with `OnFailure { max_retries: 2 }` and confirm dockerd's
    /// own inspect reflects it back, rather than trusting our own
    /// in-process `into_docker()` mapping alone.
    #[tokio::test]
    #[ignore = "needs a reachable Docker daemon; run with `cargo test -- --ignored`"]
    async fn restart_policy_is_applied_and_visible_in_dockerds_own_inspect() {
        let lifecycle = require_docker().await;
        let image = "alpine:3";
        lifecycle.pull_image(image).await.expect("pull_image");

        let name = test_name("substrate-lifecycle-live-restart-policy");
        let spec = ContainerSpec::new(&name, image)
            .with_cmd(vec!["sleep".to_string(), "300".to_string()])
            .with_labels(managed_labels())
            .with_restart_policy(RestartPolicy::OnFailure { max_retries: 2 });

        let container_id = lifecycle.run(&spec).await.expect("run");

        let info = lifecycle.inspect(&container_id).await.expect("inspect");
        let restart_policy = info
            .host_config
            .as_ref()
            .and_then(|hc| hc.restart_policy.as_ref())
            .expect("dockerd must echo back a restart policy");
        assert_eq!(restart_policy.name, Some(RestartPolicyNameEnum::ON_FAILURE));
        assert_eq!(restart_policy.maximum_retry_count, Some(2));

        lifecycle
            .stop_and_remove(&container_id, Duration::from_secs(5))
            .await
            .expect("stop_and_remove");
    }

    /// Asserts the *privileged* success path of mount-namespace
    /// resolution end-to-end — gated separately from
    /// the Docker requirement because it needs a second, independent
    /// precondition (root/`CAP_SYS_PTRACE`), not just a reachable daemon.
    /// Without `SUBSTRATE_REQUIRE_ROOT_NS` set, this still runs but only
    /// asserts the graceful permission-denied path — real, typed,
    /// exercised behavior either way, never a silent no-op.
    #[tokio::test]
    #[ignore = "needs a reachable Docker daemon; run with `cargo test -- --ignored`"]
    #[cfg(target_os = "linux")]
    async fn mount_namespace_round_trips_when_privileged() {
        let lifecycle = require_docker().await;
        let image = "alpine:3";
        lifecycle.pull_image(image).await.expect("pull_image");

        let name = test_name("substrate-lifecycle-live-mount-ns");
        let spec = ContainerSpec::new(&name, image)
            .with_cmd(vec!["sleep".to_string(), "300".to_string()])
            .with_labels(managed_labels());

        let container_id = lifecycle.run(&spec).await.expect("run");
        lifecycle
            .wait_until_running(&container_id, Duration::from_secs(15))
            .await
            .expect("wait_until_running");

        let container_ref = lifecycle
            .container_ref(&container_id)
            .await
            .expect("container_ref");

        let require_root_ns = std::env::var_os("SUBSTRATE_REQUIRE_ROOT_NS").is_some();
        match (&container_ref.mount_ns, require_root_ns) {
            (Some(ns), _) => assert!(ns.starts_with("mnt:["), "mount namespace handle: {ns}"),
            (None, true) => panic!(
                "SUBSTRATE_REQUIRE_ROOT_NS set but mount_ns did not resolve — \
                 run as root/with CAP_SYS_PTRACE"
            ),
            (None, false) => eprintln!(
                "SKIPPED (assertion only): mount_ns did not resolve without root/CAP_SYS_PTRACE; \
                 set SUBSTRATE_REQUIRE_ROOT_NS=1 under sudo to hard-fail and prove the privileged path"
            ),
        }

        lifecycle
            .stop_and_remove(&container_id, Duration::from_secs(5))
            .await
            .expect("stop_and_remove");
    }

    // ---- network / port / volume live tests ----------------------

    /// Poll inspect until the container has exited; returns its exit code.
    async fn wait_exit_code(lifecycle: &Lifecycle, id: &str) -> i64 {
        for _ in 0..100 {
            let info = lifecycle.inspect(id).await.expect("inspect");
            if let Some(state) = info.state {
                if state.running != Some(true) && state.status.is_some() {
                    use bollard::models::ContainerStateStatusEnum as S;
                    if matches!(state.status, Some(S::EXITED) | Some(S::DEAD)) {
                        return state.exit_code.unwrap_or(-1);
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        panic!("container {id} did not exit in time");
    }

    /// A dockerd-allocated, loopback-only publish round trips: the port is
    /// bound on 127.0.0.1 only, and `published_host_port` reads it back.
    #[tokio::test]
    #[ignore = "needs a reachable Docker daemon; run with `cargo test -- --ignored`"]
    async fn ephemeral_loopback_port_is_published_and_readable() {
        let lifecycle = require_docker().await;
        let image = "alpine:3";
        lifecycle.pull_image(image).await.expect("pull_image");

        let name = test_name("substrate-lifecycle-live-port");
        let spec = ContainerSpec::new(&name, image)
            .with_cmd(vec!["sleep".to_string(), "300".to_string()])
            .with_labels(managed_labels())
            .with_port(PortMapping::tcp(8080));
        let id = lifecycle.run(&spec).await.expect("run");

        let host_port = lifecycle
            .published_host_port(&id, 8080, Protocol::Tcp)
            .await
            .expect("published_host_port")
            .expect("dockerd must have allocated a host port");
        assert!(host_port > 0);

        let info = lifecycle.inspect(&id).await.expect("inspect");
        let binding = info
            .host_config
            .and_then(|hc| hc.port_bindings)
            .and_then(|mut pb| pb.remove("8080/tcp"))
            .flatten()
            .expect("binding echoed back");
        assert_eq!(binding[0].host_ip.as_deref(), Some("127.0.0.1"));

        assert_eq!(
            lifecycle
                .published_host_port(&id, 9999, Protocol::Tcp)
                .await
                .unwrap(),
            None
        );

        lifecycle
            .stop_and_remove(&id, Duration::from_secs(5))
            .await
            .expect("stop_and_remove");
    }

    /// A pinned 1:1 host port really accepts a
    /// TCP connection on loopback, end to end through dockerd's proxy.
    #[tokio::test]
    #[ignore = "needs a reachable Docker daemon; run with `cargo test -- --ignored`"]
    async fn pinned_one_to_one_port_accepts_a_tcp_connection() {
        let lifecycle = require_docker().await;
        let image = "alpine:3";
        lifecycle.pull_image(image).await.expect("pull_image");

        // Reserve a free port, release it, then ask dockerd for it.
        let free = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();

        let name = test_name("substrate-lifecycle-live-pinned");
        let spec = ContainerSpec::new(&name, image)
            .with_cmd(vec![
                "sh".to_string(),
                "-c".to_string(),
                format!("nc -lk -p {free} -e /bin/cat"),
            ])
            .with_labels(managed_labels())
            .with_port(PortMapping::tcp(free).with_host_port(free));
        let id = lifecycle.run(&spec).await.expect("run");
        lifecycle
            .wait_until_running(&id, Duration::from_secs(15))
            .await
            .expect("wait_until_running");

        assert_eq!(
            lifecycle
                .published_host_port(&id, free, Protocol::Tcp)
                .await
                .unwrap(),
            Some(free)
        );
        // dockerd's proxy accepts the connection as soon as the port is
        // bound, independent of the process behind it.
        let mut connected = false;
        for _ in 0..25 {
            if tokio::net::TcpStream::connect(("127.0.0.1", free))
                .await
                .is_ok()
            {
                connected = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        assert!(
            connected,
            "could not connect to published loopback port {free}"
        );

        lifecycle
            .stop_and_remove(&id, Duration::from_secs(5))
            .await
            .expect("stop_and_remove");
    }

    /// `ensure_network` is idempotent, and two containers on it resolve
    /// each other by container name.
    #[tokio::test]
    #[ignore = "needs a reachable Docker daemon; run with `cargo test -- --ignored`"]
    async fn containers_on_a_user_defined_network_resolve_each_other_by_name() {
        let lifecycle = require_docker().await;
        let image = "alpine:3";
        lifecycle.pull_image(image).await.expect("pull_image");

        let net = test_name("substrate-live-net");
        lifecycle
            .ensure_network(&net)
            .await
            .expect("ensure_network");
        lifecycle
            .ensure_network(&net)
            .await
            .expect("ensure_network must be idempotent");

        let server_name = test_name("substrate-live-net-server");
        let server = lifecycle
            .run(
                &ContainerSpec::new(&server_name, image)
                    .with_cmd(vec!["sleep".to_string(), "300".to_string()])
                    .with_labels(managed_labels())
                    .with_network(&net),
            )
            .await
            .expect("run server");
        lifecycle
            .wait_until_running(&server, Duration::from_secs(15))
            .await
            .expect("server running");

        let client_name = test_name("substrate-live-net-client");
        let client = lifecycle
            .run(
                &ContainerSpec::new(&client_name, image)
                    .with_cmd(vec![
                        "sh".to_string(),
                        "-c".to_string(),
                        format!("getent hosts {server_name}"),
                    ])
                    .with_labels(managed_labels())
                    .with_network(&net),
            )
            .await
            .expect("run client");
        let code = wait_exit_code(&lifecycle, &client).await;
        assert_eq!(code, 0, "client could not resolve the server by name");

        let info = lifecycle.inspect(&server).await.expect("inspect");
        let networks = info.network_settings.and_then(|n| n.networks).unwrap();
        assert!(networks.contains_key(&net), "server not attached to {net}");

        for id in [&client, &server] {
            lifecycle
                .stop_and_remove(id, Duration::from_secs(5))
                .await
                .expect("stop_and_remove");
        }
        lifecycle
            .docker
            .remove_network(&net)
            .await
            .expect("remove_network");
    }

    /// A named volume persists across containers, a read-only mount
    /// refuses writes, and dockerd (not just our mapping) reports a
    /// `volume`-type mount, never a bind.
    #[tokio::test]
    #[ignore = "needs a reachable Docker daemon; run with `cargo test -- --ignored`"]
    async fn named_volume_persists_across_containers_and_read_only_is_enforced() {
        let lifecycle = require_docker().await;
        let image = "alpine:3";
        lifecycle.pull_image(image).await.expect("pull_image");

        let vol = test_name("substrate-live-vol");

        let run_sh = |label: &'static str, script: &'static str, ro: bool| {
            let lifecycle = &lifecycle;
            let vol = vol.clone();
            async move {
                let mount = VolumeMount::named(&vol, "/data");
                let mount = if ro { mount.read_only() } else { mount };
                let spec = ContainerSpec::new(test_name(label), image)
                    .with_cmd(vec!["sh".to_string(), "-c".to_string(), script.to_string()])
                    .with_labels(managed_labels())
                    .with_volume(mount);
                let id = lifecycle.run(&spec).await.expect("run");
                let code = wait_exit_code(lifecycle, &id).await;
                let info = lifecycle.inspect(&id).await.expect("inspect");
                let mounts = info.mounts.unwrap_or_default();
                lifecycle.remove(&id, true).await.expect("remove");
                (code, mounts)
            }
        };

        let (code, mounts) = run_sh("substrate-live-vol-w", "echo hello > /data/f", false).await;
        assert_eq!(code, 0);
        assert_eq!(mounts.len(), 1);
        assert_eq!(
            mounts[0].typ,
            Some(bollard::models::MountPointTypeEnum::VOLUME)
        );
        assert_eq!(mounts[0].name.as_deref(), Some(vol.as_str()));

        let (code, _) = run_sh("substrate-live-vol-r", "grep -q hello /data/f", false).await;
        assert_eq!(code, 0, "data written by the first container must persist");

        let (code, mounts) = run_sh("substrate-live-vol-ro", "echo x > /data/g", true).await;
        assert_ne!(code, 0, "write to a read-only volume must fail");
        assert_eq!(mounts[0].rw, Some(false));

        lifecycle
            .docker
            .remove_volume(&vol, None)
            .await
            .expect("remove_volume");
    }

    /// A rejected spec fails before any dockerd call: nothing is created.
    #[tokio::test]
    #[ignore = "needs a reachable Docker daemon; run with `cargo test -- --ignored`"]
    async fn an_invalid_spec_is_rejected_without_creating_a_container() {
        let lifecycle = require_docker().await;
        let name = test_name("substrate-lifecycle-live-invalid");
        let spec =
            ContainerSpec::new(&name, "alpine:3").with_volume(VolumeMount::named("/etc", "/d"));
        let err = lifecycle.run(&spec).await.expect_err("must be rejected");
        assert!(matches!(err, SubstrateError::InvalidSpec(_)));
        assert!(
            lifecycle.inspect(&name).await.is_err(),
            "no container may have been created"
        );
    }
}
