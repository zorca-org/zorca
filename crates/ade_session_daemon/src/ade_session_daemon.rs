//! The ADE session daemon: one process per host, owning PTYs.
//!
//! Its scope is deliberately tiny — **workspaces, persistence, attach and
//! status, nothing else**. It stores each workspace's layout as an opaque
//! document but never lays anything out, never multiplexes and never renders.
//! It is the most boring process in the system because it is the only one that
//! is not allowed to die.
//!
//! It does interpret terminal output, in exactly one place and for exactly one
//! reason: [`grid`] keeps the screen each PTY has painted, so that an attach
//! can replay a *repaint at the client's current size* rather than raw
//! scrollback that is only correct at the width it was produced at. What goes
//! out on the live stream after that is still forwarded uninterpreted — the
//! client's emulator is the one drawing.
//!
//! - [`server`] — the Unix-socket accept loop, handshake and request loop.
//! - [`attach`] — `ade-daemon attach <id>`: the interactive terminal client
//!   Zed's terminal runs, the way it used to run `tmux attach`.
//! - [`sessions`] — the table: workspaces and their layouts, plus the sessions
//!   inside them — spawn on a PTY, list, kill, reap, and derive status.
//! - [`grid`] — the screen each session's PTY has painted, and the repaint an
//!   attach replays instead of raw scrollback.
//! - [`state`] — workspace persistence, and session *metadata* for crash
//!   honesty.
//! - [`proxy`] — `--stdio-proxy`: the byte pump that lets a single ssh channel
//!   carry this protocol to the host's socket, plus start-if-absent — and
//!   `--ensure` ([`proxy::ensure`]), which is that start-if-absent on its own,
//!   for a client that forwards the socket with `ssh -L` instead.
//!
//! Load-bearing invariant: **a session dies only on an explicit
//! [`Frame::Kill`](ade_session::Frame::Kill)**. A client disconnecting, a
//! detach, or an ssh channel dropping must never take a PTY with it.
//!
//! The logic lives in this library rather than in `main.rs` so that the
//! integration tests can drive a real server in-process
//! ([`Server::spawn`](server::Server::spawn)).

/// The interactive client — the one mode that is *not* unix-only. The daemon
/// runs on a unix host, but the terminal attached to it may be a Windows one,
/// reaching it through a forwarded loopback port.
#[cfg(any(unix, windows))]
pub mod attach;
/// The per-session screen and the repaint an attach replays from it.
pub mod grid;
/// The stdio side of the same transport; unix-only for the same reason as
/// [`server`].
#[cfg(unix)]
pub mod proxy;
/// Unix-socket transport. Windows gets a named pipe when it gets a daemon.
#[cfg(unix)]
pub mod server;
pub mod sessions;
pub mod state;

#[cfg(any(unix, windows))]
pub use attach::{AttachConfig, DaemonAddress};
#[cfg(unix)]
pub use proxy::ProxyConfig;
#[cfg(unix)]
pub use server::{RunningServer, Server, ServerConfig};
pub use sessions::{CreateRequest, SessionTable, StatusConfig, WorkspaceRequest};
pub use state::StateStore;

/// Version reported in [`HelloAck::daemon_version`](ade_session::HelloAck) and
/// printed by `--version`.
///
/// Re-exported rather than defined here: a client that *deploys* this binary
/// has to know what version the bytes it uploads will report, and it does not
/// depend on this crate — only on `ade_session`, which both sides share. So the
/// number lives there, and this crate's own `version` field is decorative.
pub use ade_session::DAEMON_VERSION;
