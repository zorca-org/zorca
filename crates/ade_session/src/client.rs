//! A thin, deliberately dumb connection to the session daemon.
//!
//! [`Connection`] owns a duplex stream and does nothing but frame IO plus the
//! opening handshake. It spawns no tasks and routes no events — request
//! correlation and event fan-out belong to the `DaemonBackend` layer that
//! sits above this, and do not exist yet.
//!
//! Frames go out and come back as the protocol envelope described in
//! `docs/ade/protocol-compatibility.md` and implemented by [`crate::framing`].
//! The one compatibility decision made here is §3.1's client half: the daemon
//! *selects* the generation, and this side *verifies* it — anything else is
//! still the caller's policy.

use anyhow::{Result, bail};
use futures::io::{AsyncRead, AsyncWrite};

use crate::framing::{ReadFrameError, bounded, bounded_debug, read_frame, write_frame};
use crate::proto::{Frame, Hello, HelloAck, MAX_GENERATION, MIN_GENERATION};

/// What a handshake that ends in EOF with nothing read most likely means.
///
/// Deliberately "most likely" and not certain: a post-cut daemon that crashes
/// deterministically inside its own handshake, or a transport that drops the
/// connection, produces the same signature. The retry-once rule (§6.1) filters
/// out the transient cases; this string is what is reported after the second
/// identical EOF, in place of a generic IO error nobody can act on.
pub const PRE_CUT_DIAGNOSIS: &str =
    "the daemon on this host most likely predates the protocol cut and should be replaced";

/// Whether a failed read is the pre-cut signature: the peer closed without
/// answering.
///
/// A pre-cut daemon cannot decode `{"op":"hello",…}` at all, and its receive
/// loop drops the connection without sending anything — so the client sees EOF
/// where it expected an ack. Only a transport failure can be that; a malformed
/// or unknown-op reply means something did answer.
pub fn is_handshake_eof(error: &ReadFrameError) -> bool {
    let ReadFrameError::Transport(error) = error else {
        return false;
    };
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::UnexpectedEof)
    })
}

/// Frame-level transport over any duplex byte stream — a Unix socket, a
/// named pipe, or the stdio of an `ssh` child process.
pub struct Connection<S> {
    stream: S,
}

impl<S> Connection<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    pub fn new(stream: S) -> Self {
        Self { stream }
    }

    /// Send `hello` and read the reply, which must be the daemon's
    /// [`HelloAck`]. A [`Frame::Error`] reply is surfaced as an error; so is
    /// any other frame, since `Hello` must be answered before anything else.
    ///
    /// The ack's `generation` is **verified** against this build's
    /// `[MIN_GENERATION, MAX_GENERATION]`. The daemon selects it and this side
    /// checks that it landed inside the client's own range (§3.1): a daemon
    /// answering with a generation the client cannot speak is a failed
    /// negotiation, and continuing on it would decode nonsense one frame later
    /// instead of failing here with both ranges named. Everything else about
    /// the ack — `degraded`, `capabilities`, `upgrade_ready` — is returned
    /// as-is and remains the caller's policy.
    pub async fn handshake(&mut self, hello: Hello) -> Result<HelloAck> {
        self.send(&Frame::Hello(hello)).await?;
        match self.recv().await? {
            Frame::HelloAck(ack) => {
                if ack.generation < MIN_GENERATION || ack.generation > MAX_GENERATION {
                    bail!(
                        "daemon {} selected protocol generation {}, outside this client's \
                         supported range {MIN_GENERATION}..={MAX_GENERATION} (the daemon offers \
                         {}..={})",
                        bounded(&ack.daemon_version),
                        ack.generation,
                        ack.min_generation,
                        ack.max_generation,
                    );
                }
                Ok(ack)
            }
            Frame::Error { code, message, .. } => {
                bail!(
                    "daemon rejected handshake [{}]: {}",
                    bounded(&code),
                    bounded(&message)
                )
            }
            // The daemon's mirror of this bails through `bounded_debug` for the
            // same reason: a frame is up to MAX_FRAME_BYTES, and Debug inflates
            // it. None of these three reach the wire — they become `anyhow`
            // errors that a log or a banner shows — but a peer still chose
            // every byte in them.
            other => bail!("expected HelloAck, got {}", bounded_debug(&other)),
        }
    }

    pub async fn send(&mut self, frame: &Frame) -> Result<()> {
        write_frame(&mut self.stream, frame).await
    }

    /// Read one frame. The error taxonomy is [`ReadFrameError`]: only
    /// [`ReadFrameError::Transport`] means the connection is over.
    pub async fn recv(&mut self) -> std::result::Result<Frame, ReadFrameError> {
        read_frame(&mut self.stream).await
    }

    pub fn into_inner(self) -> S {
        self.stream
    }
}
