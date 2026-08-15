//! Shared protocol library for the ADE session daemon.
//!
//! One daemon runs per host and owns PTYs, persistence, attach and status —
//! nothing else. This crate is the wire contract between it and its clients,
//! so it is a **leaf library**: no gpui, no Zed crates, and no executor
//! dependency (all async is expressed over `futures`' IO traits).
//!
//! - [`proto`] — message types, the generation range
//!   ([`proto::MIN_GENERATION`]..=[`proto::MAX_GENERATION`]) and the
//!   [`proto::error_code`] vocabulary.
//! - [`framing`] — 4-byte big-endian length prefix + the serde-JSON envelope
//!   `{"op","rid","body"}`; see `docs/ade/protocol-compatibility.md`.
//! - [`client`] — [`client::Connection`], frame IO and the handshake.
//! - [`transport`] — that same connection over a child process's stdio, i.e.
//!   `ssh <host> ade-daemon --stdio-proxy`.
//! - [`deploy`] — getting that binary onto a host without ever disturbing a
//!   daemon that is already running.
//! - [`source`] — where the bytes `deploy` uploads come from: a prebuilt binary
//!   the operator points at, or one built for the host out of this checkout.
//! - [`ssh`] — reaching a host: one-shot commands, and the single long-lived
//!   `ssh -L <local>:<remote.sock>` connection every channel rides on, where
//!   `<local>` is a Unix socket or a loopback port ([`ssh::LocalEndpoint`]).
//! - [`process`] — the one thing every spawn above shares: on Windows a child
//!   must not flash a console window ([`process::QuietCommand`]).
//!
//! `transport` spawns processes through `async-process`, the one dependency
//! here that does more than describe bytes. It is still not an executor and
//! still not a Zed crate, so the leaf-library rule holds.
//!
//! The rule has exactly one exception, and it is a `cfg(windows)` one: `ssh`
//! depends on `util` for the job object that bounds the forward's life to the
//! client's (`util::process::ProcessTreeGuard`) and for the directory that
//! child sits in (`util::process::stable_child_dir`). `util` is a Zed crate,
//! so the dependency is declared under `[target.'cfg(windows)'.dependencies]`:
//! nothing outside a Windows client links it, and every binary uploaded to a
//! host is the leaf library it was. The alternative was a second copy of the
//! job object code, which is a Win32 handle protocol worth having in one place.
//!
//! **v1 tradeoff:** `bytes` payloads are plain `Vec<u8>`, i.e. JSON number
//! arrays on the wire. That is roughly 3–4x the size of the raw bytes, and is
//! accepted for v1 because a frame dump stays readable with `jq`. Swapping in
//! base64 or a binary payload frame is an additive change.

pub mod client;
pub mod deploy;
pub mod framing;
pub mod process;
pub mod proto;
pub mod source;
pub mod ssh;
pub mod transport;

/// The version `ade-daemon --version` prints and
/// [`HelloAck::daemon_version`](proto::HelloAck) carries.
///
/// It lives **here**, in the crate both sides depend on, rather than in
/// `ade_session_daemon` — which re-exports this one. A client that deploys a
/// daemon has to tell [`deploy::ensure_daemon`] what version the bytes it is
/// uploading will report, and it cannot depend on the daemon crate to ask.
/// One constant, one number.
pub const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");

/// [`DAEMON_VERSION`] as something [`deploy::ensure_daemon`] can compare.
///
/// Falls back to `0.0.0` if this crate's version is ever not three integers,
/// which would make deployment treat every host as newer and touch nothing —
/// the safe direction. A test pins that it never happens.
pub fn daemon_version() -> Version {
    Version::parse(DAEMON_VERSION).unwrap_or_default()
}

pub use client::{Connection, PRE_CUT_DIAGNOSIS, is_handshake_eof};
pub use deploy::{
    DaemonEndpoint, DeployConfig, DeployOutcome, HostExec, LocalHost, Version, replace_daemon,
    sha256_hex,
};
pub use framing::{
    MAX_FRAME_BYTES, ReadFrameError, decode_frame, encode_frame, read_frame, rejection_frame,
    write_frame,
};
pub use process::QuietCommand;
pub use proto::{
    Frame, Hello, HelloAck, KNOWN_OPS, LAYOUT_SCHEMA_VERSION, LayoutDoc, LayoutNode,
    MAX_GENERATION, MIN_GENERATION, SessionId, SessionInfo, SessionStatus, SplitDir, Tab,
    WorkspaceInfo, effective_capabilities, error_code, select_generation, validate_capabilities,
};
pub use source::{DAEMON_BINARY_ENV, HostOs, HostPlatform, daemon_binary};
pub use ssh::{
    EnsureOutcome, HostForward, LOOPBACK_ADDRESS, LocalEndpoint, SSH_BASE_FLAGS, SshHost,
    ensure_remote_daemon,
};
pub use transport::ChildConnection;

#[cfg(test)]
mod tests {
    use super::*;

    /// The one thing the fallback in [`daemon_version`] must never have to do.
    #[test]
    fn the_shared_daemon_version_parses() {
        assert_eq!(
            Version::parse(DAEMON_VERSION),
            Some(daemon_version()),
            "DAEMON_VERSION {DAEMON_VERSION:?} is not three integers"
        );
        assert_ne!(daemon_version(), Version::default());
    }
}
