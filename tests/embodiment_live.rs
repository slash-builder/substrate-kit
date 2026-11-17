//! End-to-end embodiment round trip against a real dockerd: start a real
//! container via `lifecycle`, resolve its `ContainerRef` via
//! `lifecycle::Lifecycle::container_ref` (which itself composes
//! `cgroup`/`mount`'s resolvers), then bind it to an
//! `identity_kit::ServicePrincipal` via `principal::embody` — proving the
//! gap `principal.rs`'s own module doc flags ("[r]esolving a real
//! `ContainerRef` from a running container is `lifecycle`'s job, in a
//! later slice") is actually closed, not just individually unit-tested
//! per module.
//!
//! `principal.rs` itself is untouched by this slice — this lives in its
//! own module-spanning integration test file rather than inside
//! `principal.rs`, since it spans `lifecycle`+`cgroup`+`mount`+`principal`
//! together and none of those modules is the more natural owner of the
//! others.
//!
//! `#[ignore]`d by default and compiled on Linux only: it needs a reachable
//! Docker daemon and reads the host `/proc` and cgroup v2. Run it with
//! `cargo test --test embodiment_live -- --ignored`. Same convention as
//! `lifecycle::live` — see that module's doc for what it can and can't
//! prove about mount-namespace resolution without root.

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::time::Duration;

use identity_kit::{Capabilities, Scope, ServicePrincipal};
use substrate_kit::lifecycle::{ContainerSpec, Lifecycle};
use substrate_kit::principal::embody;

async fn require_docker() -> Lifecycle {
    match Lifecycle::connect().await {
        Ok(lifecycle) => lifecycle,
        Err(e) => panic!("this live test needs a reachable Docker daemon: {e}"),
    }
}

#[tokio::test]
#[ignore = "needs a reachable Docker daemon on a Linux host; run with `cargo test --test embodiment_live -- --ignored`"]
async fn start_a_real_container_resolve_it_and_embody_it_end_to_end() {
    let lifecycle = require_docker().await;

    let image = "alpine:3";
    lifecycle.pull_image(image).await.expect("pull_image");

    let name = format!(
        "substrate-embodiment-live-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );
    let mut labels = HashMap::new();
    labels.insert(
        "substrate.lifecycle.managed".to_string(),
        "true".to_string(),
    );
    let spec = ContainerSpec::new(&name, image)
        .with_cmd(vec!["sleep".to_string(), "300".to_string()])
        .with_labels(labels);

    let container_id = lifecycle.run(&spec).await.expect("run");
    lifecycle
        .wait_until_running(&container_id, Duration::from_secs(15))
        .await
        .expect("wait_until_running");

    // Resolve real OS facts for the running container — not a
    // hand-constructed test fixture.
    let container_ref = lifecycle
        .container_ref(&container_id)
        .await
        .expect("container_ref");
    assert_eq!(container_ref.container_id, container_id);
    assert!(
        container_ref.cgroup.is_some(),
        "cgroup path must resolve for a real running container"
    );

    // identity-kit already owns and built this principal — substrate-kit
    // only binds it, never constructs its capabilities.
    let caps = Capabilities::new(vec![Scope("run:container".to_string())]);
    let principal = ServicePrincipal::new(None, caps);
    let principal_id = principal.id();

    let embodiment = embody(&container_ref, principal);

    assert_eq!(embodiment.principal().id(), principal_id);
    assert_eq!(embodiment.container(), &container_ref);
    assert!(embodiment.has(&Scope("run:container".to_string())));
    assert!(!embodiment.has(&Scope("admin:everything".to_string())));

    lifecycle
        .stop_and_remove(&container_id, Duration::from_secs(5))
        .await
        .expect("stop_and_remove");
}
