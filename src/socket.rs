//! `socket` — Socket-hardening: AF_UNIX permission and namespace isolation for services running on the substrate.
//!
//! **Status: stub. Nothing here is implemented.**
//!
//! ## Design note: what a service on the box needs from this module
//!
//! A typical local-admin service binds its AF_UNIX socket with a bare
//! `UnixListener::bind` and then `chmod 0600`s it afterwards. That leaves four
//! gaps this module should close once, for every AF_UNIX service on the box:
//!
//! 1. **Private bind.** A `bind_private(path, mode)` that never exposes the
//!    socket at the umask default, even for an instant: create it under a
//!    restrictive umask (or bind to a temporary name in a 0700 directory and
//!    `rename`), then set the final mode. Today there is a window between
//!    `bind` and `chmod`.
//! 2. **Safe path and stale-socket handling.** Refuse a path whose parent is
//!    world-writable (for example under `/tmp`) or is a symlink; create the
//!    parent `0700` under `$XDG_RUNTIME_DIR` by default; remove a stale file
//!    only if it is a *socket* owned by the caller and no listener answers on
//!    it (an unconditional `remove_file` is the common mistake).
//! 3. **Peer credentials.** Expose the connecting peer's `(uid, gid, pid)`
//!    (`SO_PEERCRED`, via `tokio::net::UnixStream::peer_cred`) as a plain
//!    fact, so a service can *decide* on it. Mechanics only: this crate never
//!    makes the allow/deny decision (see `principal.rs`'s hard constraint; that
//!    belongs to identity-kit).
//! 4. **Socket activation (systemd).** Adopt a listener passed as
//!    `LISTEN_FDS`/`LISTEN_PID` (fd 3+, `sd_listen_fds` semantics, `FD_CLOEXEC`
//!    set, env vars unset after reading) so a user unit can start the service
//!    on first connection.
//!
//! **Not in scope here:** passing file descriptors over a connection
//! (`SCM_RIGHTS`/UDS fd passing). Nobody has asked for it, and it widens the
//! trust surface; add it only against a named consumer.
//!
//! **Why it is not implemented:** items 1-3 are a permissions/trust boundary
//! (the exact modes, the stale-socket rule and the peer-credential contract
//! need a security review first), and item 4 has an unresolved scope
//! question. Nothing else in this crate depends on it.
