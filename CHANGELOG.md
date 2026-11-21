# Changelog

All notable changes to **slashbuilder-substrate-kit** will be documented in this file.

The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1](https://github.com/slash-builder/substrate-kit/compare/v0.1.0...v0.1.1) - 2026-11-21

### Other

- document the public API; forbid unsafe code ([#6](https://github.com/slash-builder/substrate-kit/pull/6))
- *(deps)* Bump thiserror from 1.0.69 to 2.0.21 ([#4](https://github.com/slash-builder/substrate-kit/pull/4))

First public release. Status is alpha: `lifecycle`, `cgroup` (read-only), `mount` and
`principal` are real; `socket` is a stub with a design note.

### Added

- Public module surface: `lifecycle`, `cgroup`, `mount`, `principal`, `socket`.
- `lifecycle`: dockerd-backed container create, start, stop and remove, `wait_until_running`,
  restart policy, and `Lifecycle::container_ref`, which resolves a running container's uid,
  cgroup path and mount namespace.
- `lifecycle`: `ContainerSpec` carries `ports` (`PortMapping`, `Protocol`; loopback by default),
  `volumes` and `network`, with `with_port(s)`, `with_volume(s)`, `with_network` builders and
  `ContainerSpec::validate` (called by `Lifecycle::run`). `Lifecycle::ensure_network` and
  `Lifecycle::published_host_port`.
- `cgroup`: resolves a process's cgroup v2 path.
- `mount`: mount-namespace resolution, and `VolumeMount` / `VolumeDriver` for named volumes,
  optionally plugin-backed. No bind mounts: `VolumeMount::validate` rejects the `local` driver
  and the driver option keys `device`, `o` and `type` (case-insensitive), since
  `local` + `type=none,o=bind,device=/host/path` is a host bind mount.
- `principal::{ContainerRef, Embodiment, embody}`: binds a running container's OS-level facts to an
  already-defined `identity_kit::ServicePrincipal`; the crate never constructs capabilities or a
  principal definition of its own.
- `error`: `SubstrateError`, `#[non_exhaustive]`, with an `InvalidSpec` variant.
- `ContainerSpec`'s `Debug` prints env keys only, never values.

### Changed

- The package is published as `slashbuilder-substrate-kit`; the library is still imported as
  `substrate_kit`. The identity dependency is `slashbuilder-identity-kit` 0.2 from crates.io
  (imported as `identity_kit`). Minimum supported Rust is 1.88.
- The live-Docker tests (`lifecycle::live`, `tests/embodiment_live.rs`) are now `#[ignore]`d and run
  with `cargo test -- --ignored`; an unreachable daemon fails them instead of skipping them. The
  tests that read the host `/proc` are compiled on Linux only. `SUBSTRATE_REQUIRE_DOCKER` is gone.

### Removed

- The unused `nix` dependency, so the crate builds on Windows.

[Unreleased]: https://github.com/slash-builder/substrate-kit/commits/main
