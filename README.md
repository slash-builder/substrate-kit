# Substrate Kit

> The board-agnostic Linux+dockerd substrate that services run on: container lifecycle, cgroups, mounts, and the service-principal embodiment layer.

[![License: Apache 2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
![Status: alpha](https://img.shields.io/badge/status-alpha-orange.svg)

Substrate Kit is a Rust library for running containers on a single Linux host over
the local dockerd, and for binding what runs there to an identity. It owns container
and process lifecycle over dockerd, cgroup and mount-namespace resolution, declarative
named-volume and port mounts, and the *embodiment* of a service principal: resolving a
running container to the `ServicePrincipal` type that Identity Kit defines and owns.
Any Docker-capable Linux host qualifies, never a specific board.

**Audience.** Rust services that need to run, isolate and mount containers over dockerd
without depending on board-specific hardware or shelling out to the `docker` CLI.

## Status

**Alpha (0.1.x, pre-1.0).** Pin to an exact version if you depend on it; a breaking change
bumps the minor version and is noted in [CHANGELOG.md](CHANGELOG.md).

| Module | State |
|---|---|
| `lifecycle` | **Real.** dockerd-backed create/start/stop/remove, `wait_until_running`, restart policy, `container_ref`; ports, user-defined networks and named volumes; unit and live-Docker tested. |
| `cgroup` | **Real, read-only.** Resolves a process's cgroup v2 path. No cgroup creation or limits. |
| `mount` | **Real, partly.** Resolves a mount-namespace handle; `VolumeMount` declares named volumes for `lifecycle`. No bind mounts, by design: `validate()` rejects driver `local` and the `device`/`o`/`type` options, which would otherwise express one. |
| `principal` | **Real.** `embody()` binds a container to identity-kit's `ServicePrincipal` verbatim. |
| `socket` | **Stub.** One doc comment, no code. Design note in `src/socket.rs`. |

**Not here yet:** registry-pull credentials and secret delivery (security review pending),
container resource limits, label lookup and leftover cleanup, Docker `HEALTHCHECK` wiring.

## Platforms

**Linux is the supported platform** (cgroup v2, a reachable dockerd). The crate compiles on
macOS and Windows and its unit tests pass there, but `container_ref`, `cgroup` and
`mount` resolution read the Linux `/proc` filesystem and cannot work on those hosts.

## Install

```toml
[dependencies]
slashbuilder-substrate-kit = "0.1"
tokio = { version = "1", features = ["full"] }
```

The package is `slashbuilder-substrate-kit`; **the library is still imported as `substrate_kit`**
(`use substrate_kit::lifecycle::Lifecycle;`). Its identity dependency is
`slashbuilder-identity-kit`, imported as `identity_kit`. Requires Rust 1.88 or newer.

## Quick start

```rust,ignore
use std::time::Duration;
use substrate_kit::lifecycle::{ContainerSpec, Lifecycle, PortMapping};
use substrate_kit::mount::VolumeMount;

let lifecycle = Lifecycle::connect().await?;
lifecycle.ensure_network("app-net").await?;

let spec = ContainerSpec::new("media", "jellyfin/jellyfin:latest")
    .with_network("app-net")
    .with_port(PortMapping::tcp(8096).with_host_port(8096)) // loopback only
    .with_volume(VolumeMount::named("media-config", "/config"));

let id = lifecycle.run(&spec).await?;
lifecycle.wait_until_running(&id, Duration::from_secs(30)).await?;
let container = lifecycle.container_ref(&id).await?;           // Linux hosts
let embodiment = substrate_kit::embody(&container, principal); // principal: identity_kit::ServicePrincipal
```

## Scope

**In scope.**

- Container / process lifecycle over dockerd (create, start, stop, health, restart policy).
- Namespace and cgroup resource-isolation primitives.
- Mount / volume attach primitives.
- Service-principal *embodiment*: resolving a running container to an Identity Kit `ServicePrincipal`.
- Socket-hardening for AF_UNIX-based services (planned; `socket` is a stub).

**Out of scope.**

- The service-principal *definition*: that is Identity Kit. Substrate Kit only embodies it at runtime, never redefines its shape.
- Board bring-up and hardware-specific carrier support (GPIO, power sequencing, PCIe enumeration).
- Content-addressing and block/manifest storage: that is Storage Kit.

## Modules

- **`lifecycle`**: container lifecycle over dockerd (create, start, stop, health, restart policy).
- **`cgroup`**: cgroup resolution for a running process.
- **`mount`**: mount-namespace resolution and named-volume declarations.
- **`principal`**: resolves a running container to an Identity Kit `ServicePrincipal`.
- **`socket`**: AF_UNIX permission and namespace isolation (stub).

## Building and testing

```sh
git clone https://github.com/slash-builder/substrate-kit.git
cd substrate-kit
cargo test
```

`cargo test` needs no Docker daemon. The live tests, which talk to a real dockerd, are
`#[ignore]`d by default; see [CONTRIBUTING.md](CONTRIBUTING.md) to run them on a Linux host.

## Contributing

Contributions welcome under Apache-2.0, signed off under the Developer Certificate of
Origin (`git commit -s`). See [CONTRIBUTING.md](CONTRIBUTING.md). Report vulnerabilities
privately: see [SECURITY.md](SECURITY.md).

## License

Apache-2.0. See [LICENSE](LICENSE).

## About

Part of the **SlashBuilder Kit ecosystem**: a set of reusable Rust and Dart libraries built
around the BeOS-style "Kit" pattern. Each Kit owns a self-contained domain; products are
composed from Kits.

Maintained by [SlashBuilder](https://slashbuilder.com) / Douglas Lockamy.
