//! `ade-daemon attach <session-id>` — the interactive terminal client.
//!
//! The deliberately boring counterpart to the daemon: it owns no session, no
//! scrollback and no policy. It connects to the daemon, attaches, and then does
//! nothing but move bytes — daemon → stdout, stdin → daemon — with the local
//! tty in raw mode so that the *session's* terminal, and not this process's
//! line discipline, is what interprets the keystrokes.
//!
//! *Where* the daemon is comes in two shapes ([`DaemonAddress`], `--socket` or
//! `--tcp`), because the ssh forward's local end does: a Unix socket where the
//! ssh client can bind one, a loopback port where it cannot. That is the only
//! difference — the frames, the pumps and the tty handling are identical, and
//! the connect is the one place that knows.
//!
//! Three rules shape it:
//!
//! - **It never starts a daemon.** Start-if-absent is [`crate::proxy`]'s one
//!   piece of policy, and it exists because a proxy has a client behind it that
//!   wants a host. An attach to a daemon that is not running has nothing to
//!   attach to, so it says so and exits nonzero rather than booting an empty
//!   daemon.
//! - **Its death is a detach.** Closing the terminal kills this process; the
//!   connection drops, the daemon unsubscribes it, and the session and its pty
//!   carry on. That is "closing detaches, never kills", enforced by this client
//!   having no way to kill anything.
//! - **The terminal is restored however it ends.** `RawMode` restores what it
//!   saved from `Drop`, which covers the error paths and a panic alike.
//!
//! One connection, one writer: [`Frame::Write`] and [`Frame::Resize`] are
//! queued on a channel and written by a single task, because two tasks writing
//! frames to one stream could interleave a length prefix with somebody else's
//! payload. The connect hands back the two halves already separated — on a
//! socket, two clones of one fd.
//!
//! Everything above is platform-neutral, and so is the code for it. Only the
//! local terminal is not: a Unix tty is termios plus SIGWINCH ([`tty`]), a
//! Windows console is a pair of console modes plus a size poll ([`console`]),
//! and each module hands the same two things back — a `RawMode` guard and a
//! `terminal_size()`. This client runs on Windows precisely because the *host*
//! side does not: the daemon lives on a Unix box, and what a Windows Zed runs
//! is this client against the local end of a TCP-mode forward.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use ade_session::client::{Connection, PRE_CUT_DIAGNOSIS, is_handshake_eof};
use ade_session::framing::{ReadFrameError, bounded, rejection_frame};
use ade_session::proto::{Frame, Hello, SessionId, error_code};
use anyhow::{Context as _, Result, anyhow};
use smol::Unblock;
use smol::channel::{Receiver, Sender};
use smol::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use smol::net::TcpStream;
#[cfg(unix)]
use smol::net::unix::UnixStream;

#[cfg(windows)]
use self::console::{RawMode, terminal_size};
#[cfg(unix)]
use self::tty::{RawMode, terminal_size};
#[cfg(unix)]
use crate::server::LEGACY_GENERATION;
/// The window's lower end, as the client sees it. On Windows there is no
/// `server` module to take it from — this build has no daemon there.
#[cfg(windows)]
const LEGACY_GENERATION: u32 = ade_session::proto::MIN_GENERATION;

/// Bytes read from the local tty in one go.
const INPUT_CHUNK_BYTES: usize = 8192;

/// At most half a MiB of keystrokes waits in memory while a transport is down.
const OUTBOUND_QUEUE_FRAMES: usize = 64;

/// How long to wait before the one retry a handshake that ended in EOF gets.
///
/// Short enough that a user waits on it without noticing, long enough that a
/// daemon in the middle of binding its socket, or a transport that just
/// dropped a connection, has moved on by the time the second attempt lands
/// (`docs/ade/protocol-compatibility.md` §6.1).
pub(crate) const HANDSHAKE_RETRY_DELAY: Duration = Duration::from_millis(200);

/// How long a live attach waits between attempts while its transport is down.
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

/// A replay describes a fresh terminal, so re-establish that baseline first.
const RESET_TERMINAL: &[u8] = b"\x1bc";

/// The two halves of a connection, back together as the one duplex stream
/// [`Connection::handshake`] needs. A join, not an adapter: reads go to the
/// reader, writes to the writer, nothing buffered or interpreted between.
/// Both halves are `Unpin`, so the projection is a plain `Pin::new` — no
/// `unsafe` here.
pub struct Duplex<R, W> {
    reader: R,
    writer: W,
}

impl<R, W> Duplex<R, W> {
    pub fn new(reader: R, writer: W) -> Self {
        Self { reader, writer }
    }

    /// Take the halves back, so the caller can go on using them separately —
    /// which is the whole reason this type is temporary.
    pub fn into_halves(self) -> (R, W) {
        (self.reader, self.writer)
    }
}

impl<R: AsyncRead + Unpin, W: Unpin> AsyncRead for Duplex<R, W> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.reader).poll_read(context, buffer)
    }
}

impl<R: Unpin, W: AsyncWrite + Unpin> AsyncWrite for Duplex<R, W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.writer).poll_write(context, buffer)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.writer).poll_flush(context)
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.writer).poll_close(context)
    }
}

/// Whether a failed handshake carries §6.1's pre-cut signature: EOF with
/// nothing read.
///
/// [`Connection::handshake`] answers with an `anyhow::Error`, but the
/// [`ReadFrameError`] is still in its chain — so the shared predicate is what
/// decides, and this side never grows a second opinion about what a pre-cut
/// daemon looks like.
pub(crate) fn handshake_ended_in_eof(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<ReadFrameError>()
            .is_some_and(is_handshake_eof)
    })
}

/// Park a blocking thread. `smol::Timer` is disallowed by the workspace lints.
pub(crate) async fn sleep(duration: Duration) {
    smol::unblock(move || std::thread::sleep(duration)).await;
}

/// Answer, or log, a frame the daemon sent that this build could not decode.
///
/// A daemon sending an op or a body this client cannot read is a §3.3
/// violation, and the contract for it is the same in both directions: reply
/// with the rejection frame when there is a `rid` to echo, otherwise say so on
/// stderr and keep pumping. Nothing here ends the connection — that is the
/// whole repeal (§2).
fn report_undecodable(error: &ReadFrameError) -> Option<Frame> {
    match rejection_frame(error) {
        Some(reply) => Some(reply),
        None => {
            eprintln!("[ade: ignoring a frame this client cannot read: {error}]");
            None
        }
    }
}

/// How often the Windows client looks for a console resize, there being no
/// SIGWINCH to be told by. Fast enough that a drag reflows the session while
/// the mouse is still moving, cheap enough to be nothing: two Win32 calls.
#[cfg(windows)]
const RESIZE_POLL: std::time::Duration = std::time::Duration::from_millis(200);

/// Where the daemon is listening, as *this* process can reach it.
///
/// A Unix socket is the local daemon's own socket, and equally the local end of
/// a Unix client's `ssh -L` forward. A loopback address is the local end of a
/// TCP-mode forward, which is what a Windows client gets — its ssh cannot bind
/// a Unix socket, so the app hands this client an address instead. Nothing else
/// changes: the same frames over a different byte pipe.
#[derive(Clone, Debug)]
pub enum DaemonAddress {
    Socket(PathBuf),
    Tcp(String),
}

impl std::fmt::Display for DaemonAddress {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Socket(path) => write!(formatter, "{}", path.display()),
            Self::Tcp(address) => write!(formatter, "{address}"),
        }
    }
}

/// Which session to attach to, and where the daemon is listening.
#[derive(Clone, Debug)]
pub struct AttachConfig {
    pub address: DaemonAddress,
    pub session_id: SessionId,
    /// Which terminal view this client is the pty for, from `--view-id`. The
    /// daemon needs it to honour a focus naming this view; without one the
    /// client is simply never the focus owner.
    pub view_id: Option<String>,
}

impl AttachConfig {
    pub fn new(socket_path: impl Into<PathBuf>, session_id: impl Into<SessionId>) -> Self {
        Self {
            address: DaemonAddress::Socket(socket_path.into()),
            session_id: session_id.into(),
            view_id: None,
        }
    }

    /// The same client against a forwarded loopback port — `127.0.0.1:<port>`.
    pub fn tcp(address: impl Into<String>, session_id: impl Into<SessionId>) -> Self {
        Self {
            address: DaemonAddress::Tcp(address.into()),
            session_id: session_id.into(),
            view_id: None,
        }
    }

    pub fn with_view_id(mut self, view_id: Option<String>) -> Self {
        self.view_id = view_id;
        self
    }
}

/// Attach, pump until the session ends, and restore the tty.
///
/// Returns `Ok(())` for every ordinary ending — the session exited or it was
/// killed from elsewhere. A transport failure reconnects to the same session;
/// only an initial attach failure is returned to the caller.
pub async fn run(config: AttachConfig) -> Result<()> {
    match config.address.clone() {
        // The connect is handed over as a closure rather than a stream because
        // §6.1's retry needs a *second* one: a daemon that dropped the
        // connection mid-handshake left nothing to try again on. It answers
        // with the two halves rather than one stream; on a socket they are two
        // clones of the one connection, which is what they always were.
        #[cfg(unix)]
        DaemonAddress::Socket(path) => {
            attached(config, move || {
                let path = path.clone();
                async move {
                    let stream = UnixStream::connect(&path).await?;
                    Ok((stream.clone(), stream))
                }
            })
            .await
        }
        // The variant stays on every platform so the type is not two types;
        // only the connect is impossible here. `main` refuses `--socket` before
        // this is reachable, so this is the backstop for a caller that built
        // the config directly.
        #[cfg(windows)]
        DaemonAddress::Socket(path) => Err(anyhow::anyhow!(
            "cannot attach through the Unix socket {}: Windows has no Unix \
             sockets, so attach through --tcp <address> instead",
            path.display()
        )),
        DaemonAddress::Tcp(address) => {
            attached(config, move || {
                let address = address.clone();
                async move {
                    let stream = TcpStream::connect(address.as_str()).await?;
                    Ok((stream.clone(), stream))
                }
            })
            .await
        }
    }
}

/// Connect and handshake, with §6.1's one retry.
///
/// A handshake that ends in EOF with no reply is the signature of a daemon that
/// predates the protocol cut: it cannot decode `{"op":"hello",…}` and drops the
/// connection without writing anything. A transient failure looks identical, so
/// the first one buys a retry; the second is reported as the diagnosis, because
/// "unexpected end of file" is not something a user can act on and "replace the
/// daemon on that host" is.
#[derive(Debug)]
enum AttachFailure {
    Transport(anyhow::Error),
    Rejected { code: String, message: String },
    Fatal(anyhow::Error),
}

impl AttachFailure {
    fn from_error(error: anyhow::Error) -> Self {
        if error.chain().any(|cause| {
            cause.downcast_ref::<std::io::Error>().is_some()
                || matches!(
                    cause.downcast_ref::<ReadFrameError>(),
                    Some(ReadFrameError::Transport(_))
                )
        }) {
            Self::Transport(error)
        } else {
            Self::Fatal(error)
        }
    }

    fn into_initial_error(self, session_id: &SessionId) -> anyhow::Error {
        match self {
            Self::Transport(error) | Self::Fatal(error) => error,
            Self::Rejected { code, message } => anyhow!(
                "cannot attach to session {session_id} [{}]: {}",
                bounded(&code),
                bounded(&message)
            ),
        }
    }
}

/// The halves, plus **the generation this connection negotiated** — not the
/// one some earlier connection did. Attach is its own connection and
/// renegotiates on every reconnect, so what gates a frame here is what came
/// back from this handshake.
async fn handshaken<R, W, C, F>(
    config: &AttachConfig,
    connect: &C,
    diagnose_pre_cut: bool,
) -> std::result::Result<(R, W, u32), AttachFailure>
where
    R: AsyncRead + AsyncWrite + Unpin,
    W: AsyncRead + AsyncWrite + Unpin,
    C: Fn() -> F,
    F: Future<Output = Result<(R, W)>>,
{
    let mut retried = false;
    loop {
        let (reader, writer) = connect()
            .await
            .with_context(|| format!("no ADE session daemon is listening on {}", config.address))
            .map_err(AttachFailure::Transport)?;
        // The handshake is one frame out, one frame in, so it is the one place
        // that wants the halves as a single duplex — [`Connection::handshake`]
        // owns the §3.1 verification and this client does not get a second
        // opinion about it. The halves come back out unchanged.
        let mut connection = Connection::new(Duplex::new(reader, writer));
        let error = match connection.handshake(Hello::current()).await {
            Ok(ack) => {
                let (reader, writer) = connection.into_inner().into_halves();
                return Ok((reader, writer, ack.generation));
            }
            Err(error) => error,
        };
        if !handshake_ended_in_eof(&error) || !diagnose_pre_cut {
            return Err(AttachFailure::from_error(
                error.context("handshaking with the session daemon"),
            ));
        }
        if retried {
            return Err(AttachFailure::Fatal(
                error
                    .context(PRE_CUT_DIAGNOSIS)
                    .context("handshaking with the session daemon"),
            ));
        }
        retried = true;
        log::debug!("the handshake ended in EOF with no reply; retrying once");
        sleep(HANDSHAKE_RETRY_DELAY).await;
    }
}

/// Everything after the connect, which is the same work whatever carries it.
///
/// Generic rather than duplicated: [`Connection`] is already generic over the
/// stream, and the two halves are what the one-writer rule needs — a reader
/// half for the output pump and a writer half for the single writer task.
async fn attached<R, W, C, F>(config: AttachConfig, connect: C) -> Result<()>
where
    R: AsyncRead + AsyncWrite + Unpin,
    W: AsyncRead + AsyncWrite + Unpin,
    C: Fn() -> F,
    F: Future<Output = Result<(R, W)>>,
{
    let mut stdout = Unblock::new(std::io::stdout());
    let (mut daemon, mut writer) = connect_and_attach(&config, &connect, &mut stdout, false)
        .await
        .map_err(|error| error.into_initial_error(&config.session_id))?;

    // From here the terminal belongs to the session.
    let raw = RawMode::enable();
    let (outbound, queued) = smol::channel::bounded::<QueuedFrame>(OUTBOUND_QUEUE_FRAMES);
    let input = smol::spawn(pump_input(outbound.clone(), config.session_id.clone()));
    let resize = raw
        .is_some()
        .then(|| smol::spawn(pump_resize(outbound.clone(), config.session_id.clone())));

    let mut generation = 0;
    let mut pending = None;
    let ending: Result<String> = 'pumping: loop {
        let result = smol::future::or(
            pump_output(
                &mut daemon,
                &config.session_id,
                &mut stdout,
                &outbound,
                generation,
            ),
            pump_writer(&mut writer, &queued, &mut pending, generation),
        )
        .await;
        match result {
            PumpResult::Ended(ending) => break Ok(ending),
            PumpResult::Reconnect => {
                generation = generation.wrapping_add(1);
                if matches!(pending, Some(QueuedFrame::Connection { .. })) {
                    pending = None;
                }
                loop {
                    match connect_and_attach(&config, &connect, &mut stdout, true).await {
                        Ok(connection) => {
                            (daemon, writer) = connection;
                            break;
                        }
                        Err(AttachFailure::Transport(error)) => {
                            log::debug!(
                                "reattaching session {} after its transport closed: {error:#}",
                                config.session_id
                            );
                            sleep(RECONNECT_DELAY).await;
                        }
                        Err(AttachFailure::Rejected { code, message }) => {
                            break 'pumping Ok(rejected_ending(&code, &message));
                        }
                        Err(AttachFailure::Fatal(error)) => break 'pumping Err(error),
                    }
                }
            }
        }
    };

    drop(input);
    drop(resize);
    drop(outbound);
    // After the tty is cooked again, so the line does not stair-step.
    drop(raw);
    match ending {
        Ok(ending) => {
            eprintln!("{ending}");
            Ok(())
        }
        Err(error) => Err(error),
    }
}

/// Open one transport, attach, and paint the daemon's current screen.
async fn connect_and_attach<R, W, C, F>(
    config: &AttachConfig,
    connect: &C,
    stdout: &mut Unblock<std::io::Stdout>,
    reconnecting: bool,
) -> std::result::Result<(Connection<R>, Connection<W>), AttachFailure>
where
    R: AsyncRead + AsyncWrite + Unpin,
    W: AsyncRead + AsyncWrite + Unpin,
    C: Fn() -> F,
    F: Future<Output = Result<(R, W)>>,
{
    let (reader, writer, generation) = handshaken(config, connect, !reconnecting).await?;
    let mut daemon = Connection::new(reader);
    let mut writer = Connection::new(writer);
    if let Some((cols, rows)) = terminal_size() {
        writer
            .send(&Frame::Resize {
                session_id: config.session_id.clone(),
                cols,
                rows,
            })
            .await
            .context("sending the terminal size")
            .map_err(AttachFailure::from_error)?;
    }
    writer
        .send(&Frame::Attach {
            session_id: config.session_id.clone(),
            // Generation 2 has no views. `--view-id` is still accepted on the
            // command line — the caller cannot know what this connection will
            // negotiate — it simply does not reach the wire there.
            view_id: config
                .view_id
                .clone()
                .filter(|_| generation > LEGACY_GENERATION),
            request_id: Some(1),
        })
        .await
        .context("sending Attach")
        .map_err(AttachFailure::from_error)?;
    await_replay(
        &mut daemon,
        &mut writer,
        &config.session_id,
        stdout,
        reconnecting,
    )
    .await?;
    Ok((daemon, writer))
}

/// Wait for the attach to be answered, writing the replayed scrollback out.
async fn await_replay<R: AsyncRead + AsyncWrite + Unpin, W: AsyncRead + AsyncWrite + Unpin>(
    daemon: &mut Connection<R>,
    writer: &mut Connection<W>,
    session_id: &SessionId,
    stdout: &mut Unblock<std::io::Stdout>,
    reconnecting: bool,
) -> std::result::Result<(), AttachFailure> {
    loop {
        let frame = match daemon.recv().await {
            Ok(frame) => frame,
            // The connection is over: nothing will ever answer the attach.
            Err(ReadFrameError::Transport(error)) => {
                return Err(AttachFailure::Transport(
                    error.context("waiting for the replay"),
                ));
            }
            // A frame this build cannot read is not an answer either — but it
            // is also not a reason to give up on the attach, so it is answered
            // or logged and the wait continues.
            Err(error) => {
                if let Some(reply) = report_undecodable(&error) {
                    writer
                        .send(&reply)
                        .await
                        .context("answering an undecodable frame")
                        .map_err(AttachFailure::from_error)?;
                }
                continue;
            }
        };
        match frame {
            Frame::Replay {
                session_id: id,
                bytes,
                ..
            } if &id == session_id => {
                if reconnecting {
                    write_out(stdout, RESET_TERMINAL)
                        .await
                        .map_err(AttachFailure::Fatal)?;
                }
                return write_out(stdout, &bytes)
                    .await
                    .map_err(AttachFailure::Fatal);
            }
            Frame::Error {
                code,
                message,
                session_id: id,
                request_id: Some(1),
                ..
            } if id.as_ref().is_none_or(|id| id == session_id) => {
                return Err(AttachFailure::Rejected { code, message });
            }
            Frame::Error { code, message, .. } => {
                eprintln!(
                    "[ade: the daemon rejected a frame while attaching: {} ({})]",
                    bounded(&message),
                    bounded(&code)
                );
            }
            // Frames for another session cannot reach a connection that
            // attached to exactly one, but ignoring them is free and keeps this
            // loop honest about the protocol being multiplexed.
            _ => {}
        }
    }
}

fn rejected_ending(code: &str, message: &str) -> String {
    if code == error_code::NOT_FOUND {
        "[ade: session was killed]".to_owned()
    } else {
        format!("[ade: {} ({})]", bounded(message), bounded(code))
    }
}

enum QueuedFrame {
    Persistent(Frame),
    Connection { generation: u64, frame: Frame },
}

impl QueuedFrame {
    fn frame(&self) -> &Frame {
        match self {
            Self::Persistent(frame) | Self::Connection { frame, .. } => frame,
        }
    }

    fn is_stale(&self, generation: u64) -> bool {
        match self {
            Self::Connection {
                generation: queued, ..
            } => *queued != generation,
            Self::Persistent(Frame::Resize { cols, rows, .. }) => {
                terminal_size().is_some_and(|current| current != (*cols, *rows))
            }
            Self::Persistent(_) => false,
        }
    }
}

enum PumpResult {
    Reconnect,
    Ended(String),
}

/// Move daemon output to the terminal until the session or transport ends.
async fn pump_output<S: AsyncRead + AsyncWrite + Unpin>(
    daemon: &mut Connection<S>,
    session_id: &SessionId,
    stdout: &mut Unblock<std::io::Stdout>,
    outbound: &Sender<QueuedFrame>,
    generation: u64,
) -> PumpResult {
    loop {
        let frame = match daemon.recv().await {
            Ok(frame) => frame,
            // The daemon went away, or the socket did. The session is detached,
            // not killed, so reconnect to it through the same local endpoint.
            Err(ReadFrameError::Transport(_)) => {
                return PumpResult::Reconnect;
            }
            // One frame this build cannot read is one frame, not the end of the
            // terminal: answer what can be answered and keep the session up.
            Err(error) => {
                if let Some(reply) = report_undecodable(&error)
                    && outbound
                        .send(QueuedFrame::Connection {
                            generation,
                            frame: reply,
                        })
                        .await
                        .is_err()
                {
                    return PumpResult::Ended("[ade: terminal closed]".to_owned());
                }
                continue;
            }
        };
        if frame.session_id().is_some_and(|id| id != session_id) {
            continue;
        }
        match frame {
            Frame::Output { bytes, .. } | Frame::Replay { bytes, .. } => {
                if write_out(stdout, &bytes).await.is_err() {
                    return PumpResult::Ended("[ade: terminal closed]".to_owned());
                }
            }
            Frame::Exited { exit_code, .. } => {
                let ending = match exit_code {
                    Some(code) => format!("[ade: session exited with status {code}]"),
                    None => "[ade: session exited]".to_owned(),
                };
                return PumpResult::Ended(ending);
            }
            Frame::Removed { .. } => {
                return PumpResult::Ended("[ade: session was killed]".to_owned());
            }
            // The daemon's answer to *one frame this client sent* — a `resize`
            // it could not decode, an op this daemon does not implement. §2
            // makes those request-scoped and keeps the daemon serving, so the
            // terminal has to survive them too; ending here would let one bad
            // frame do what the envelope exists to prevent. It goes to stderr
            // rather than into the session's output, which belongs to the pty.
            Frame::Error { code, message, .. } if error_code::is_request_scoped(&code) => {
                eprintln!(
                    "[ade: the daemon rejected a frame from this client: {} ({})]",
                    bounded(&message),
                    bounded(&code)
                );
            }
            // Everything else is about the session or the connection. The code
            // goes in the line the user reads: the prose is the daemon's and
            // may be anything, the code is the part support can ask about.
            Frame::Error { code, message, .. } => {
                return PumpResult::Ended(format!(
                    "[ade: {} ({})]",
                    bounded(&message),
                    bounded(&code)
                ));
            }
            _ => {}
        }
    }
}

/// Drain the persistent input queue through one transport.
///
/// `pending` lives outside this future so stale connection-scoped replies can
/// be discarded when the transport changes.
async fn pump_writer<S: AsyncRead + AsyncWrite + Unpin>(
    writer: &mut Connection<S>,
    queued: &Receiver<QueuedFrame>,
    pending: &mut Option<QueuedFrame>,
    generation: u64,
) -> PumpResult {
    loop {
        if pending.is_none() {
            *pending = queued.recv().await.ok();
        }
        if pending
            .as_ref()
            .is_some_and(|frame| frame.is_stale(generation))
        {
            *pending = None;
            continue;
        }
        let sent = match pending.as_ref() {
            Some(frame) => {
                let frame = frame.frame().clone();
                // The protocol has no write acknowledgement, so retrying an
                // ambiguous send could execute a terminal command twice.
                *pending = None;
                writer.send(&frame).await
            }
            None => return PumpResult::Ended("[ade: terminal closed]".to_owned()),
        };
        if sent.is_err() {
            return PumpResult::Reconnect;
        }
    }
}

/// Local keystrokes → [`Frame::Write`]. Ends on EOF, which leaves the client
/// running: a closed stdin is not a closed session.
async fn pump_input(outbound: Sender<QueuedFrame>, session_id: SessionId) {
    let mut stdin = Unblock::new(std::io::stdin());
    let mut buffer = vec![0u8; INPUT_CHUNK_BYTES];
    loop {
        let read = match stdin.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let frame = Frame::Write {
            session_id: session_id.clone(),
            bytes: buffer[..read].to_vec(),
        };
        if outbound.send(QueuedFrame::Persistent(frame)).await.is_err() {
            break;
        }
    }
}

/// SIGWINCH → [`Frame::Resize`]. The initial size is sent before Attach.
#[cfg(unix)]
async fn pump_resize(outbound: Sender<QueuedFrame>, session_id: SessionId) {
    let Some(signals) = tty::winch_signals() else {
        return;
    };
    let mut signals = Unblock::new(signals);
    let mut buffer = [0u8; 64];
    loop {
        match signals.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if !send_size(&outbound, &session_id).await {
            break;
        }
    }
}

/// A changed console size → [`Frame::Resize`]. Windows has no SIGWINCH, so the
/// size is polled instead; the initial size is sent before Attach.
///
/// Only *changes* are sent. A `Resize` costs the daemon a `TIOCSWINSZ` and the
/// session a SIGWINCH, so resending the same size five times a second would
/// have every attached agent redraw for nothing.
///
/// The sleep rides smol's blocking pool rather than a detached thread, for the
/// same reason the pump is a task: dropping the task has to end the polling.
/// (`smol::Timer` is disallowed workspace-wide, hence the blocking sleep.)
#[cfg(windows)]
async fn pump_resize(outbound: Sender<QueuedFrame>, session_id: SessionId) {
    let mut last = terminal_size();
    loop {
        smol::unblock(|| std::thread::sleep(RESIZE_POLL)).await;
        let size = terminal_size();
        if size == last {
            continue;
        }
        last = size;
        if !send_size(&outbound, &session_id).await {
            break;
        }
    }
}

/// Queue the terminal's current size. `false` means the queue is closed and the
/// caller should stop.
async fn send_size(outbound: &Sender<QueuedFrame>, session_id: &SessionId) -> bool {
    let Some((cols, rows)) = terminal_size() else {
        return true;
    };
    outbound
        .send(QueuedFrame::Persistent(Frame::Resize {
            session_id: session_id.clone(),
            cols,
            rows,
        }))
        .await
        .is_ok()
}

async fn write_out(stdout: &mut Unblock<std::io::Stdout>, bytes: &[u8]) -> Result<()> {
    stdout.write_all(bytes).await.context("writing to stdout")?;
    // stdout is line buffered and pty output frequently carries no newline, so
    // an unflushed write would sit in the buffer until something else pushed a
    // newline through — i.e. the prompt would not appear.
    stdout.flush().await.context("flushing stdout")
}

/// The Unix terminal: termios raw mode and SIGWINCH.
#[cfg(unix)]
mod tty {
    use std::io::IsTerminal as _;
    use std::os::fd::{FromRawFd as _, RawFd};
    use std::sync::atomic::{AtomicI32, Ordering};

    /// stdin, as a raw fd. The tty this client owns.
    const STDIN: RawFd = 0;

    /// The tty's saved terminal settings, restored on drop.
    ///
    /// `None` from [`Self::enable`] means stdin is not a terminal — a pipe, a
    /// test harness, `< /dev/null` — in which case there is nothing to put into
    /// raw mode and nothing to restore.
    pub(super) struct RawMode {
        fd: RawFd,
        saved: libc::termios,
    }

    impl RawMode {
        pub(super) fn enable() -> Option<Self> {
            if !std::io::stdin().is_terminal() {
                return None;
            }
            // SAFETY: `STDIN` is a terminal (just checked) and `saved` is a
            // correctly-sized, zeroed termios for tcgetattr to fill in.
            unsafe {
                let mut saved: libc::termios = std::mem::zeroed();
                if libc::tcgetattr(STDIN, &raw mut saved) != 0 {
                    return None;
                }
                let mut settings = saved;
                libc::cfmakeraw(&raw mut settings);
                if libc::tcsetattr(STDIN, libc::TCSANOW, &raw const settings) != 0 {
                    return None;
                }
                Some(Self { fd: STDIN, saved })
            }
        }
    }

    impl Drop for RawMode {
        fn drop(&mut self) {
            // SAFETY: restoring settings this process saved from the same fd.
            unsafe {
                libc::tcsetattr(self.fd, libc::TCSANOW, &raw const self.saved);
            }
        }
    }

    /// Write end of the SIGWINCH self-pipe, read by [`winch_signals`].
    ///
    /// A signal handler may only call async-signal-safe functions, which rules
    /// out touching a channel or an executor. `write(2)` on a pipe is safe, so
    /// the handler writes one byte and the resize pump — an ordinary blocking
    /// reader on the other end — does the actual work.
    static WINCH_PIPE: AtomicI32 = AtomicI32::new(-1);

    extern "C" fn on_winch(_signal: libc::c_int) {
        let fd = WINCH_PIPE.load(Ordering::Relaxed);
        if fd >= 0 {
            let byte = 1u8;
            // SAFETY: a one-byte write to a pipe fd this process owns, which is
            // async-signal-safe. A full pipe just drops the notification.
            unsafe {
                libc::write(fd, std::ptr::from_ref(&byte).cast(), 1);
            }
        }
    }

    /// Install the SIGWINCH handler and hand back the readable end of its pipe.
    pub(super) fn winch_signals() -> Option<std::fs::File> {
        let mut fds = [0 as RawFd; 2];
        // SAFETY: `pipe` fills the two-element array it is given, and `signal`
        // installs a plain `extern "C"` handler.
        unsafe {
            if libc::pipe(fds.as_mut_ptr()) != 0 {
                return None;
            }
            WINCH_PIPE.store(fds[1], Ordering::Relaxed);
            let handler = on_winch as extern "C" fn(libc::c_int);
            if libc::signal(libc::SIGWINCH, handler as libc::sighandler_t) == libc::SIG_ERR {
                return None;
            }
            Some(std::fs::File::from_raw_fd(fds[0]))
        }
    }

    /// The terminal's current size, or `None` when it has none to report.
    pub(super) fn terminal_size() -> Option<(u16, u16)> {
        // SAFETY: TIOCGWINSZ fills the winsize it is given.
        let size = unsafe {
            let mut size: libc::winsize = std::mem::zeroed();
            if libc::ioctl(STDIN, libc::TIOCGWINSZ, &raw mut size) != 0 {
                return None;
            }
            size
        };
        (size.ws_col > 0 && size.ws_row > 0).then_some((size.ws_col, size.ws_row))
    }
}

/// The Windows console: the same two jobs, done with console modes.
///
/// The console this client is handed is a ConPTY that Zed owns, so the handles
/// really are console handles and `GetConsoleMode` really answers — this is not
/// a redirected pipe, which is the case [`RawMode::enable`] returns `None` for.
#[cfg(windows)]
mod console {
    use std::io::IsTerminal as _;

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Console::{
        CONSOLE_MODE, CONSOLE_SCREEN_BUFFER_INFO, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT,
        ENABLE_PROCESSED_INPUT, ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_INPUT,
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetConsoleScreenBufferInfo,
        GetStdHandle, STD_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleMode,
    };

    /// Input handling this console must stop doing, because the session's own
    /// terminal does it: echo and line editing are the pty's, and
    /// `ENABLE_PROCESSED_INPUT` would turn Ctrl+C into a console control event
    /// that kills *this* client instead of a `0x03` byte the agent sees.
    const INPUT_OFF: u32 = ENABLE_ECHO_INPUT.0 | ENABLE_LINE_INPUT.0 | ENABLE_PROCESSED_INPUT.0;

    /// …and the one thing it must start doing: deliver keys as VT escape
    /// sequences, so an ordinary byte read on stdin yields exactly what the
    /// Unix client reads off its tty. Without it a plain read gets the console's
    /// own encoding and arrow keys never reach the session.
    const INPUT_ON: u32 = ENABLE_VIRTUAL_TERMINAL_INPUT.0;

    /// Output is added to, never taken away: the session's bytes are VT, so the
    /// console has to interpret VT. `ENABLE_PROCESSED_OUTPUT` is what makes the
    /// escape processing apply at all.
    const OUTPUT_ON: u32 = ENABLE_VIRTUAL_TERMINAL_PROCESSING.0 | ENABLE_PROCESSED_OUTPUT.0;

    /// The console's saved modes, restored on drop — the Windows spelling of
    /// the saved termios, with two handles to put back instead of one.
    ///
    /// `None` from [`Self::enable`] means stdin is not a console, which is the
    /// piped case the integration tests run in.
    pub(super) struct RawMode {
        input: HANDLE,
        output: HANDLE,
        saved_input: CONSOLE_MODE,
        saved_output: CONSOLE_MODE,
    }

    impl RawMode {
        pub(super) fn enable() -> Option<Self> {
            if !std::io::stdin().is_terminal() {
                return None;
            }
            let input = std_handle(STD_INPUT_HANDLE)?;
            let output = std_handle(STD_OUTPUT_HANDLE)?;
            let saved_input = mode(input)?;
            let saved_output = mode(output)?;
            set_mode(input, CONSOLE_MODE((saved_input.0 & !INPUT_OFF) | INPUT_ON))?;
            // Half a raw console is worse than none: an input handle left in VT
            // mode with nothing reading it swallows the user's keys. So the
            // input half goes back before this reports failure.
            if set_mode(output, CONSOLE_MODE(saved_output.0 | OUTPUT_ON)).is_none() {
                let _ = set_mode(input, saved_input);
                return None;
            }
            Some(Self {
                input,
                output,
                saved_input,
                saved_output,
            })
        }
    }

    impl Drop for RawMode {
        fn drop(&mut self) {
            let _ = set_mode(self.input, self.saved_input);
            let _ = set_mode(self.output, self.saved_output);
        }
    }

    /// The console's current size, or `None` when it has none to report.
    ///
    /// `srWindow` and not `dwSize`: the buffer can be taller than the window,
    /// and what the session must be told is the visible extent — the same thing
    /// `TIOCGWINSZ` reports on Unix.
    pub(super) fn terminal_size() -> Option<(u16, u16)> {
        let output = std_handle(STD_OUTPUT_HANDLE)?;
        let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
        // SAFETY: a console handle this process owns, and a correctly-sized
        // struct for the call to fill in.
        unsafe { GetConsoleScreenBufferInfo(output, &raw mut info) }.ok()?;
        let window = info.srWindow;
        // Widened first: the fields are `i16`, and an inclusive right edge at
        // `i16::MAX` would overflow the `+ 1` in its own width.
        let cols = i32::from(window.Right) - i32::from(window.Left) + 1;
        let rows = i32::from(window.Bottom) - i32::from(window.Top) + 1;
        match (u16::try_from(cols), u16::try_from(rows)) {
            (Ok(cols), Ok(rows)) if cols > 0 && rows > 0 => Some((cols, rows)),
            _ => None,
        }
    }

    fn std_handle(which: STD_HANDLE) -> Option<HANDLE> {
        // SAFETY: no arguments beyond a constant, and the handle is borrowed
        // from the process — nothing here closes it.
        unsafe { GetStdHandle(which) }.ok()
    }

    fn mode(handle: HANDLE) -> Option<CONSOLE_MODE> {
        let mut mode = CONSOLE_MODE::default();
        // SAFETY: a handle this process owns, and a `CONSOLE_MODE` for the call
        // to fill in.
        unsafe { GetConsoleMode(handle, &raw mut mode) }.ok()?;
        Some(mode)
    }

    fn set_mode(handle: HANDLE, mode: CONSOLE_MODE) -> Option<()> {
        // SAFETY: a handle this process owns, and a mode read from it.
        unsafe { SetConsoleMode(handle, mode) }.ok()
    }
}

/// The output pump's error taxonomy, driven over a socket pair standing in for
/// the daemon. Unix-only because that is where the pair is; the pump itself is
/// the same code on both platforms.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Feed `frames` to [`pump_output`] as a daemon would, and return the one
    /// line it ends on.
    ///
    /// The writes run *beside* the pump rather than ahead of it. A socket pair
    /// holds a couple of hundred kilobytes, so sending everything first
    /// deadlocks the moment a test carries a frame bigger than that — with
    /// nothing draining the far end, `send` never returns. A real daemon writes
    /// concurrently too, so this is also the more honest fixture.
    fn ending_after(frames: Vec<Frame>) -> String {
        smol::block_on(async {
            let (client, daemon) = UnixStream::pair().expect("a socket pair");
            let feed = async {
                let mut daemon = Connection::new(daemon);
                for frame in &frames {
                    daemon.send(frame).await.expect("queueing a frame");
                }
            };
            let (outbound, _queued) = smol::channel::bounded::<QueuedFrame>(OUTBOUND_QUEUE_FRAMES);
            let mut stdout = Unblock::new(std::io::stdout());
            let mut client = Connection::new(client);
            let session_id = SessionId::new("s-1");
            let pump = pump_output(&mut client, &session_id, &mut stdout, &outbound, 0);
            match smol::future::zip(feed, pump).await.1 {
                PumpResult::Ended(ending) => ending,
                PumpResult::Reconnect => panic!("the scripted daemon disconnected"),
            }
        })
    }

    #[test]
    fn a_resize_error_before_replay_does_not_refuse_the_attach() {
        smol::block_on(async {
            let (client, daemon) = UnixStream::pair().expect("a socket pair");
            let mut feed = Connection::new(daemon);
            let mut reader = Connection::new(client.clone());
            let mut writer = Connection::new(client);
            let session_id = SessionId::new("s-1");
            let frames = [
                Frame::Error {
                    session_id: Some(session_id.clone()),
                    workspace_id: None,
                    code: error_code::INVALID_ARGUMENT.to_owned(),
                    message: "the lost session has no pty to resize".to_owned(),
                    request_id: None,
                },
                Frame::Replay {
                    session_id: session_id.clone(),
                    bytes: Vec::new(),
                    truncated: false,
                },
            ];
            let send = async {
                for frame in frames {
                    feed.send(&frame).await.expect("sending attach response");
                }
            };
            let mut stdout = Unblock::new(std::io::stdout());
            let attach = await_replay(&mut reader, &mut writer, &session_id, &mut stdout, false);

            smol::future::zip(send, attach)
                .await
                .1
                .expect("the replay still answers Attach");
        });
    }

    /// §2: `malformed_body` and `unknown_op` answer *one frame this client
    /// sent* and the daemon keeps serving after them, so the terminal must too.
    /// Both arrive shaped as `rejection_frame` builds them — `session_id: None`
    /// — which is exactly why the pump's other-session filter cannot catch them.
    #[test]
    fn a_request_scoped_error_is_a_diagnostic_and_not_the_end_of_the_terminal() {
        let ending = ending_after(vec![
            Frame::Error {
                session_id: None,
                workspace_id: None,
                code: error_code::MALFORMED_BODY.to_owned(),
                message: "malformed body for operation \"resize\"".to_owned(),
                request_id: Some(7),
            },
            // Legal with no `rid` at all, and equally not the end of anything.
            Frame::Error {
                session_id: None,
                workspace_id: None,
                code: error_code::UNKNOWN_OP.to_owned(),
                message: "unknown operation \"resize\"".to_owned(),
                request_id: None,
            },
            Frame::Exited {
                session_id: SessionId::new("s-1"),
                exit_code: Some(0),
            },
        ]);
        assert_eq!(
            ending, "[ade: session exited with status 0]",
            "a request-scoped error tore the terminal down"
        );
    }

    /// The other half of the taxonomy: an error about the session itself is
    /// still terminal, and the code is still in the line the user reads.
    #[test]
    fn an_error_about_this_session_still_ends_the_terminal() {
        let ending = ending_after(vec![Frame::Error {
            session_id: Some(SessionId::new("s-1")),
            workspace_id: None,
            code: error_code::INTERNAL.to_owned(),
            message: "the pty is gone".to_owned(),
            request_id: None,
        }]);
        assert_eq!(ending, "[ade: the pty is gone (internal)]");
    }

    /// …and the daemon chose every byte of that prose. A frame runs to
    /// `MAX_FRAME_BYTES`, so the line the user reads is bounded rather than
    /// however long the peer felt like making it.
    #[test]
    fn a_terminal_error_does_not_print_the_whole_daemon_back() {
        let ending = ending_after(vec![Frame::Error {
            session_id: Some(SessionId::new("s-1")),
            workspace_id: None,
            code: error_code::INTERNAL.to_owned(),
            message: "x".repeat(1024 * 1024),
            request_id: None,
        }]);
        assert!(
            ending.len() < 1024,
            "the ending grew with what the daemon sent: {} bytes",
            ending.len()
        );
        assert!(ending.contains('…'), "expected an elision: {ending}");
    }
}

/// What the attach client puts on the wire is gated on the generation **this**
/// connection negotiated, including after a reconnect. Over a loopback TCP pair
/// rather than a socket pair, so the same test runs on both platforms.
#[cfg(test)]
mod generation_tests {
    use ade_session::proto::HelloAck;
    use smol::net::{TcpListener, TcpStream};

    use super::*;

    /// A daemon that selects `generation`, then answers the attach with an
    /// empty replay. Returns the `view_id` the client actually sent.
    async fn scripted_daemon(stream: TcpStream, generation: u32) -> Option<String> {
        let mut daemon = Connection::new(stream);
        let request_id = match daemon.recv().await.expect("hello") {
            Frame::Hello(hello) => hello.request_id,
            other => panic!("expected hello, got {other:?}"),
        };
        daemon
            .send(&Frame::HelloAck(HelloAck {
                daemon_version: "test".to_owned(),
                protocol_version: generation,
                host_os: "test".to_owned(),
                min_generation: generation,
                max_generation: generation,
                generation,
                capabilities: Vec::new(),
                degraded: false,
                binary_hash: None,
                upgrade_ready: None,
                instance_id: None,
                request_id,
            }))
            .await
            .expect("hello_ack");
        loop {
            // A `resize` may precede the attach when the test's stdout is a
            // real console; anything else here is not this test's subject.
            match daemon.recv().await.expect("a frame from the client") {
                Frame::Attach {
                    session_id,
                    view_id,
                    ..
                } => {
                    daemon
                        .send(&Frame::Replay {
                            session_id,
                            bytes: Vec::new(),
                            truncated: false,
                        })
                        .await
                        .expect("replay");
                    return view_id;
                }
                _ => continue,
            }
        }
    }

    /// One connect-and-attach against a daemon serving `generation`.
    fn view_id_sent_at(generation: u32, reconnecting: bool) -> Option<String> {
        smol::block_on(async move {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let address = listener.local_addr().expect("the bound port").to_string();
            let serving = smol::spawn(async move {
                let (stream, _) = listener.accept().await.expect("accept");
                scripted_daemon(stream, generation).await
            });

            let config =
                AttachConfig::tcp(address.clone(), "s-1").with_view_id(Some("view-1".to_owned()));
            let connect = || {
                let address = address.clone();
                async move {
                    let stream = TcpStream::connect(&address).await?;
                    Ok((stream.clone(), stream))
                }
            };
            let mut stdout = Unblock::new(std::io::stdout());
            connect_and_attach(&config, &connect, &mut stdout, reconnecting)
                .await
                .expect("the attach was answered");
            serving.await
        })
    }

    /// `--view-id` is accepted whatever the daemon turns out to be — the caller
    /// cannot know before the handshake — but the field only reaches a peer
    /// that has the op it feeds.
    #[test]
    fn a_generation_two_daemon_is_never_sent_a_view_id() {
        assert_eq!(view_id_sent_at(LEGACY_GENERATION, false), None);
    }

    #[test]
    fn a_generation_three_daemon_is_sent_the_view_id() {
        assert_eq!(
            view_id_sent_at(3, false),
            Some("view-1".to_owned()),
            "the view the client was told to draw"
        );
    }

    /// Attach is its own connection and handshakes again on every reconnect, so
    /// the gate is re-derived there rather than remembered from the first
    /// connect — a daemon replaced under a running attach can be either one.
    #[test]
    fn a_reconnect_re_derives_the_gate_from_its_own_handshake() {
        assert_eq!(view_id_sent_at(LEGACY_GENERATION, true), None);
        assert_eq!(view_id_sent_at(3, true), Some("view-1".to_owned()));
    }
}
