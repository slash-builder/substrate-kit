# Contributing to Substrate Kit

Thanks for your interest. Contributions are welcome: bug reports, feature requests, documentation improvements and code patches.

## Quick reference

| Item | Value |
|---|---|
| Licence | Apache-2.0 |
| Sign-off | `git commit -s` (Developer Certificate of Origin) |
| Required approver | A code owner: see [`.github/CODEOWNERS`](.github/CODEOWNERS) |
| CI | GitHub Actions, on every pull request |
| Conduct | [Contributor Covenant 2.1](CODE_OF_CONDUCT.md); reports to `conduct@slashbuilder.com` |
| Security | [SECURITY.md](SECURITY.md); never a public issue |

## Before you start

Open an issue first for substantial design changes: new public API surfaces, breaking changes, new dependencies. Small changes (typos, doc fixes, small bug fixes) can go straight to a pull request.

### Kit-specific invariants

The substrate is the trust boundary every dockerd-backed service on the box runs inside. Two things warrant a design discussion before code:

- The `ServicePrincipal` embodiment resolves to Identity Kit's type verbatim. Substrate Kit never defines its own principal shape, only maps a running container or process to the one Identity Kit already owns.
- Board-agnosticism: nothing in this crate may assume specific hardware. Any Docker-capable Linux host qualifies.

The public surface is alpha and may change until the Kit is locked.

## Local development

```sh
cargo build
cargo test                 # no Docker needed
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
```

Requires Rust 1.88 or newer.

### Live-Docker tests

Some tests talk to a real Docker daemon (no mocks). They are `#[ignore]`d, so `cargo test` never needs a daemon and the macOS and Windows CI runners, which have none, cannot fail on them. To run them, on a host with a reachable dockerd (your user in the `docker` group, or equivalent):

```sh
cargo test --lib live -- --ignored --test-threads=1       # lifecycle, ports, networks, volumes
cargo test --test embodiment_live -- --ignored            # end-to-end embodiment round trip
```

An unreachable daemon fails these tests instead of skipping them. The tests that resolve a `ContainerRef` read the host `/proc` and need Linux with cgroup v2; they are compiled on Linux only. They pull `alpine:3` from Docker Hub. Set `SUBSTRATE_REQUIRE_ROOT_NS=1` and run as root (or with `CAP_SYS_PTRACE`) to also assert the privileged mount-namespace path.

## Code style

- `cargo fmt --all` must produce no diff.
- `cargo clippy --all-targets --all-features -- -D warnings` must pass with zero warnings.

## Continuous integration

Every pull request runs formatting, clippy, docs, tests on Linux (x86-64 and arm64), macOS and Windows, the minimum-supported-Rust check, `cargo-deny` (advisories, licences, sources), a hygiene check, and a DCO check. The live-Docker tests are not part of CI.

## Pull request process

1. Fork the repository and create a branch from `main`.
2. Make your changes and sign off each commit with `git commit -s`.
3. Run the commands above and confirm they pass; run the live tests if you touched dockerd behaviour.
4. Open a pull request against `main` and address review feedback. A code owner must approve before merge.
5. The maintainer merges (squash by default).

## Sign your commits (DCO)

This project uses the [Developer Certificate of Origin 1.1](https://developercertificate.org/).
By adding a `Signed-off-by` line you certify that you wrote the change or have the right to submit it
under the project's licence (Apache-2.0).

```sh
git commit -s -m "your message"      # adds: Signed-off-by: Your Name <you@example.com>
git rebase --signoff main            # add the line to commits you already made
```

Use your real name and a working email address. A check on every pull request fails if any
non-merge commit is missing the line.

Under section 5 of the Apache License 2.0, a contribution you submit is licensed under the same
terms as the project unless you state otherwise. There is no separate contributor agreement.

## Code of Conduct

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md). Be kind, assume good intent, and report problems to **conduct@slashbuilder.com**.

## About

Substrate Kit is part of the **SlashBuilder Kit ecosystem**. See the [README](README.md) for the project overview.
