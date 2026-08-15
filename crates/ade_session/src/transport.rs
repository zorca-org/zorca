//! A [`Connection`] that speaks frames over a child process's stdio.
//!
//! This is the client end of `--stdio-proxy`. Locally the child is the daemon
//! binary itself; remotely it is `ssh <host> ~/.ade/bin/ade-daemon
//! --stdio-proxy`, and the ssh channel carries exactly the same frames a Unix
//! socket would. One child per host, not one per session — frames are
//! session-tagged, so a single connection multiplexes everything.
//!
//! The child's **stderr is never part of the stream**. It is inherited by
//! default, so ssh's own diagnostics ("Permission denied", "Host key
//! verification failed") land on the client's stderr instead of being parsed
//! as a frame length. Redirect it with [`ChildConnection::spawn_command`] if
//! it should go somewhere else.

use std::pin::Pin;
use std::process::{ExitStatus, Stdio};
use std::task::{Context, Poll};

use anyhow::{Context as _, Result, bail};
use async_process::{Child, ChildStdin, ChildStdout, Command};
use futures::io::{AsyncRead, AsyncWrite};

use crate::client::Connection;
use crate::framing::ReadFrameError;
use crate::process::QuietCommand as _;
use crate::proto::{Frame, Hello, HelloAck};

/// The duplex half of a child process: read from its stdout, write to its
/// stdin.
pub struct ChildStdio {
    stdin: ChildStdin,
    stdout: ChildStdout,
}

impl AsyncRead for ChildStdio {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.stdout).poll_read(cx, buf)
    }
}

impl AsyncWrite for ChildStdio {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.stdin).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stdin).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stdin).poll_close(cx)
    }
}

/// A [`Connection`] plus the child process carrying it.
///
/// Dropping this drops the child handle without killing it; the pipes close,
/// which is what a proxy reads as "the client is gone". That is a *detach*, and
/// the daemon on the far side keeps every session it owns.
pub struct ChildConnection {
    connection: Connection<ChildStdio>,
    child: Child,
}

impl ChildConnection {
    /// Spawn `argv` and frame over its stdio. `argv[0]` is the program.
    pub fn spawn(argv: &[String]) -> Result<Self> {
        let Some((program, arguments)) = argv.split_first() else {
            bail!("an empty argv cannot be spawned");
        };
        let mut command = Command::new(program);
        command.args(arguments);
        Self::spawn_command(command)
    }

    /// Spawn a pre-configured command. Its stdin and stdout are overridden with
    /// pipes — they are the transport — and it is made
    /// [`quiet`](crate::process::QuietCommand::quiet), because a GUI client
    /// spawning this must not flash a console window on Windows. Everything
    /// else is left alone.
    pub fn spawn_command(mut command: Command) -> Result<Self> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .quiet()
            .spawn()
            .context("spawning the daemon transport")?;
        let stdin = child.stdin.take().context("child has no stdin pipe")?;
        let stdout = child.stdout.take().context("child has no stdout pipe")?;
        Ok(Self {
            connection: Connection::new(ChildStdio { stdin, stdout }),
            child,
        })
    }

    pub async fn handshake(&mut self, hello: Hello) -> Result<HelloAck> {
        self.connection.handshake(hello).await
    }

    pub async fn send(&mut self, frame: &Frame) -> Result<()> {
        self.connection.send(frame).await
    }

    /// Read one frame, keeping [`ReadFrameError`]'s split intact: a proxy's
    /// stdio is exactly where a pre-cut daemon shows up as EOF, and flattening
    /// that into one opaque error is what §6.1's diagnosis needs to tell apart.
    pub async fn recv(&mut self) -> std::result::Result<Frame, ReadFrameError> {
        self.connection.recv().await
    }

    pub fn connection(&mut self) -> &mut Connection<ChildStdio> {
        &mut self.connection
    }

    pub fn child(&mut self) -> &mut Child {
        &mut self.child
    }

    /// Close stdin — the far end's EOF — and wait for the child to exit.
    ///
    /// stdout is held open until the child is gone so that a proxy flushing its
    /// last bytes gets to finish rather than dying on `EPIPE`.
    pub async fn shutdown(self) -> Result<ExitStatus> {
        let Self {
            connection,
            mut child,
        } = self;
        let ChildStdio { stdin, stdout } = connection.into_inner();
        drop(stdin);
        let status = child.status().await.context("waiting for the transport")?;
        drop(stdout);
        Ok(status)
    }
}
