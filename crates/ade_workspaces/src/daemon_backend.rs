//! The ADE session daemon behind the [session-backend seam](crate::SessionBackend)
//! — the default implementation since 2026-08-03, and the end of the tmux era
//! (anamnesis marks #821 / #822 / #824).
//!
//! **One control connection per backend, not one per call.** Every method here
//! rides a single framed connection to the host's daemon, opened on first use
//! and reopened if it breaks. Locally that connection is `ade-daemon
//! --stdio-proxy` over the daemon binary's own stdio; for a remote host it is a
//! channel on that host's single `ssh -L` forward. Either way it is *one*,
//! which is the constant-connections-per-host invariant.
//!
//! **One backend per host, and the backend owns the host's ssh connection.**
//! [`DaemonBackend::remote`] builds an endpoint whose address is a local one
//! that an `ssh -L` forward makes point at the host's daemon socket — a Unix
//! socket where the ssh client can bind one, a loopback port where it cannot
//! (Windows). The choice is made when the endpoint is built, because the attach
//! argv names the address and is handed out before the forward exists. Bringing
//! that up is strictly ordered: `ade-daemon --ensure` over a short-lived ssh
//! command **first**, and only then the forward — because a forward whose far
//! end nobody has bound establishes happily and fails one channel at a time
//! with a bare EOF (pinned by `ade_session`'s loopback tests). The forward is
//! re-established lazily, on the first operation after it dies.
//!
//! **A host with no daemon binary gets one.** If that first `--ensure` comes
//! back [`EnsureOutcome::NotInstalled`] — and only then — [`HostLink`] probes
//! the host's platform, obtains a daemon binary for it (`ade_session::source`:
//! `ADE_COPY_DAEMON`, else a release build out of this checkout), deploys it
//! under `ade_session::deploy`'s never-disturb-a-running-daemon policy, and
//! retries `--ensure` once. First connect to a fresh host can therefore take
//! minutes on a cold cross-compile; every call here is blocking and runs off
//! the UI thread, and the deployment steps log at `info` so `Zed.log` shows
//! what it is doing.
//!
//! **Two id namespaces, joined here.** The daemon mints its own opaque session
//! ids (uuids), while everything above this seam names a session by its
//! *workspace* — also daemon-minted, by
//! [`SessionBackend::create_workspace`], and cached in the registry. This
//! backend keeps the workspace id as the seam id and passes it to the daemon as
//! each session's `workspace_id`. Resolving one to the other is a listing.
//!
//! **Attach is still an argv**, and deliberately. Locally, or over a host's
//! forward, it names *our own* client: `ade-daemon attach <id> --socket <path>`
//! (or `--tcp <address>`, or on Windows nothing at all — see [`Address`]),
//! which Zed's terminal spawns the way it used to spawn `tmux attach`. Closing
//! the terminal kills the client, which is a detach, and the session survives
//! it. See the seam's module docs for why the stream-shaped attach waits for
//! the remote transport.
//!
//! **A dead process is not a live session.** [`Self::list`] and [`Self::exists`]
//! report only sessions the daemon has not seen exit, so a workspace whose agent
//! died reads as disconnected upstairs — including the `(lost)` rows a
//! restarted daemon reports, whose ptys really are unrecoverable. Nothing is
//! hidden: the exited row is still in the daemon's own listing, and
//! [`Self::kill`] takes it with the rest.

use crate::{
    Attached, BackendWorkspace, DaemonEvent, DaemonFreshnessObserver, LayoutEvent, SessionBackend,
    SessionChange, SessionId, SessionInfo, SessionSpec, StatusDelivery, StatusEvent,
    WorkspaceLayout, WorkspaceListing, WorkspaceStatus,
};
use ade_session::{
    EnsureOutcome, LOOPBACK_ADDRESS, LayoutDoc, LocalEndpoint, PRE_CUT_DIAGNOSIS, ReadFrameError,
    deploy::{DEFAULT_SOCKET_PATH, DEFAULT_STATE_DIR, DaemonEndpoint},
    framing::{bounded, bounded_debug},
    is_handshake_eof,
    proto::{self, Frame, Hello, HelloAck, SessionStatus},
    rejection_frame,
    transport::ChildConnection,
};
use anyhow::{Context as _, Result, bail};
use smol::channel::{Receiver, Sender};
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

/// Overrides which daemon binary is used. First in the resolution order, so a
/// developer can point the app at a build without installing anything.
pub const DAEMON_BIN_ENV: &str = "ADE_DAEMON_BIN";

/// The binary's installed name — what `deploy` writes and what a release build
/// ships beside the app.
const DAEMON_BIN: &str = "ade-daemon";

/// The same binary under cargo's name for it. A dev build runs from
/// `target/debug`, where the daemon lands as the crate name until a bundle
/// renames it.
const DAEMON_BIN_IN_TARGET: &str = "ade_session_daemon";

/// Size a session is created with, before any client has attached to say what
/// its terminal is. The attach client sends a real [`Frame::Resize`] as soon as
/// it has a tty, so this only shapes output produced in the first instants.
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

/// What a workspace's *extra* sessions call themselves.
///
/// The first session carries the workspace id as its label (see
/// [`DaemonBackend::create_session`]), which is how the daemon names a workspace
/// it has to invent. A sibling must not repeat that — it would be a second
/// candidate for the same name — and has nothing else to say about itself yet:
/// per-agent identity arrives with the agent layer.
const EXTRA_SESSION_LABEL: &str = "terminal";

/// How long the status stream waits before its *first* reconnect after the
/// daemon (or the transport under it) goes away. It doubles per consecutive
/// failure up to [`MAX_RESUBSCRIBE_DELAY`], and any subscribe that works puts
/// it back here.
///
/// Short because the common failure is transient — a daemon being restarted, a
/// forward being re-established — and the dots are stale until it comes back.
const FIRST_RESUBSCRIBE_DELAY: Duration = Duration::from_secs(1);

/// Ceiling on that doubling.
///
/// The failure this exists for is *permanent*: no daemon binary at the path we
/// resolved, which no amount of retrying fixes. At one attempt a second that
/// was a process spawn per second forever — and on Windows a console window
/// flashing per second. Thirty seconds keeps a host that really does come back
/// coming back within half a minute, which is well inside what a user reads as
/// "it noticed".
const MAX_RESUBSCRIBE_DELAY: Duration = Duration::from_secs(30);

/// How long to wait before the one retry a handshake that ended in EOF gets
/// (`docs/ade/protocol-compatibility.md` §6.1).
///
/// Short enough that a user waits on it without noticing, long enough that a
/// daemon in the middle of binding its socket, or a transport that just dropped
/// a connection, has moved on by the time the second attempt lands. The same
/// value the attach client and `--ensure` use, spelled again here rather than
/// shared: that constant is private to the daemon crate, which this one depends
/// on only in tests.
const HANDSHAKE_RETRY_DELAY: Duration = Duration::from_millis(200);

/// The generation a daemon too old for this client is asked to exit at.
///
/// Generation 2 is where the envelope — and `Shutdown` with it — shipped, and
/// the only generation such a daemon serves. Below it there is no frame to
/// send: a pre-cut daemon cannot decode one at all.
const RETIRING_GENERATION: u32 = 2;

/// The generation views became addressable at: [`Frame::FocusSession`],
/// `Attach.view_id`, and the attach client's `--view-id` option.
///
/// All three are gated on it, and the third is why the gate cannot be
/// wire-only: a generation-2 host *binary* rejects an unknown CLI option before
/// a single frame flows, so an argv shaped for this generation never reaches
/// the daemon that would have refused the frame.
const VIEW_GENERATION: u32 = 3;

/// No handshake has answered yet. Distinct from every real generation, and it
/// reads as the current one: nothing has said otherwise, so nothing is gated.
const GENERATION_UNKNOWN: u32 = 0;

/// How long a daemon that accepted `Shutdown` has to acknowledge it.
///
/// Not [`ANSWER_TIMEOUT`]: the daemon may fsync its ledger on the way out, and
/// this wait is held under the host lock, so the number is generous but finite.
/// Waiting forever wedges every later operation on that host behind a daemon
/// that is never going to answer.
const SHUTDOWN_ACK_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a request keeps waiting once the daemon has sent it something that
/// cannot be its answer.
///
/// Waiting *past* such a frame is the rule ([`DaemonBackend::request`]) and it
/// is right: an unsolicited error is legal at any time (§2), a mismatched `rid`
/// is a daemon bug rather than this request's answer, and a frame this build
/// cannot decode costs one request and not the connection (§2's repeal). What
/// none of that says is when to stop — and the daemon has *spent* a reply on
/// each of those frames, so "wait past it" alone is "wait forever", holding the
/// connection lock every later request to this host needs.
///
/// The clock therefore starts at the first frame that cannot be the answer, and
/// not at the request: a daemon that has merely said nothing yet is not
/// misbehaving, and §8's persist-before-ack can legitimately put an fsync in
/// front of an ack. Ten seconds is a bound on a daemon that has already
/// misspoken, and not a latency budget for a healthy one — a reply is queued on
/// an unbounded channel drained by a single writer task
/// (`crates/ade_session_daemon/src/server.rs:488`), and §8.2 puts a FIFO
/// persist worker in front of an ack, so how long a *correct* answer may take
/// is not something this side can put a number on. It does not have to: by the
/// time this clock is armed the daemon has spent this request's reply on some
/// other frame, and the wait is for something that is not coming.
///
/// A daemon that says *nothing at all* is deliberately not bounded here. That
/// is the transport's silence rather than a spent reply, this side is asleep on
/// a read rather than spinning, and cutting it short would fail exactly the
/// slow-but-correct acks the paragraph above protects.
///
/// **This bound is conditional on the wakeup getting a thread, and the wakeup
/// can in principle be starved.** [`sleep`] is `smol::unblock` around
/// `std::thread::sleep`, so [`AnswerDeadline`]'s timer is a job on smol's
/// process-wide `blocking` pool: FIFO, one job per worker until that job
/// returns, growth capped at 500 threads. The same pool carries jobs that need
/// not ever return — `smol::Unblock` over stdin is a read on a pipe nobody may
/// write to, and every `smol::fs` call is a blocking syscall that does not come
/// back on a hung network mount. With the pool saturated by those, the sleep
/// never starts; and once the peer has gone *silent* nothing else re-checks the
/// clock, because [`DaemonConnection::receive`] reads it before each read and
/// there is no next read. The wait is then unbounded again. This is a
/// starvation edge, not a routine outcome — it takes 500 concurrently stuck
/// blocking jobs — and the fix is a timer that does not need a thread
/// (`gpui::BackgroundExecutor::timer`, the replacement `clippy.toml` names for
/// the disallowed `smol::Timer`), not a different number here.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);

/// What an on-demand "upgrade host daemon" actually did.
///
/// Two answers rather than a bool, because "nothing happened" is not a failure
/// here: a daemon already running exactly these bytes is nothing for the
/// operator to worry about, and saying so is different from saying it swapped
/// the binary. There is no third, "busy": a click forces the shutdown, so a
/// daemon holding live sessions is upgraded over rather than refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonUpgradeOutcome {
    /// The daemon was stale, exited, and fresh bytes are in its place.
    Upgraded,
    /// The daemon is already running exactly the binary this client would
    /// deploy. Nothing happened, and nothing needed to.
    UpToDate,
}

/// The daemon's own "no": the [`Frame::Error`] that answered a request, kept
/// **typed** so a caller can branch on the code instead of reading prose.
///
/// Three decisions depend on which refusal it was — a failed kill that did
/// happen but could not be recorded, a local daemon too old to talk to, a
/// layout sync pushing into a workspace that is gone — and every one of them
/// used to be a substring match on a formatted string. The `Display` is still
/// `code: message`, which is what the sidebar shows.
///
/// The codes are `ade_session::proto::codes`, an open set: an unrecognised one
/// must read as an ordinary failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonRefusal {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for DaemonRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for DaemonRefusal {}

/// A handshake whose two generation windows do not meet, with both ranges kept
/// as numbers.
///
/// Typed for the same reason [`DaemonRefusal`] is: the direction decides
/// whether the UI may offer to replace the daemon, and the wording is prose the
/// contract forbids parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GenerationSkew {
    /// What this connection's `Hello` announced — not the crate constants: a
    /// caller may have pinned a narrower range.
    pub offered: (u32, u32),
    /// What the daemon said it serves. `None` from an `unsupported_generation`
    /// refusal, whose frame carries a code and prose and no numbers.
    pub daemon: Option<(u32, u32)>,
}

impl std::fmt::Display for GenerationSkew {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (min, max) = self.offered;
        match self.daemon {
            Some((daemon_min, daemon_max)) => write!(
                f,
                "this client speaks protocol generation {min}..={max} and the daemon speaks \
                 {daemon_min}..={daemon_max}"
            ),
            None => write!(f, "this client speaks protocol generation {min}..={max}"),
        }
    }
}

impl std::error::Error for GenerationSkew {}

/// Which end of an incompatible pair has to move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outdated {
    /// The daemon serves nothing this client speaks. Replacing it is the fix,
    /// and the only direction in which deploying this client's binary is an
    /// upgrade rather than a downgrade.
    Daemon,
    /// The daemon's window starts above this client's. Only a newer client
    /// meets it; pushing our bytes over it would destroy sessions to install
    /// something older.
    Client,
}

impl GenerationSkew {
    /// Which end to blame, from the numbers alone.
    ///
    /// **A daemon that named no range is read as the newer one.** That is the
    /// answer that offers no destructive action, and it is also the likely one:
    /// a daemon below this build's floor cannot decode `Hello` and never
    /// answers at all — it EOFs, which is [`PRE_CUT_DIAGNOSIS`]'s case, not
    /// this.
    fn outdated(&self) -> Outdated {
        match self.daemon {
            Some((_, daemon_max)) if daemon_max < self.offered.0 => Outdated::Daemon,
            _ => Outdated::Client,
        }
    }
}

/// The code the daemon refused with, wherever [`DaemonRefusal`] sits in
/// `error`'s chain — every `with_context` above it keeps it reachable.
pub fn refusal_code(error: &anyhow::Error) -> Option<&str> {
    error.chain().find_map(|cause| {
        cause
            .downcast_ref::<DaemonRefusal>()
            .map(|refusal| refusal.code.as_str())
    })
}

/// Whether a request asks the daemon to *change* something.
///
/// Only these make `persisted: false` news (§8.5): a read's ack carries the
/// same flag and means nothing by it, and warning on every `OpenWorkspace`
/// against a degraded daemon is how the real signal gets ignored.
fn mutates(frame: &Frame) -> bool {
    matches!(
        frame,
        Frame::CreateWorkspace { .. }
            | Frame::CreateSession { .. }
            | Frame::UpdateLayout { .. }
            | Frame::RenameWorkspace { .. }
            | Frame::KillWorkspace { .. }
            | Frame::Kill { .. }
    )
}

/// Sessions kept alive by the ADE session daemon on this machine.
pub struct DaemonBackend {
    endpoint: Endpoint,
    /// The control connection, opened on first use. `None` means "not
    /// connected", which is also what a broken connection is reset to — the
    /// next call reconnects, and the proxy restarts the daemon if it really is
    /// gone. Sessions lost that way come back as exited `(lost)` rows rather
    /// than being quietly recreated.
    connection: Mutex<Option<Live>>,
    next_request_id: AtomicU64,
    /// [`ANSWER_TIMEOUT`], except in tests that would otherwise have to sit
    /// through it to reach the failure they are about.
    answer_timeout: Duration,
    /// What the last handshake — on *either* connection — said about the
    /// daemon. See [`Remembered`].
    remembered: Arc<Remembered>,
}

/// The daemon snapshot the UI reads, shared by every connection to it.
///
/// **Shared because the status stream handshakes on its own connection.** It
/// reconnects independently of the control one and its ack is just as current,
/// so an upgrade the status thread was the first to meet still clears the arrow
/// (§6.4's "update on every successful reconnect").
///
/// **A UI snapshot, never the authority for sending**: a reconnect
/// renegotiates, so what a frame is gated on is the generation held with the
/// live connection in [`DaemonBackend::connection`]. Nothing here is read to
/// admit or refuse a frame.
#[derive(Default)]
struct Remembered {
    /// The last handshake's `degraded` flag — §8.5, read-only ledger on a
    /// newer schema. Set on (re)connect, stale between them like
    /// [`DaemonBackend::daemon_stale`]; nothing before the first handshake
    /// claims it.
    degraded: AtomicBool,
    /// Which daemon answered — see [`SessionBackend::instance_id`]. `None`
    /// until one has, and from a daemon too old to say. Never unset by a later
    /// handshake that omits it: the only way to get one is to reach a daemon,
    /// and a daemon that once named itself is the daemon this backend is for.
    instance_id: Mutex<Option<String>>,
    /// The negotiated generation, [`GENERATION_UNKNOWN`] before there has been
    /// a handshake. Kept across disconnects, so the arrow reads last-known
    /// rather than blinking off with the channel.
    generation: AtomicU32,
}

impl Remembered {
    /// Record what a handshake said, and tell the sidebar if the generation
    /// moved.
    ///
    /// `endpoint` is only ever used to reach the freshness observers, which are
    /// channel sends: this must not call back into the backend, whose
    /// connection lock the control caller is holding while it runs.
    fn remember(&self, endpoint: &Endpoint, ack: &HelloAck) {
        self.degraded.store(ack.degraded, Ordering::Relaxed);
        if let Some(instance) = &ack.instance_id {
            *self.instance_id.lock().unwrap_or_else(|e| e.into_inner()) = Some(instance.clone());
        }
        // Same rule as a freshness verdict: only a *change* is announced, or a
        // host reconnected four times would repaint every sidebar four times.
        if self.generation.swap(ack.generation, Ordering::Relaxed) != ack.generation
            && let Transport::Forwarded(link) = &endpoint.transport
        {
            link.announce_freshness();
        }
    }
}

/// The live control connection and the generation its own handshake selected.
///
/// One struct, so the two can only be read together under one lock: a
/// generation read beside the slot could be paired with the connection a
/// reconnect has since replaced.
struct Live {
    connection: DaemonConnection,
    generation: u32,
}

impl Default for DaemonBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl DaemonBackend {
    /// The backend against this machine's daemon, at the default socket.
    pub fn new() -> Self {
        Self::with_endpoint(Endpoint::local())
    }

    /// The backend against the daemon on `destination` — anything `ssh` itself
    /// accepts, including an alias out of the user's `~/.ssh/config`, which is
    /// deliberately still what resolves it.
    ///
    /// Nothing is contacted here: the host's `$HOME`, the `--ensure` and the
    /// forward all happen on the first operation, so constructing a backend for
    /// an unreachable host is free and the failure lands where the user can see
    /// which action caused it.
    pub fn remote(destination: &str) -> Result<Self> {
        Self::remote_with_args(destination, Vec::new())
    }

    /// [`Self::remote`] with extra ssh arguments — `-i`, `-p`, `-o …` — spliced
    /// in after the mandatory flags.
    ///
    /// Production passes none: auth and host resolution are the user's ssh
    /// config's job and ADE never implements its own. This exists so tests can
    /// name a dedicated key.
    pub fn remote_with_args(destination: &str, extra_args: Vec<String>) -> Result<Self> {
        Ok(Self::with_endpoint(Endpoint::remote(
            destination,
            extra_args,
        )?))
    }

    fn with_endpoint(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            connection: Mutex::new(None),
            next_request_id: AtomicU64::new(1),
            answer_timeout: ANSWER_TIMEOUT,
            remembered: Arc::new(Remembered::default()),
        }
    }

    #[cfg(test)]
    fn remember(&self, ack: &HelloAck) {
        self.remembered.remember(&self.endpoint, ack);
    }

    /// [`Self::remote_with_args`] with the host's paths given rather than read
    /// from its `$HOME`, and the local end named too.
    ///
    /// Tests only, and only so that a loopback test does not have to install a
    /// daemon into the operator's real `~/.ade` and leave one running there.
    /// Everything else — `--ensure` before the forward, the forward itself, the
    /// channel, the argv — is the production path.
    #[cfg(all(test, unix))]
    pub(crate) fn remote_at(
        destination: &str,
        extra_args: Vec<String>,
        local_socket: PathBuf,
        remote: (String, String, String),
    ) -> Self {
        Self::remote_preset(
            destination,
            extra_args,
            LocalEndpoint::Socket(local_socket),
            remote,
        )
    }

    /// [`Self::remote_at`] over a loopback port instead of a Unix socket — the
    /// transport a Windows client is on.
    ///
    /// Tests only, and it exists because this box is the only place ADE has
    /// tests: without it the TCP path would ship having never been run.
    #[cfg(all(test, unix))]
    pub(crate) fn remote_over_tcp_at(
        destination: &str,
        extra_args: Vec<String>,
        remote: (String, String, String),
    ) -> Result<Self> {
        Ok(Self::remote_preset(
            destination,
            extra_args,
            LocalEndpoint::loopback()?,
            remote,
        ))
    }

    #[cfg(all(test, unix))]
    fn remote_preset(
        destination: &str,
        extra_args: Vec<String>,
        local: LocalEndpoint,
        remote: (String, String, String),
    ) -> Self {
        let (bin, socket, state_dir) = remote;
        let host = ade_session::SshHost::new(destination).with_extra_args(extra_args);
        Self::with_endpoint(Endpoint {
            bin_path: resolve_binary(),
            address: Address::Named(local.clone()),
            state_dir: PathBuf::new(),
            transport: Transport::Forwarded(Arc::new(HostLink::with_paths(
                host,
                local,
                RemotePaths {
                    bin,
                    socket,
                    state_dir,
                },
            ))),
        })
    }

    /// The backend against a daemon that is already listening on `socket_path`,
    /// talked to directly instead of through a proxy child. Tests only:
    /// production always goes through the proxy, because start-if-absent lives
    /// there.
    #[cfg(all(test, unix))]
    pub(crate) fn connected_to(
        socket_path: impl Into<PathBuf>,
        bin_path: impl Into<PathBuf>,
    ) -> Self {
        Self::with_endpoint(Endpoint {
            bin_path: bin_path.into(),
            address: Address::Named(LocalEndpoint::Socket(socket_path.into())),
            state_dir: PathBuf::new(),
            transport: Transport::Direct,
        })
    }

    /// Whether an ack claims its mutation reached the daemon's ledger. Frames
    /// that are not mutation acks have nothing to say and answer `true`.
    fn acked_persisted(frame: &Frame) -> bool {
        match frame {
            Frame::Created { persisted, .. }
            | Frame::Workspace { persisted, .. }
            | Frame::LayoutChanged { persisted, .. }
            | Frame::WorkspaceRemoved { persisted, .. } => *persisted,
            _ => true,
        }
    }

    /// Send one request and wait for the reply that answers it.
    ///
    /// Three failure modes, kept apart on purpose: a [`Frame::Error`] is the
    /// daemon saying no and leaves the connection healthy, an IO failure means
    /// the transport is gone and drops it so the next call reconnects, and a
    /// daemon that talks without ever answering runs out of
    /// [`ANSWER_TIMEOUT`] and drops the connection too — see
    /// [`DaemonConnection::receive`] for why that one cannot keep it either.
    ///
    /// **`request_id` is what makes an error frame this request's answer**, and
    /// it has to be, because an error carrying no `rid` is legal *at any time*
    /// (`docs/ade/protocol-compatibility.md` §2): the daemon emits one when a
    /// write it was never asked to correlate fails. The connection discipline
    /// here — one request in flight, never attached, never subscribed — used to
    /// be the whole justification for reading any error as the answer; it is
    /// now the reason a *mismatched* rid is a daemon bug rather than a
    /// concurrent request, and both mismatches are logged and waited past
    /// instead of failing a request the daemon has not answered yet — but only
    /// until [`ANSWER_TIMEOUT`], because the daemon has already spent its reply
    /// on the frame being waited past.
    fn request<T>(
        &self,
        request_id: u64,
        request: Frame,
        want: impl Fn(&Frame) -> Option<T>,
    ) -> Result<T> {
        self.request_captured(request_id, request, want)
            .map(|(value, _degraded)| value)
    }

    /// [`Self::request`], answering also with the `degraded` flag of the
    /// connection that served it.
    ///
    /// The only sound way to pair an answer with the ledger state behind it:
    /// [`Self::degraded`] is re-armed by every reconnect, so a caller reading it
    /// after the fact can judge a degraded daemon's listing by a healthy
    /// daemon's flag. Read here, under the connection lock, it belongs to the
    /// daemon that answered.
    fn request_captured<T>(
        &self,
        request_id: u64,
        request: Frame,
        want: impl Fn(&Frame) -> Option<T>,
    ) -> Result<(T, bool)> {
        let mutation = mutates(&request);
        let mut slot = self.connection.lock().unwrap_or_else(|e| e.into_inner());
        let outcome = smol::block_on(async {
            let connection = &mut self.live(&mut slot).await?.connection;
            connection.send(&request).await?;
            // Unarmed until the daemon spends a frame on something that is not
            // this request's answer — see [`ANSWER_TIMEOUT`].
            let mut deadline: Option<AnswerDeadline> = None;
            let mut unanswered = 0usize;
            loop {
                let reply = match connection.receive(deadline.as_mut()).await? {
                    Received::Frame(reply) => Some(reply),
                    Received::Discarded => None,
                    // Both shapes end here and the wording has to fit both: a
                    // daemon that fell silent after one unusable frame, and one
                    // that never stopped sending them. What they have in common
                    // is the window since the first such frame — which is when
                    // the clock started — passing without an answer.
                    Received::Expired => bail!(
                        "the session daemon sent {unanswered} frame(s) that could not answer \
                         request {request_id}, and nothing that could in the {:?} since the \
                         first of them",
                        self.answer_timeout,
                    ),
                };
                if let Some(reply) = &reply
                    && let Some(value) = want(reply)
                {
                    if mutation && !Self::acked_persisted(reply) {
                        // §8.5. Not a failure and never retried: the mutation
                        // happened, only its ledger row did not. The standing
                        // condition behind it is already reported once per
                        // connection by the handshake's `degraded` flag.
                        log::warn!(
                            "the session daemon applied request {request_id} in memory only: \
                             its ledger is read-only, so this will not survive a restart"
                        );
                    }
                    return anyhow::Ok(Ok((
                        value,
                        self.remembered.degraded.load(Ordering::Relaxed),
                    )));
                }
                // Whatever it was, the daemon has now written something that
                // was not the answer, so the wait stops being open-ended.
                unanswered += 1;
                // Armed once and then carried across iterations: re-arming
                // would push the deadline out with every frame the daemon
                // sends, and re-creating the wakeup would park a pool thread
                // per frame — see [`AnswerDeadline`].
                deadline.get_or_insert_with(|| AnswerDeadline::armed(self.answer_timeout));
                if let Some(Frame::Error {
                    code,
                    message,
                    request_id: reply_id,
                    ..
                }) = reply
                {
                    match reply_id {
                        // The answer. The code travels with the prose because
                        // "not_found" and "internal" are operationally
                        // different answers — and because three callers decide
                        // on it ([`DaemonRefusal`]). Both are `bounded`: this
                        // becomes the ADE sidebar's failure text, and a
                        // `message` is a frame field the peer sizes up to
                        // `MAX_FRAME_BYTES`.
                        Some(id) if id == request_id => {
                            return anyhow::Ok(Err(DaemonRefusal {
                                code: bounded(&code).into_owned(),
                                message: bounded(&message).into_owned(),
                            }));
                        }
                        // Unsolicited by contract: diagnostics, never a
                        // pending request's answer. A log line is not the wire,
                        // but the peer still chose every byte of it.
                        None => log::warn!(
                            "the session daemon reported [{}] {}, unprompted",
                            bounded(&code),
                            bounded(&message)
                        ),
                        Some(id) => log::warn!(
                            "the session daemon answered request {id} with [{}] {} \
                             while request {request_id} was the only one in flight",
                            bounded(&code),
                            bounded(&message)
                        ),
                    }
                }
            }
        });
        match outcome {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(refusal)) => Err(anyhow::Error::new(refusal)),
            // The connection is gone, or is being given up on, which here is
            // the same thing: a read abandoned mid-frame cannot be resumed, and
            // an answer that arrives after this request stopped waiting would
            // meet the next one. Dropping it costs a reconnect and no session —
            // to the daemon it is a detach.
            Err(error) => {
                *slot = None;
                self.endpoint.on_connection_lost();
                Err(error)
            }
        }
    }

    /// The live control connection, opened if there is none.
    ///
    /// Takes the slot rather than the lock, so a caller that has to hold the
    /// connection across more than this — every request does — cannot
    /// accidentally drop it in between.
    async fn live<'a>(&self, slot: &'a mut Option<Live>) -> Result<&'a mut Live> {
        Ok(match slot {
            Some(live) => live,
            None => {
                let (connection, ack) = self.endpoint.connect().await?;
                self.remembered.remember(&self.endpoint, &ack);
                slot.insert(Live {
                    connection,
                    generation: ack.generation,
                })
            }
        })
    }

    /// Send one frame on the control connection and do not wait for anything.
    ///
    /// Only for ops the daemon answers with nothing at all — see
    /// [`Self::request`] for the correlated kind. A write that fails drops the
    /// connection for the same reason a request's does: a stream abandoned
    /// mid-frame cannot be resumed.
    ///
    /// `since` is the generation the op arrived at. An older connection is not
    /// sent it at all: the daemon would answer `uncapable_peer` and go on
    /// serving, but the sender's half of that contract is not to emit it (see
    /// `ade_session::proto`'s module doc). The check is under the slot lock the
    /// write itself takes, so a reconnect cannot renegotiate in between.
    fn notify(&self, since: u32, frame: Frame) -> Result<()> {
        let mut slot = self.connection.lock().unwrap_or_else(|e| e.into_inner());
        let outcome = smol::block_on(async {
            let live = self.live(&mut slot).await?;
            if live.generation < since {
                return Ok(());
            }
            live.connection.send(&frame).await
        });
        if outcome.is_err() {
            *slot = None;
            self.endpoint.on_connection_lost();
        }
        outcome
    }

    fn request_id(&self) -> u64 {
        self.next_request_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Replace this host's daemon binary now, because a human asked for it.
    ///
    /// The connect-time upgrade ([`HostLink::upgrade_if_stale`]) only fires
    /// when a connect happens to catch the daemon both stale and expendable,
    /// which on a host somebody actually works on may never happen. This is
    /// the way through: it re-probes rather than trusting the cached
    /// "already ensured", and it answers with what it found instead of
    /// warning into the log.
    ///
    /// Remote hosts only. The local daemon ships inside the app and is
    /// replaced when the app is.
    pub fn upgrade_daemon(&self) -> Result<DaemonUpgradeOutcome> {
        let Transport::Forwarded(link) = &self.endpoint.transport else {
            bail!("the local daemon upgrades with the app");
        };
        // Keep ordinary requests and a second upgrade off the old control
        // connection while this one is shutting its daemon down. Apart from
        // serialising repeated clicks, this means no request can succeed on
        // the daemon after we have decided that its connection is disposable.
        let mut connection = self.connection.lock().unwrap_or_else(|e| e.into_inner());
        let outcome = link.upgrade_on_demand();
        self.finish_upgrade(&mut connection, outcome)
    }

    fn finish_upgrade(
        &self,
        connection: &mut Option<Live>,
        outcome: Result<DaemonUpgradeOutcome>,
    ) -> Result<DaemonUpgradeOutcome> {
        if !matches!(&outcome, Ok(DaemonUpgradeOutcome::UpToDate)) {
            // The daemon our control connection was talking to is gone. Same
            // reset a died-under-us connection gets: drop the slot and mark
            // the link un-ensured, so the next request reconnects to the
            // replacement rather than writing into a dead channel. An error
            // may have happened after shutdown, so it needs the same reset.
            *connection = None;
            self.endpoint.on_connection_lost();
        }
        outcome
    }

    /// Whether this host's daemon is known to be behind the client.
    ///
    /// Reads what earlier probes recorded and asks nothing itself: this
    /// answers a *render*, and a control that costs an ssh round trip to draw
    /// would make every frame wait on the network. `false` therefore covers
    /// both "up to date" and "nobody has looked yet".
    ///
    /// Always `false` for the local daemon, which is replaced with the app.
    pub fn daemon_stale(&self) -> bool {
        match &self.endpoint.transport {
            Transport::Forwarded(link) => link.daemon_stale(),
            _ => false,
        }
    }

    /// Whether the last handshake found this host's ledger read-only (a newer
    /// schema than the daemon can write). `false` before any handshake, like
    /// [`Self::daemon_stale`].
    pub fn degraded(&self) -> bool {
        self.remembered.degraded.load(Ordering::Relaxed)
    }

    /// The generation the last handshake settled on, `None` before there has
    /// been one — see [`SessionBackend::daemon_generation`].
    pub fn daemon_generation(&self) -> Option<u32> {
        match self.remembered.generation.load(Ordering::Relaxed) {
            GENERATION_UNKNOWN => None,
            generation => Some(generation),
        }
    }

    /// The daemon this backend last handshook with — see
    /// [`SessionBackend::instance_id`].
    pub fn instance_id(&self) -> Option<String> {
        self.remembered
            .instance_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Ask to be told when [`Self::daemon_stale`] changes its answer.
    ///
    /// A local daemon registers nothing: its answer is the constant `false`, so
    /// an observer here would be one that never fires.
    pub fn observe_daemon_freshness(&self, observer: DaemonFreshnessObserver) {
        if let Transport::Forwarded(link) = &self.endpoint.transport {
            link.observe_daemon_freshness(observer);
        }
    }

    /// Every session the daemon holds, exited ones included.
    fn daemon_sessions(&self) -> Result<Vec<proto::SessionInfo>> {
        let id = self.request_id();
        self.request(
            id,
            Frame::ListSessions {
                request_id: Some(id),
            },
            |frame| match frame {
                Frame::SessionList {
                    sessions,
                    request_id: Some(reply_id),
                } if *reply_id == id => Some(sessions.clone()),
                _ => None,
            },
        )
        .context("listing the daemon's sessions")
    }

    /// The live session carrying `id` as its workspace id, newest first.
    ///
    /// Newest because a workspace can legitimately have a tombstone and a
    /// replacement (recreate after a crash); the one still running is the one
    /// the caller means.
    fn live_session(&self, id: &SessionId) -> Result<Option<proto::SessionInfo>> {
        Ok(newest_live(&self.daemon_sessions()?, id))
    }

    /// Create the daemon session for `spec`, reaping any tombstone it replaces.
    fn create_session(&self, spec: &SessionSpec) -> Result<proto::SessionInfo> {
        let existing = self.daemon_sessions()?;
        if newest_live(&existing, &spec.id).is_some() {
            bail!("a session for {} is already running", spec.id);
        }
        // Rows for this workspace whose process is already gone. The user has
        // been shown them ("Session … is gone") and has now asked for a new
        // one, so removing them is finishing that job — it ends no process,
        // because there is none left to end.
        for tombstone in existing
            .iter()
            .filter(|session| session.workspace_id == spec.id.as_str())
        {
            self.kill_daemon_session(&tombstone.id)?;
        }

        let request_id = self.request_id();
        self.request(
            request_id,
            Frame::CreateSession {
                // The seam's id, which is what makes a daemon session findable
                // from the registry's cached name. See the module docs.
                workspace_id: spec.id.to_string(),
                cwd: spec.directory.display().to_string(),
                // Empty means "the user's login shell", resolved by the daemon
                // on its own host. Resolving it here would send this machine's
                // `$SHELL` to a host that may not even share our OS.
                command: String::new(),
                env: Vec::new(),
                cols: DEFAULT_COLS,
                rows: DEFAULT_ROWS,
                // Session-level only for now: a workspace's session is a shell,
                // and per-agent windows arrive with the agent layer.
                agent_kind: "shell".to_owned(),
                instance_label: spec.id.to_string(),
                scrollback_bytes: None,
                request_id: Some(request_id),
            },
            |frame| match frame {
                Frame::Created {
                    session,
                    request_id: Some(reply_id),
                    ..
                } if *reply_id == request_id => Some(session.clone()),
                _ => None,
            },
        )
        .with_context(|| format!("creating a session for {}", spec.id))
    }

    /// The argv that gets a client onto one daemon session. No round trip: the
    /// binary and the address are the endpoint's, and the id is the caller's.
    fn session_argv(&self, id: &proto::SessionId, view_id: &str) -> Result<Vec<String>> {
        let mut argv = vec![
            self.endpoint.bin_path.display().to_string(),
            "attach".to_owned(),
            id.to_string(),
        ];
        argv.extend(view_argv(view_id));
        argv.extend(client_argv(&self.endpoint.address));
        Ok(argv)
    }

    fn kill_daemon_session(&self, id: &proto::SessionId) -> Result<()> {
        let request_id = self.request_id();
        self.request(
            request_id,
            Frame::Kill {
                session_id: id.clone(),
                request_id: Some(request_id),
            },
            // Matched by session id and not by `rid`, deliberately: the direct
            // reply to a kill is a rid-less `Removed`, same shape as the
            // broadcast every subscriber gets, so the session it names is the
            // only thing that can answer this.
            |frame| match frame {
                Frame::Removed { session_id } if session_id == id => Some(()),
                _ => None,
            },
        )
        .with_context(|| format!("killing daemon session {id}"))
    }
}

impl SessionBackend for DaemonBackend {
    fn create(&self, spec: &SessionSpec) -> Result<SessionId> {
        self.create_session(spec)?;
        // The seam's id, not the daemon's: the registry caches this, and it has
        // to be the same string `attach` and `exists` are called with later.
        Ok(spec.id.clone())
    }

    /// A sibling session in a workspace the daemon already holds.
    ///
    /// Deliberately without [`Self::create_session`]'s bookkeeping: no
    /// one-live-session guard, because a second live session is the point, and
    /// no tombstone reaping, because the rows it would take belong to siblings
    /// the caller never mentioned. A session never touches its workspace's
    /// record or its layout, so the new session enters the document only when
    /// this window captures it.
    fn create_session_in_workspace(&self, workspace_id: &str, cwd: &Path) -> Result<String> {
        let request_id = self.request_id();
        let session = self
            .request(
                request_id,
                Frame::CreateSession {
                    workspace_id: workspace_id.to_owned(),
                    cwd: cwd.display().to_string(),
                    // Empty means the user's login shell, resolved on the
                    // daemon's own host — see [`Self::create_session`].
                    command: String::new(),
                    env: Vec::new(),
                    cols: DEFAULT_COLS,
                    rows: DEFAULT_ROWS,
                    agent_kind: "shell".to_owned(),
                    // The workspace's first session labels itself with the
                    // workspace id; this one says what it is instead, so the
                    // daemon's own listing tells the two apart.
                    instance_label: EXTRA_SESSION_LABEL.to_owned(),
                    scrollback_bytes: None,
                    request_id: Some(request_id),
                },
                |frame| match frame {
                    Frame::Created {
                        session,
                        request_id: Some(reply_id),
                        ..
                    } if *reply_id == request_id => Some(session.clone()),
                    _ => None,
                },
            )
            .with_context(|| format!("creating a session in workspace {workspace_id}"))?;
        Ok(session.id.to_string())
    }

    fn list(&self) -> Result<Vec<SessionInfo>> {
        let mut seen = HashSet::new();
        Ok(self
            .daemon_sessions()?
            .into_iter()
            .filter(|session| session.status != SessionStatus::Exited)
            .filter(|session| !session.workspace_id.is_empty())
            .filter(|session| seen.insert(session.workspace_id.clone()))
            .map(|session| SessionInfo {
                id: SessionId::from(session.workspace_id),
            })
            .collect())
    }

    fn exists(&self, id: &SessionId) -> Result<bool> {
        Ok(self.live_session(id)?.is_some())
    }

    /// One frame, answered with the minted record alone: the row's first
    /// terminal is a separate create-session into the id this returns.
    ///
    /// **At generation 2 the reply is the combined create** — record, first
    /// login shell and a one-leaf layout — and nothing here has to change for
    /// it. The first terminal comes from [`SessionBackend::attach`], which is
    /// attach-*or*-create: it finds the session the daemon already made and
    /// never mints a second. Only the reply's extra `sessions` are dropped,
    /// which the listing that follows re-reads anyway.
    fn create_workspace(&self, root: &Path, name: Option<&str>) -> Result<BackendWorkspace> {
        let request_id = self.request_id();
        let workspace = self
            .request(
                request_id,
                Frame::CreateWorkspace {
                    root: root.display().to_string(),
                    name: name.map(str::to_owned),
                    request_id: Some(request_id),
                    env: Vec::new(),
                    cols: None,
                    rows: None,
                },
                |frame| match frame {
                    Frame::Workspace {
                        workspace,
                        request_id: Some(reply_id),
                        ..
                    } if *reply_id == request_id => Some(workspace.clone()),
                    _ => None,
                },
            )
            .with_context(|| format!("creating a workspace at {}", root.display()))?;
        Ok(BackendWorkspace {
            id: workspace.id,
            name: workspace.name,
            project_root: workspace.project_root,
            created_at: workspace.created_at,
        })
    }

    /// Every workspace record the daemon holds, sessions or no sessions.
    ///
    /// Not derived from [`Self::list`]: the daemon's workspace records outlive
    /// the sessions in them — a restarted daemon keeps the record and reports
    /// its ptys as lost — so a workspace with nothing running is exactly the
    /// case a listing of *sessions* cannot see, and exactly the one an empty
    /// registry most needs told about.
    fn list_workspaces(&self) -> Result<WorkspaceListing> {
        let request_id = self.request_id();
        let (workspaces, degraded) = self
            .request_captured(
                request_id,
                Frame::ListWorkspaces {
                    request_id: Some(request_id),
                },
                |frame| match frame {
                    Frame::WorkspaceList {
                        workspaces,
                        request_id: Some(reply_id),
                    } if *reply_id == request_id => Some(workspaces.clone()),
                    _ => None,
                },
            )
            .context("listing the daemon's workspaces")?;
        Ok(WorkspaceListing {
            workspaces: workspaces
                .into_iter()
                .map(|workspace| BackendWorkspace {
                    id: workspace.id,
                    name: workspace.name,
                    project_root: workspace.project_root,
                    created_at: workspace.created_at,
                })
                .collect(),
            degraded,
        })
    }

    fn attach(&self, spec: &SessionSpec) -> Result<Attached> {
        // Attach-or-create, like tmux's `new-session -A`: the first open of a
        // workspace that has no session is one step, and reopening a pane on a
        // live one reattaches to everything still running in it.
        let session = match self.live_session(&spec.id)? {
            Some(session) => session,
            None => self.create_session(spec)?,
        };
        Ok(Attached {
            argv: self.session_argv(&session.id, "")?,
            session_id: session.id.to_string(),
        })
    }

    fn attach_session(&self, session_id: &str, view_id: &str) -> Result<Vec<String>> {
        self.session_argv(&proto::SessionId::new(session_id), view_id)
    }

    /// One frame, no answer: `focus_session` is fire-and-forget on the wire,
    /// like `resize`. A daemon that refuses it says so with a rid-less error,
    /// which the next request logs and reads past — the right trade for a
    /// hint that fires on every focus change and must never block one.
    ///
    /// A generation-2 connection has no focus notion, so nothing is sent and
    /// the pty keeps the smallest attached client's size, as it did then.
    fn focus_session(&self, session_id: &str, view_id: &str, hover: bool) -> Result<()> {
        self.notify(
            VIEW_GENERATION,
            Frame::FocusSession {
                session_id: proto::SessionId::new(session_id),
                view_id: view_id.to_owned(),
                hover,
            },
        )
        .with_context(|| format!("focusing view {view_id} on daemon session {session_id}"))
    }

    fn open_workspace(&self, workspace_id: &str) -> Result<WorkspaceLayout> {
        let request_id = self.request_id();
        let workspace = self
            .request(
                request_id,
                Frame::OpenWorkspace {
                    workspace_id: workspace_id.to_owned(),
                    request_id: Some(request_id),
                },
                |frame| match frame {
                    Frame::Workspace {
                        workspace,
                        request_id: Some(reply_id),
                        ..
                    } if *reply_id == request_id => Some(workspace.clone()),
                    _ => None,
                },
            )
            .with_context(|| format!("opening workspace {workspace_id}"))?;
        Ok(WorkspaceLayout {
            layout: workspace.layout,
            rev: workspace.layout_rev,
        })
    }

    fn update_layout(&self, workspace_id: &str, layout: &LayoutDoc, rev: u64) -> Result<()> {
        let request_id = self.request_id();
        self.request(
            request_id,
            Frame::UpdateLayout {
                workspace_id: workspace_id.to_owned(),
                layout: layout.clone(),
                rev,
                request_id: Some(request_id),
            },
            |frame| match frame {
                // The ack, not the broadcast: the broadcast carries no request
                // id, and reaches this client on its event stream instead.
                Frame::LayoutChanged {
                    request_id: Some(reply_id),
                    ..
                } if *reply_id == request_id => Some(()),
                _ => None,
            },
        )
        .with_context(|| format!("storing layout rev {rev} for workspace {workspace_id}"))
    }

    fn kill_session(&self, session_id: &str) -> Result<()> {
        self.kill_daemon_session(&proto::SessionId::new(session_id))
    }

    /// One frame, answered with the renamed workspace. The daemon holds the
    /// name, so this is the write and the registry row is the copy.
    ///
    /// A daemon that does not implement the frame answers `unknown_op` against
    /// this request's `rid` and keeps serving — the connection survives it
    /// (§2), and the caller surfaces the error rather than recording a name
    /// only this client believes in. A daemon old enough not to speak the
    /// envelope at all never gets this far: it fails the handshake, where
    /// §6.1's diagnosis names it.
    fn rename_workspace(&self, workspace_id: &str, name: &str) -> Result<()> {
        let request_id = self.request_id();
        self.request(
            request_id,
            Frame::RenameWorkspace {
                workspace_id: workspace_id.to_owned(),
                name: name.to_owned(),
                request_id: Some(request_id),
            },
            |frame| match frame {
                // The ack, not the broadcast: the broadcast carries no request
                // id and reaches this client on its event stream.
                Frame::Workspace {
                    request_id: Some(reply_id),
                    ..
                } if *reply_id == request_id => Some(()),
                _ => None,
            },
        )
        .with_context(|| format!("renaming workspace {workspace_id}"))
    }

    /// One frame, and the daemon does the rest: every session in the workspace
    /// dies, the record and its layout are deleted, and every other client is
    /// told with a [`Frame::WorkspaceRemoved`] of its own.
    ///
    /// Not [`Self::kill`] with extra steps: that one takes the sessions and
    /// leaves the workspace behind, holding a layout full of tabs whose
    /// sessions are gone.
    fn kill_workspace(&self, workspace_id: &str) -> Result<()> {
        let request_id = self.request_id();
        self.request(
            request_id,
            Frame::KillWorkspace {
                workspace_id: workspace_id.to_owned(),
                request_id: Some(request_id),
            },
            |frame| match frame {
                Frame::WorkspaceRemoved {
                    request_id: Some(reply_id),
                    ..
                } if *reply_id == request_id => Some(()),
                _ => None,
            },
        )
        .with_context(|| format!("killing workspace {workspace_id}"))
    }

    /// See [`DaemonBackend::upgrade_daemon`] — this is the trait's door onto
    /// it, so the lifecycle layer can reach it through `Arc<dyn
    /// SessionBackend>` without knowing which backend it holds.
    fn upgrade_daemon(&self) -> Result<DaemonUpgradeOutcome> {
        DaemonBackend::upgrade_daemon(self)
    }

    /// See [`DaemonBackend::daemon_stale`].
    fn daemon_stale(&self) -> bool {
        DaemonBackend::daemon_stale(self)
    }

    /// See [`DaemonBackend::instance_id`].
    fn instance_id(&self) -> Option<String> {
        DaemonBackend::instance_id(self)
    }

    fn daemon_generation(&self) -> Option<u32> {
        DaemonBackend::daemon_generation(self)
    }

    /// See [`DaemonBackend::observe_daemon_freshness`].
    fn observe_daemon_freshness(&self, observer: DaemonFreshnessObserver) {
        DaemonBackend::observe_daemon_freshness(self, observer);
    }

    fn detach(&self, _: &SessionId) -> Result<()> {
        // Nothing to do, and nothing that *may* be done. A daemon session's
        // clients are its attached connections, and this backend's control
        // connection is not one of them — the attach client process is, and it
        // detaches by dying when its terminal closes. Sending the daemon a
        // `Detach` from here would detach nobody; killing the client from here
        // would be the app reaching into a terminal it does not own.
        Ok(())
    }

    fn kill(&self, id: &SessionId) -> Result<()> {
        // Every row for this workspace, exited ones included: the caller asked
        // for the session to be gone, and leaving a tombstone behind would make
        // it reappear as "gone" instead of as never-created.
        for session in self
            .daemon_sessions()?
            .iter()
            .filter(|session| session.workspace_id == id.as_str())
        {
            self.kill_daemon_session(&session.id)?;
        }
        Ok(())
    }

    fn reset_workspace_sessions(&self, id: &SessionId, directory: &Path) -> Result<()> {
        self.kill(id)?;
        if let Transport::Forwarded(link) = &self.endpoint.transport {
            link.recover_stale_daemon_processes(directory)?;
        }
        Ok(())
    }

    fn status_delivery(&self) -> StatusDelivery {
        StatusDelivery::Push
    }

    fn subscribe_events(&self) -> Result<Receiver<DaemonEvent>> {
        let (sender, receiver) = smol::channel::unbounded();
        let endpoint = self.endpoint.clone();
        let remembered = self.remembered.clone();
        // A plain thread, not a task: it owns a connection of its own and
        // spends its life blocked on it, which is the one thing an executor
        // thread must not do.
        std::thread::Builder::new()
            .name("ade-daemon-status".to_owned())
            .spawn(move || stream_status(endpoint, remembered, sender))
            .context("spawning the daemon status thread")?;
        Ok(receiver)
    }
}

/// Follow the daemon's event stream for as long as anybody is listening,
/// reconnecting when it breaks.
///
/// Reconnecting re-subscribes, and subscribing pushes a fresh status snapshot,
/// so a caller that missed events while the daemon was away is brought back up
/// to date rather than left holding a stale picture.
///
/// **A failure that never resolves must not cost a spawn a second, and must not
/// be silent.** Consecutive failures back off
/// ([`FIRST_RESUBSCRIBE_DELAY`] → [`MAX_RESUBSCRIBE_DELAY`]) and a subscribe
/// that worked resets the schedule; the first failure and every *change* of
/// failure is a warning, while the repeats stay at debug so a permanent one
/// does not bury the log it is trying to be visible in.
///
/// The state of the stream itself is [`DaemonEvent::Up`] / [`DaemonEvent::Down`]
/// on the same channel as the events, so a subscriber sees a failure in the
/// order it happened rather than only in the log. Incompatible retries keep
/// going at the maximum delay — a swapped-back daemon must heal by itself — and
/// say so once, not once per attempt.
fn stream_status(endpoint: Endpoint, remembered: Arc<Remembered>, sender: Sender<DaemonEvent>) {
    let mut delay = FIRST_RESUBSCRIBE_DELAY;
    let mut last_failure: Option<String> = None;
    let mut known_workspaces = HashMap::new();
    let mut has_workspace_snapshot = false;
    let mut announced: Option<StreamState> = None;

    while !sender.is_closed() {
        let mut subscribed = false;
        let outcome = smol::block_on(stream_status_once(
            &endpoint,
            &remembered,
            &sender,
            &mut subscribed,
            &mut announced,
            &mut known_workspaces,
            &mut has_workspace_snapshot,
        ));

        if subscribed {
            // The stream did work, so whatever broke it is a fresh failure and
            // is worth both a warning and a short retry — however long the
            // previous run of failures had stretched the delay.
            delay = FIRST_RESUBSCRIBE_DELAY;
            last_failure = None;
        }

        if let Err(error) = outcome {
            let message = format!("{error:#}");
            if last_failure.as_deref() == Some(message.as_str()) {
                log::debug!(
                    "daemon status stream for {endpoint} is still failing, retrying in {delay:?}: {message}"
                );
            } else {
                log::warn!(
                    "daemon status stream for {endpoint} stopped, retrying in {delay:?}: {message}"
                );
            }
            last_failure = Some(message.clone());
            let outdated = incompatible_daemon(&error);
            announce(
                &sender,
                &mut announced,
                StreamState::Down { message, outdated },
            );
            if outdated.is_some() {
                // The two ends cannot talk. Retrying is still right — the
                // daemon may be swapped back — but only at the far end of the
                // backoff, and the subscriber has already been told once.
                delay = MAX_RESUBSCRIBE_DELAY;
            }

            // The next connect re-runs `--ensure` on a remote host, so a daemon
            // that died behind a live forward is brought back rather than
            // reconnected to forever.
            endpoint.on_connection_lost();
        }

        if sender.is_closed() {
            break;
        }
        std::thread::sleep(delay);
        delay = next_resubscribe_delay(delay);
    }
}

/// The delay after one more consecutive failure: doubling, capped.
fn next_resubscribe_delay(delay: Duration) -> Duration {
    (delay * 2).min(MAX_RESUBSCRIBE_DELAY)
}

/// What the subscriber was last told about the stream — see [`announce`].
#[derive(Debug, PartialEq, Eq)]
enum StreamState {
    Up,
    Down {
        message: String,
        outdated: Option<Outdated>,
    },
}

/// Sends a stream transition, and only a transition.
///
/// The same failure retried is not news: a reconnect loop against a daemon this
/// client cannot speak to would otherwise re-open the incompatibility dialog
/// every backoff. A *changed* class or direction is news, and so is recovery.
fn announce(sender: &Sender<DaemonEvent>, announced: &mut Option<StreamState>, state: StreamState) {
    if announced.as_ref() == Some(&state) {
        return;
    }
    let event = match &state {
        StreamState::Up => DaemonEvent::Up,
        StreamState::Down { message, outdated } => DaemonEvent::Down {
            message: message.clone(),
            outdated: *outdated,
        },
    };
    *announced = Some(state);
    // Unbounded: the only failure is a receiver that is gone, which the loop
    // notices on its own.
    let _ = sender.try_send(event);
}

/// One subscription, from connect to disconnect.
///
/// `subscribed` is set the moment the subscription is known to be live — which
/// is what tells [`stream_status`] that this attempt was not a permanent
/// failure, since every healthy run ends the same way a broken one does: with
/// an error off the connection.
async fn stream_status_once(
    endpoint: &Endpoint,
    remembered: &Remembered,
    sender: &Sender<DaemonEvent>,
    subscribed: &mut bool,
    announced: &mut Option<StreamState>,
    known_workspaces: &mut HashMap<String, KnownWorkspace>,
    has_workspace_snapshot: &mut bool,
) -> Result<()> {
    let (mut connection, ack) = endpoint.connect().await?;
    // This handshake is as current as a control one: an upgrade met here first
    // clears the arrow without waiting for a control request. Snapshot only —
    // nothing below gates a frame on it.
    remembered.remember(endpoint, &ack);
    let degraded = ack.degraded;

    // **Subscribe first, list second.** The daemon's events name its own
    // session ids while this crate's callers name workspaces, and the listing
    // is what joins the two — so it has to be taken from *after* the point the
    // event stream starts, or a session created in the gap would be pushed
    // under a name nothing recognises. Whatever arrives while the listing is in
    // flight is held and replayed once the join is in hand.
    connection
        .send(&Frame::Subscribe {
            request_id: Some(1),
        })
        .await?;
    connection
        .send(&Frame::ListSessions {
            request_id: Some(2),
        })
        .await?;
    connection
        .send(&Frame::ListWorkspaces {
            request_id: Some(3),
        })
        .await?;

    let mut pending = Vec::new();
    let mut sessions = None;
    let mut workspaces = None;
    while sessions.is_none() || workspaces.is_none() {
        match connection.recv_decodable().await? {
            Frame::SessionList {
                sessions: listed,
                request_id: Some(2),
            } => sessions = Some(listed),
            Frame::WorkspaceList {
                workspaces: listed,
                request_id: Some(3),
            } => workspaces = Some(listed),
            other => pending.push(other),
        }
    }
    let mut join = SessionJoin::from_listing(sessions.unwrap_or_default());

    // Both listings came back, which means the `Subscribe` ahead of them did
    // too. Reconcile the workspace snapshot before replaying changes queued
    // behind it: this repairs layouts and removals missed while disconnected.
    *subscribed = true;
    let first_workspace_snapshot = !*has_workspace_snapshot;
    for event in
        workspace_snapshot_events(workspaces.unwrap_or_default(), known_workspaces, degraded)
    {
        if sender.send(event).await.is_err() {
            return Ok(());
        }
    }
    *has_workspace_snapshot = true;
    // After the recovery snapshot, and not before: a subscriber that is told
    // the host is back reads the state it was brought back to.
    announce(sender, announced, StreamState::Up);

    for frame in pending {
        if let Some(event) = status_event(frame, &mut join) {
            if accept_workspace_event(&event, known_workspaces, first_workspace_snapshot)
                && sender.send(event).await.is_err()
            {
                return Ok(());
            }
        }
    }
    loop {
        let frame = connection.recv_decodable().await?;
        if let Some(event) = status_event(frame, &mut join) {
            if accept_workspace_event(&event, known_workspaces, true)
                && sender.send(event).await.is_err()
            {
                // Nobody is listening any more: that is the unsubscribe.
                return Ok(());
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct KnownWorkspace {
    rev: u64,
    layout: LayoutDoc,
}

/// The news in a resubscribe's snapshot: what changed, and what the daemon no
/// longer has.
///
/// **A degraded daemon's silence is not a removal.** Its ledger is read-only and
/// its listing may omit live workspaces (§8.5), and a synthesized
/// `WorkspaceRemoved` deletes this client's row — with none of the reconcile
/// sweep's guards. So on `degraded` nothing is synthesized *and* the absent ids
/// stay in `known`, which is what makes the first healthy snapshot that still
/// omits one emit exactly one removal. Explicit removal events are unaffected:
/// they go through [`accept_workspace_event`].
fn workspace_snapshot_events(
    workspaces: Vec<proto::WorkspaceInfo>,
    known: &mut HashMap<String, KnownWorkspace>,
    degraded: bool,
) -> Vec<DaemonEvent> {
    let current: HashMap<_, _> = workspaces
        .iter()
        .map(|workspace| {
            (
                workspace.id.clone(),
                KnownWorkspace {
                    rev: workspace.layout_rev,
                    layout: workspace.layout.clone(),
                },
            )
        })
        .collect();
    let mut events = Vec::new();
    if !degraded {
        for workspace_id in known.keys() {
            if !current.contains_key(workspace_id) {
                events.push(DaemonEvent::WorkspaceRemoved {
                    workspace_id: workspace_id.clone(),
                });
            }
        }
    }
    for workspace in workspaces {
        let previous = known.get(&workspace.id);
        let replaced = previous.is_some_and(|previous| {
            workspace.layout_rev < previous.rev
                || (workspace.layout_rev == previous.rev && workspace.layout != previous.layout)
        });
        if replaced {
            events.push(DaemonEvent::WorkspaceReset(LayoutEvent {
                workspace_id: workspace.id,
                layout: workspace.layout,
                rev: workspace.layout_rev,
            }));
        } else if previous.is_none_or(|previous| {
            workspace.layout_rev != previous.rev || workspace.layout != previous.layout
        }) {
            events.push(DaemonEvent::Layout(LayoutEvent {
                workspace_id: workspace.id,
                layout: workspace.layout,
                rev: workspace.layout_rev,
            }));
        }
    }
    if degraded {
        known.extend(current);
    } else {
        *known = current;
    }
    events
}

fn accept_workspace_event(
    event: &DaemonEvent,
    known: &mut HashMap<String, KnownWorkspace>,
    forward_unknown_removal: bool,
) -> bool {
    match event {
        DaemonEvent::Layout(event) => {
            if known
                .get(&event.workspace_id)
                .is_some_and(|workspace| workspace.rev >= event.rev)
            {
                return false;
            }
            known.insert(
                event.workspace_id.clone(),
                KnownWorkspace {
                    rev: event.rev,
                    layout: event.layout.clone(),
                },
            );
            true
        }
        DaemonEvent::WorkspaceReset(event) => {
            known.insert(
                event.workspace_id.clone(),
                KnownWorkspace {
                    rev: event.rev,
                    layout: event.layout.clone(),
                },
            );
            true
        }
        DaemonEvent::WorkspaceRemoved { workspace_id } => {
            known.remove(workspace_id).is_some() || forward_unknown_removal
        }
        // Not workspace news, and not produced here at all: the stream's own
        // state is announced by [`announce`], past this filter.
        DaemonEvent::Session(_) | DaemonEvent::Up | DaemonEvent::Down { .. } => true,
    }
}

/// The daemon-session → workspace join, and which of those sessions still have
/// a process.
///
/// **The live set is what makes a workspace's status an aggregate.** Every
/// session of a workspace is pushed under the one seam id, so a bare
/// translation would report the first agent to exit as the *workspace* exiting
/// while its siblings are still running — a grey dot on a row that attaches
/// fine. The workspace only goes non-running when its last live session does.
#[derive(Default)]
struct SessionJoin {
    workspaces: HashMap<proto::SessionId, String>,
    live: HashSet<proto::SessionId>,
}

impl SessionJoin {
    fn from_listing(sessions: Vec<proto::SessionInfo>) -> Self {
        let mut join = Self::default();
        for session in sessions {
            join.note(
                session.id,
                session.workspace_id,
                session.status != SessionStatus::Exited,
            );
        }
        join
    }

    /// Records what a frame just said about one session.
    fn note(&mut self, id: proto::SessionId, workspace_id: String, alive: bool) {
        if alive {
            self.live.insert(id.clone());
        } else {
            self.live.remove(&id);
        }
        if !workspace_id.is_empty() {
            self.workspaces.insert(id, workspace_id);
        }
    }

    /// Drops a session the daemon no longer has.
    fn forget(&mut self, id: &proto::SessionId) {
        self.live.remove(id);
        self.workspaces.remove(id);
    }

    fn workspace_of(&self, id: &proto::SessionId) -> Option<&str> {
        self.workspaces.get(id).map(String::as_str)
    }

    /// The id this crate knows a daemon session by.
    ///
    /// Falls back to the daemon's own id for a session that appeared between
    /// the listing and the subscription — rare, and better reported under a
    /// name the caller does not recognise than dropped, since an event is also
    /// a "something changed" signal.
    fn seam_id(&self, id: &proto::SessionId, fallback: &str) -> SessionId {
        self.workspace_of(id)
            .filter(|workspace| !workspace.is_empty())
            .or(Some(fallback).filter(|workspace| !workspace.is_empty()))
            .unwrap_or(id.as_str())
            .into()
    }

    /// Whether any session of `workspace` still has a process.
    fn any_live_in(&self, workspace: &str) -> bool {
        self.live
            .iter()
            .any(|id| self.workspace_of(id) == Some(workspace))
    }

    /// What one session's death means for its *workspace*: the death itself
    /// when it was the last live one, and plain "running" while a sibling holds
    /// the row up.
    ///
    /// Called after the death has been recorded, so the live set already
    /// excludes the session that just went.
    fn after_death(&self, workspace: Option<&str>, death: SessionChange) -> SessionChange {
        match workspace {
            Some(workspace) if self.any_live_in(workspace) => {
                SessionChange::Status(WorkspaceStatus::Running)
            }
            _ => death,
        }
    }
}

/// Translate one pushed frame, keeping the daemon-id → workspace join current.
fn status_event(frame: Frame, join: &mut SessionJoin) -> Option<DaemonEvent> {
    let session = match frame {
        Frame::Created { session, .. } => {
            let seam = join.seam_id(&session.id, &session.workspace_id);
            // A workspace that already had a live session did not appear just
            // now — only its *session* did. `Created` is what tells a
            // subscriber holding no listing that a row exists at all, so a
            // sibling reports as a status move instead.
            let first = !join.any_live_in(&session.workspace_id);
            let status = workspace_status(session.status);
            join.note(
                session.id,
                session.workspace_id,
                session.status != SessionStatus::Exited,
            );
            StatusEvent::new(
                seam,
                if first {
                    SessionChange::Created(status)
                } else {
                    SessionChange::Status(status)
                },
            )
        }
        Frame::Status {
            session_id, status, ..
        } => {
            let seam = join.seam_id(&session_id, "");
            let workspace = join.workspace_of(&session_id).map(str::to_owned);
            let alive = status != SessionStatus::Exited;
            join.note(session_id, workspace.clone().unwrap_or_default(), alive);
            let change = SessionChange::Status(workspace_status(status));
            StatusEvent::new(
                seam,
                if alive {
                    change
                } else {
                    join.after_death(workspace.as_deref(), change)
                },
            )
        }
        Frame::Exited { session_id, .. } => {
            let seam = join.seam_id(&session_id, "");
            let workspace = join.workspace_of(&session_id).map(str::to_owned);
            join.note(session_id, workspace.clone().unwrap_or_default(), false);
            StatusEvent::new(
                seam,
                join.after_death(workspace.as_deref(), SessionChange::Exited),
            )
        }
        Frame::Removed { session_id } => {
            let seam = join.seam_id(&session_id, "");
            let workspace = join.workspace_of(&session_id).map(str::to_owned);
            join.forget(&session_id);
            StatusEvent::new(
                seam,
                join.after_death(workspace.as_deref(), SessionChange::Removed),
            )
        }
        // Layouts are named by the daemon's own workspace id, which is this
        // crate's seam id already — there is no join to keep, and no session to
        // look up.
        Frame::LayoutChanged {
            workspace_id,
            layout,
            rev,
            ..
        } => {
            return Some(DaemonEvent::Layout(LayoutEvent {
                workspace_id,
                layout,
                rev,
            }));
        }
        Frame::Workspace { workspace, .. } => {
            return Some(DaemonEvent::Layout(LayoutEvent {
                workspace_id: workspace.id,
                layout: workspace.layout,
                rev: workspace.layout_rev,
            }));
        }
        // Same: a workspace id is a seam id already. The sessions that went
        // with it announce themselves one `Removed` at a time, which is what
        // keeps the join map below current.
        Frame::WorkspaceRemoved { workspace_id, .. } => {
            return Some(DaemonEvent::WorkspaceRemoved { workspace_id });
        }
        _ => return None,
    };
    Some(DaemonEvent::Session(session))
}

/// The daemon's per-agent status, seen from the session level this seam works
/// at.
///
/// Working / needs-input / idle are all *the session is alive and reachable*;
/// telling them apart is per-agent telemetry, which the sidebar does not render
/// yet. Only "the process is gone" changes what a workspace is.
fn workspace_status(status: SessionStatus) -> WorkspaceStatus {
    match status {
        SessionStatus::Working | SessionStatus::NeedsInput | SessionStatus::Idle => {
            WorkspaceStatus::Running
        }
        SessionStatus::Exited => WorkspaceStatus::Disconnected,
    }
}

/// The newest live session carrying `id` as its workspace id.
fn newest_live(sessions: &[proto::SessionInfo], id: &SessionId) -> Option<proto::SessionInfo> {
    sessions
        .iter()
        .filter(|session| {
            session.workspace_id == id.as_str() && session.status != SessionStatus::Exited
        })
        .max_by_key(|session| session.created_at)
        .cloned()
}

/// Where the daemon is, and how to reach it.
///
/// Every address here is an address on **this** machine, remote endpoints
/// included: `bin_path` is our own attach client and `address` is either the
/// local daemon's socket or the local end of the host's ssh forward. That is
/// what keeps [`SessionBackend::attach`]'s argv a plain local command for a
/// remote workspace — the attach client is one more channel on the host's
/// single ssh connection, never an ssh invocation of its own.
#[derive(Clone, Debug)]
struct Endpoint {
    bin_path: PathBuf,
    address: Address,
    state_dir: PathBuf,
    transport: Transport,
}

/// The address, in the two shapes a command line can carry it.
///
/// [`Self::Named`] is every address something has to be *told*: the socket a
/// local unix daemon binds, and the local end of a host's forward. Both are
/// spelled out to the proxy and to the attach client, because both are choices
/// this crate made and neither side could derive.
///
/// [`Self::DefaultPipe`] is the Windows local daemon, where there is nothing to
/// tell. `ade-daemon` derives `\\.\pipe\ade-daemon-<sid>` from the SID it is
/// already running under, in every mode that needs it — so a name passed from
/// here could only ever be the same one it would have derived, or a wrong one.
/// The variant carries no name for exactly that reason, and the two argvs below
/// emit no endpoint flag for it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Address {
    Named(LocalEndpoint),
    #[cfg(windows)]
    DefaultPipe,
}

impl std::fmt::Display for Address {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Named(address) => address.fmt(formatter),
            #[cfg(windows)]
            Self::DefaultPipe => formatter.write_str("this user's daemon pipe"),
        }
    }
}

/// How [`Endpoint::address`] is reached.
#[derive(Clone, Debug)]
enum Transport {
    /// Spawn `ade-daemon --stdio-proxy`, which starts the daemon if it is not
    /// listening yet.
    Proxy,
    /// Connect straight to the address. Tests only — production takes the
    /// proxy, because start-if-absent lives there. Not `unix`-gated like the
    /// helpers that build one: the loopback half of it is what lets the
    /// control connection's own tests run a scripted daemon on any platform.
    #[cfg(test)]
    Direct,
    /// Connect straight to the address, which a host's `ssh -L` forward makes
    /// point at that host's daemon. The link owns the one ssh connection.
    Forwarded(Arc<HostLink>),
}

impl Endpoint {
    /// This machine's daemon: our own binary, at the standard endpoint — the
    /// socket under `~/.ade` on unix, this user's own pipe on Windows.
    ///
    /// The state dir is named on both, and on Windows that is the one thing
    /// this side does name: the daemon would derive the same `~/.ade/daemon`
    /// itself, but from `$HOME` where `dirs` reads the profile, and a client
    /// that says which ledger it means cannot disagree with the daemon it
    /// starts about where the sessions were written.
    fn local() -> Self {
        #[cfg(not(windows))]
        let address = Address::Named(LocalEndpoint::Socket(expand_home(DEFAULT_SOCKET_PATH)));
        #[cfg(windows)]
        let address = Address::DefaultPipe;
        Self {
            bin_path: resolve_binary(),
            address,
            state_dir: expand_home(DEFAULT_STATE_DIR),
            transport: Transport::Proxy,
        }
    }

    /// A host's daemon, reached over one forwarded socket.
    ///
    /// The forward's local end is a Unix socket where the ssh client can bind
    /// one and a loopback port where it cannot — Windows OpenSSH cannot, while
    /// the far end stays the host's Unix socket either way. Which one is
    /// settled **here**, at construction, because the attach argv names it and
    /// has to be handed out before the forward exists.
    fn remote(destination: &str, extra_args: Vec<String>) -> Result<Self> {
        #[cfg(unix)]
        let address = LocalEndpoint::Socket(host_socket_path(destination));
        #[cfg(not(unix))]
        let address = LocalEndpoint::loopback()?;

        let host = ade_session::SshHost::new(destination).with_extra_args(extra_args);
        Ok(Self {
            // Our own client, run locally against the forwarded address.
            bin_path: resolve_binary(),
            address: Address::Named(address.clone()),
            // Unused: the daemon's state lives on the host, and the proxy argv
            // this would feed is never built for a forwarded endpoint.
            state_dir: PathBuf::new(),
            transport: Transport::Forwarded(Arc::new(HostLink::new(host, address))),
        })
    }

    /// A fresh connection, handshaken and ready — with §6.1's one retry around
    /// the pair.
    ///
    /// The retry wraps *both* steps because a handshake that ended in EOF
    /// cannot be repeated on the connection that ended: the far side is gone,
    /// and a second `Hello` down the same pipe reads EOF again for reasons that
    /// have nothing to do with the daemon's age. So the unit is
    /// [`Self::open`] + handshake, and what it rebuilds differs per transport —
    /// a fresh `--stdio-proxy` child locally, a fresh socket or loopback
    /// connect behind a forward.
    async fn connect(&self) -> Result<(DaemonConnection, HelloAck)> {
        let outcome = self.connect_once().await;
        // The one upgrade path a *local* daemon has. It is too old to speak this
        // client's generation and said so, which is connection-fatal at the
        // daemon end — so the polite exit rides a connection of its own, and the
        // proxy then starts the binary that ships with this app. A remote host
        // is not this case: its daemon is replaced by the deploy path, and a
        // refusal there must reach the operator's upgrade prompt untouched.
        let Err(error) = outcome else {
            return outcome;
        };
        // Only ever *downwards*. A refusal whose direction is not provably
        // "the daemon is behind" — a newer local daemon, or one that named no
        // range — fails closed to the client-too-old surface: asking a newer
        // daemon to exit is how this client would install older bytes over the
        // sessions it holds.
        if matches!(self.transport, Transport::Forwarded(_))
            || refusal_code(&error) != Some(proto::error_code::UNSUPPORTED_GENERATION)
            || incompatible_daemon(&error) != Some(Outdated::Daemon)
        {
            return Err(error);
        }
        log::warn!(
            "the local session daemon speaks a protocol generation this client does not; \
             asking it to exit so the shipped one can take its place: {error:#}"
        );
        self.retire_outdated_local_daemon().await?;
        self.connect_once().await
    }

    /// [`Self::connect`] without the generation-skew recovery, so that the
    /// retry after it cannot recurse.
    async fn connect_once(&self) -> Result<(DaemonConnection, HelloAck)> {
        if let Transport::Forwarded(link) = &self.transport {
            // Blocking, deliberately: `--ensure` and the forward are one
            // short ssh command and one process spawn, and bringing a
            // host's single connection up is rare and strictly sequential.
            //
            // Outside the retried unit, and deliberately: §6.1 retries the
            // handshake, not the host. Re-running `--ensure` per attempt would
            // put a second ssh round trip inside a 200ms window to answer a
            // question — is this daemon pre-cut — that `--ensure` cannot
            // answer anyway.
            link.ensure_ready()?;
        }
        handshaken(|| self.open()).await
    }

    /// Ask an outdated local daemon to exit, on a connection it can actually
    /// read.
    ///
    /// The refusal that brought us here ended that connection, and this
    /// client's own `Hello` is what it refused — so the request is handshaken
    /// at **generation 2**, the generation `Shutdown` has existed at and the
    /// only one such a daemon serves. Unforced: the daemon re-checks for itself
    /// that it holds nothing an upgrade may sacrifice, and its decline is the
    /// answer this returns rather than something to override. Nothing here may
    /// hard-kill a local daemon; only the operator's own click does that, and
    /// only on a remote host.
    async fn retire_outdated_local_daemon(&self) -> Result<()> {
        let mut connection = self.open().await?;
        connection
            .handshake_at(Hello {
                min_generation: RETIRING_GENERATION,
                max_generation: RETIRING_GENERATION,
                capabilities: Vec::new(),
                request_id: None,
            })
            .await
            .context("handshaking with the outdated local daemon at generation 2")?;
        connection
            .send(&Frame::Shutdown {
                force: false,
                request_id: Some(1),
            })
            .await
            .context("asking the outdated local daemon to exit")?;
        await_shutdown_ack(&mut connection)
            .await
            .context("the outdated local daemon would not exit")
    }

    /// One connection, opened and not yet handshaken.
    async fn open(&self) -> Result<DaemonConnection> {
        match &self.transport {
            // Through the proxy rather than straight to the socket, because
            // start-if-absent is the proxy's job: the first thing ADE does
            // after a reboot must bring the daemon up, not fail because
            // nothing is listening yet.
            Transport::Proxy => Ok(DaemonConnection::Proxied(
                ChildConnection::spawn(&self.proxy_argv()).with_context(|| {
                    // Naming the binary, because [`resolve_binary`] falls
                    // through to a bare name for `PATH` to resolve: with no
                    // daemon installed anywhere, "not found" reaches the
                    // user only as this spawn failing, and it has to say
                    // *what* was not found.
                    format!(
                        "starting the daemon transport `{}`",
                        self.bin_path.display()
                    )
                })?,
            )),
            _ => self.open_directly().await,
        }
    }

    /// Connect to the local end, whatever kind it is. One channel on the ssh
    /// forward for a remote host; the socket itself for a local daemon.
    async fn open_directly(&self) -> Result<DaemonConnection> {
        let connection = match &self.address {
            #[cfg(unix)]
            Address::Named(LocalEndpoint::Socket(path)) => {
                DaemonConnection::Socket(ade_session::Connection::new(
                    smol::net::unix::UnixStream::connect(path)
                        .await
                        .with_context(|| format!("connecting to {}", path.display()))?,
                ))
            }
            #[cfg(not(unix))]
            Address::Named(LocalEndpoint::Socket(path)) => bail!(
                "this platform cannot connect to the Unix socket {}",
                path.display()
            ),
            // Unreachable: the only Windows endpoint that names it is the local
            // one, and that is [`Transport::Proxy`] — which is the point. The
            // pipe client lives in the daemon binary, once, and this crate
            // reaches it through `--stdio-proxy` rather than growing a second.
            #[cfg(windows)]
            Address::DefaultPipe => bail!(
                "this user's daemon pipe is only ever reached through the proxy, never connected \
                 to directly"
            ),
            Address::Named(LocalEndpoint::Loopback(port)) => {
                DaemonConnection::Tcp(ade_session::Connection::new(
                    smol::net::TcpStream::connect((LOOPBACK_ADDRESS, *port))
                        .await
                        .with_context(|| format!("connecting to {LOOPBACK_ADDRESS}:{port}"))?,
                ))
            }
        };
        Ok(connection)
    }

    /// A connection to this endpoint just died.
    ///
    /// For a forwarded endpoint that is the cue to re-run `--ensure` before the
    /// next operation: an EOF on a channel means either the ssh went away (the
    /// liveness check catches that) or the daemon behind it did, and only
    /// `--ensure` can tell the two apart or fix the second.
    fn on_connection_lost(&self) {
        if let Transport::Forwarded(link) = &self.transport {
            link.needs_ensure();
        }
    }

    fn proxy_argv(&self) -> Vec<String> {
        match &self.address {
            Address::Named(address) => DaemonEndpoint::preinstalled(
                self.bin_path.display().to_string(),
                // Only ever built for [`Transport::Proxy`], which fronts a
                // *local* daemon, and a daemon binds a socket. A loopback
                // address here would be a construction bug; it is passed
                // through so the child says so rather than this panicking.
                address.to_string(),
                self.state_dir.display().to_string(),
            )
            .proxy_argv(),
            // The same argv minus the endpoint, which `--stdio-proxy` derives
            // for itself here. Built rather than borrowed from
            // [`ade_session::deploy`]: that module's argv names a socket, and
            // this endpoint is a pipe.
            #[cfg(windows)]
            Address::DefaultPipe => vec![
                self.bin_path.display().to_string(),
                "--stdio-proxy".to_owned(),
                "--state-dir".to_owned(),
                self.state_dir.display().to_string(),
            ],
        }
    }
}

/// Wait out a daemon that has been sent [`Frame::Shutdown`], bounded by
/// [`SHUTDOWN_ACK_TIMEOUT`].
///
/// A declined shutdown is a [`DaemonRefusal`] carrying the daemon's reason —
/// correlated, like every other request: an error with no rid is the daemon
/// reporting something else entirely and must not be read as a refusal to exit.
async fn await_shutdown_ack(connection: &mut DaemonConnection) -> Result<()> {
    let mut deadline = AnswerDeadline::armed(SHUTDOWN_ACK_TIMEOUT);
    loop {
        match connection
            .receive(Some(&mut deadline))
            .await
            .context("waiting for the shutdown answer")?
        {
            Received::Frame(Frame::ShutdownAck { .. }) => return Ok(()),
            Received::Frame(Frame::Error {
                code,
                message,
                request_id: Some(1),
                ..
            }) => {
                return Err(anyhow::Error::new(DaemonRefusal {
                    code: bounded(&code).into_owned(),
                    message: bounded(&message).into_owned(),
                }));
            }
            Received::Frame(other) => {
                log::debug!("ignoring {other:?} while waiting for ShutdownAck")
            }
            Received::Discarded => continue,
            // The caller drops the connection with this error, which is what
            // [`Received::Expired`] requires: the read was abandoned mid-frame.
            Received::Expired => bail!(
                "the session daemon accepted the shutdown request and did not answer within \
                 {SHUTDOWN_ACK_TIMEOUT:?}"
            ),
        }
    }
}

/// Open a connection and handshake on it, with §6.1's one retry.
///
/// A handshake that ends in EOF with no reply is the signature of a daemon that
/// predates the protocol cut: it cannot decode `{"op":"hello",…}` at all, and
/// its receive loop drops the connection without writing anything. A transient
/// failure — a daemon still binding its socket, a forward that just died —
/// looks exactly the same, so the first one buys a retry and only the second is
/// diagnosed. Anything else is an *answer*: a generation outside this client's
/// range, an explicit error frame, a spawn that failed. Retrying an answer
/// would just get it twice.
async fn handshaken<C, F>(open: C) -> Result<(DaemonConnection, HelloAck)>
where
    C: Fn() -> F,
    F: Future<Output = Result<DaemonConnection>>,
{
    let mut retried = false;
    loop {
        let mut connection = open().await?;
        let error = match connection.handshake().await {
            Ok(ack) => return Ok((connection, ack)),
            Err(error) => error,
        };
        // Before the delay, not after: the failed connection is a proxy child
        // or a socket, and holding it open for 200ms to prove nothing is what
        // leaks one per retry.
        drop(connection);
        if !handshake_ended_in_eof(&error) {
            return Err(error);
        }
        if retried {
            return Err(error.context(PRE_CUT_DIAGNOSIS));
        }
        retried = true;
        log::debug!("the daemon handshake ended in EOF with no reply; retrying once");
        sleep(HANDSHAKE_RETRY_DELAY).await;
    }
}

/// Whether a failed handshake carries §6.1's pre-cut signature: EOF with
/// nothing read.
///
/// [`ade_session::Connection::handshake`] answers with an `anyhow::Error`, but
/// the [`ReadFrameError`] is still in its chain — so the shared predicate is
/// what decides, and this client never grows a second opinion about what a
/// pre-cut daemon looks like. Written out here rather than imported because the
/// daemon crate keeps its copy `pub(crate)` and is a dev-dependency of this one.
fn handshake_ended_in_eof(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<ReadFrameError>()
            .is_some_and(is_handshake_eof)
    })
}

/// Whether an error chain carries [`PRE_CUT_DIAGNOSIS`] — i.e. [`handshaken`]
/// already retried and concluded the daemon predates the protocol cut.
///
/// The diagnosis rides the chain as a context string, not a typed error, so
/// this matches the sentence itself — the one place it is produced
/// ([`handshaken`]) and the one place it is acted on (the forced shutdown's
/// fallback) share the constant.
fn is_pre_cut_daemon(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.to_string().contains(PRE_CUT_DIAGNOSIS))
}

/// Whether this failure is "the two ends cannot talk" rather than "the host
/// could not be reached", and if so which end is behind.
///
/// Three shapes reach here and none of them is read out of prose: the pre-cut
/// EOF diagnosis, a typed [`DaemonRefusal`] with `unsupported_generation`, and
/// a typed [`GenerationSkew`] from an ack outside the range we offered. An
/// ordinary network or spawn failure is `None` and keeps the plain-terminal
/// fallback it always had.
pub(crate) fn incompatible_daemon(error: &anyhow::Error) -> Option<Outdated> {
    if is_pre_cut_daemon(error) {
        return Some(Outdated::Daemon);
    }
    let skew = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<GenerationSkew>());
    if let Some(skew) = skew {
        return Some(skew.outdated());
    }
    (refusal_code(error) == Some(proto::error_code::UNSUPPORTED_GENERATION)).then(|| {
        GenerationSkew {
            offered: (proto::MIN_GENERATION, proto::MAX_GENERATION),
            daemon: None,
        }
        .outdated()
    })
}

/// The POSIX script the forced upgrade runs to take down a pre-cut daemon.
///
/// Everything is guarded: no pidfile or a non-numeric one aborts with a
/// sentence telling the operator to stop the daemon by hand, and a pid whose
/// command name is not `ade-daemon` is never signalled — a recycled pid must
/// not take an unrelated process with it. A pid that is already dead skips
/// straight to removing the socket, which still has to happen: deployment
/// refuses to overwrite the binary while a socket file exists.
fn pre_cut_kill_script(state_dir: &str, socket: &str) -> String {
    use ade_session::deploy::shell_quote;

    format!(
        concat!(
            "pidfile={pidfile}\n",
            "socket={socket}\n",
            "if [ ! -f \"$pidfile\" ]; then\n",
            "  echo \"no pidfile at $pidfile; stop the daemon by hand\" >&2; exit 3\n",
            "fi\n",
            "pid=$(cat \"$pidfile\")\n",
            "case \"$pid\" in ''|*[!0-9]*)\n",
            "  echo \"the pidfile at $pidfile does not hold a pid: $pid\" >&2; exit 3;;\n",
            "esac\n",
            "if kill -0 \"$pid\" 2>/dev/null; then\n",
            "  case \"$(ps -o comm= -p \"$pid\")\" in\n",
            "    *ade-daemon*) ;;\n",
            "    *) echo \"pid $pid is not an ade-daemon; stop the daemon by hand\" >&2; exit 4;;\n",
            "  esac\n",
            "  kill \"$pid\"\n",
            "  i=0\n",
            "  while kill -0 \"$pid\" 2>/dev/null && [ \"$i\" -lt 50 ]; do i=$((i+1)); sleep 0.1; done\n",
            "  if kill -0 \"$pid\" 2>/dev/null; then\n",
            "    kill -9 \"$pid\"\n",
            "    i=0\n",
            "    while kill -0 \"$pid\" 2>/dev/null && [ \"$i\" -lt 20 ]; do i=$((i+1)); sleep 0.1; done\n",
            "  fi\n",
            "  if kill -0 \"$pid\" 2>/dev/null; then\n",
            "    echo \"pid $pid did not exit\" >&2; exit 5\n",
            "  fi\n",
            "fi\n",
            "rm -f \"$socket\"\n",
        ),
        pidfile = shell_quote(&format!("{state_dir}/daemon.pid")),
        socket = shell_quote(socket),
    )
}

/// Reap terminal process groups an older daemon may have acknowledged before
/// they actually exited. The exact remote worktree is the safety boundary.
fn stale_daemon_recovery_script(directory: &Path) -> Result<String> {
    use ade_session::deploy::shell_quote;

    let directory = directory.to_str().with_context(|| {
        format!(
            "the remote worktree path is not UTF-8: {}",
            directory.display()
        )
    })?;
    Ok(format!(
        concat!(
            "root={root}\n",
            "if ! root=$(cd \"$root\" 2>/dev/null && pwd -P); then\n",
            "  echo \"cannot enter the worktree at $root\" >&2; exit 2\n",
            "fi\n",
            "if [ \"$root\" = / ]; then echo \"refusing to recover every terminal on the host\" >&2; exit 2; fi\n",
            "[ -r /proc/$$/stat ] || exit 0\n",
            "own_stat=$(cat /proc/$$/stat) || exit 2\n",
            "set -- ${{own_stat##*) }}\n",
            "own_group=$3\n",
            "groups=\n",
            "for process in /proc/[0-9]*; do\n",
            "  cwd=$(readlink \"$process/cwd\" 2>/dev/null) || continue\n",
            "  case \"$cwd\" in \"$root\"|\"$root\"/*) ;; *) continue;; esac\n",
            "  stat=$(cat \"$process/stat\" 2>/dev/null) || continue\n",
            "  set -- ${{stat##*) }}\n",
            "  group=$3\n",
            "  tty=$5\n",
            "  case \"$group:$tty\" in *[!0-9:]*) continue;; 0:*|*:0) continue;; esac\n",
            "  [ \"$group\" = \"$own_group\" ] && continue\n",
            "  case \" $groups \" in *\" $group \"*) ;; *) groups=\"$groups $group\";; esac\n",
            "done\n",
            "[ -n \"$groups\" ] || exit 0\n",
            "for group in $groups; do kill -HUP -\"$group\" 2>/dev/null || :; done\n",
            "sleep 1\n",
            "for group in $groups; do kill -KILL -\"$group\" 2>/dev/null || :; done\n",
            "attempt=0\n",
            "while [ \"$attempt\" -lt 200 ]; do\n",
            "  alive=\n",
            "  for group in $groups; do kill -0 -\"$group\" 2>/dev/null && alive=\"$alive $group\"; done\n",
            "  [ -z \"$alive\" ] && exit 0\n",
            "  groups=$alive\n",
            "  attempt=$((attempt + 1))\n",
            "  sleep 0.01\n",
            "done\n",
            "echo \"terminal process groups did not exit:$groups\" >&2\n",
            "exit 1\n",
        ),
        root = shell_quote(directory),
    ))
}

/// Park a blocking thread. `smol::Timer` is disallowed by the workspace lints.
async fn sleep(duration: Duration) {
    smol::unblock(move || std::thread::sleep(duration)).await;
}

/// How an endpoint is named in a log line: enough to tell *which* daemon a
/// message is about when several backends are logging at once.
///
/// A forwarded endpoint is named by its host, which is what the user thinks the
/// thing is; a local one by the binary being spawned and the socket it is meant
/// to bind, which are the two things a failure there is usually about.
impl std::fmt::Display for Endpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.transport {
            Transport::Forwarded(link) => {
                write!(formatter, "{} via {}", link.host.destination, self.address)
            }
            _ => write!(formatter, "{} at {}", self.bin_path.display(), self.address),
        }
    }
}

/// How the attach client is told where the daemon is — the flag pair
/// `ade-daemon attach` takes, which is the seam between this crate's idea of a
/// local endpoint and the client's command line.
fn client_argv(address: &Address) -> Vec<String> {
    match address {
        Address::Named(LocalEndpoint::Socket(path)) => {
            vec!["--socket".to_owned(), path.display().to_string()]
        }
        Address::Named(LocalEndpoint::Loopback(port)) => {
            vec!["--tcp".to_owned(), format!("{LOOPBACK_ADDRESS}:{port}")]
        }
        // Nothing at all: `ade-daemon attach <id>` with neither `--pipe` nor
        // `--tcp` derives this user's own pipe, which is the one this client
        // means. Naming it would mean deriving a SID in this crate to arrive at
        // the same string the client is about to derive anyway.
        #[cfg(windows)]
        Address::DefaultPipe => Vec::new(),
    }
}

/// Where ADE keeps one forwarded socket per remote host, on *this* machine.
#[cfg(unix)]
const HOST_SOCKET_DIR: &str = "~/.ade/hosts";

/// The local socket a host's daemon is reachable on once forwarded.
///
/// One per destination, so two backends for one host would collide on it —
/// which is the right shape, since there is only ever meant to be one.
#[cfg(unix)]
fn host_socket_path(destination: &str) -> PathBuf {
    expand_home(HOST_SOCKET_DIR).join(format!("{}.sock", sanitize_host(destination)))
}

/// An ssh destination as a single filename: `/`, `@` and `:` are what a
/// destination may carry that a path component may not.
#[cfg(unix)]
fn sanitize_host(destination: &str) -> String {
    destination
        .chars()
        .map(|character| match character {
            '/' | '@' | ':' => '_',
            other => other,
        })
        .collect()
}

/// The host's absolute paths. Absolute because the client has to name the same
/// endpoint the daemon binds, and `~` is only a shell's idea.
#[derive(Clone, Debug)]
struct RemotePaths {
    bin: String,
    socket: String,
    state_dir: String,
}

/// What the `--ensure` line says beyond "a daemon is listening".
///
/// The line is `ade-daemon <version>` followed by optional `key=value` tokens
/// a newer daemon appends: `hash=<hex sha256 of its binary>`,
/// `upgrade_ready=<bool>` and `generations=<min>..=<max>`. Absent tokens decode
/// to the conservative reading — no hash means a legacy daemon nothing may
/// touch, and readiness defaults to `false` for the same reason.
#[derive(Debug, PartialEq, Eq)]
struct EnsureReport {
    /// The daemon's binary identity, and the *only* thing that can say whether
    /// the host is behind: the version on the same line is `ade_session`'s
    /// crate version, pinned like every crate in this workspace, so comparing
    /// versions would be comparing a constant with itself.
    hash: Option<String>,
    upgrade_ready: bool,
    /// The protocol window the daemon serves — see [`Generations`].
    generations: Generations,
}

/// What the `generations=` token said, in the three states the deploy guard has
/// to tell apart.
///
/// **Absent and malformed are opposites, not the same.** No token is a daemon
/// predating it, which is definitionally older than this client and safe to
/// replace; a token this build cannot read can only come from a daemon whose
/// spelling is newer than this build knows, and replacing that one is the
/// downgrade the guard exists to refuse.
#[derive(Debug, PartialEq, Eq, Default)]
enum Generations {
    #[default]
    Absent,
    Valid(u32, u32),
    Malformed,
}

impl EnsureReport {
    fn parse(line: &str) -> Self {
        let mut report = Self {
            hash: None,
            upgrade_ready: false,
            generations: Generations::Absent,
        };
        for token in line.split_whitespace() {
            if let Some(value) = token.strip_prefix("hash=") {
                if !value.is_empty() {
                    report.hash = Some(value.to_owned());
                }
            } else if let Some(value) = token.strip_prefix("upgrade_ready=") {
                report.upgrade_ready = value == "true";
            } else if let Some(value) = token.strip_prefix("generations=") {
                // A second token is malformed however well each one parses:
                // last-wins would let an unknown format hide a real window
                // behind a readable one.
                report.generations = match report.generations {
                    Generations::Absent => parse_generations(value),
                    _ => Generations::Malformed,
                };
            }
        }
        report
    }
}

/// `<min>..=<max>`, or [`Generations::Malformed`] for anything else.
fn parse_generations(value: &str) -> Generations {
    let Some((min, max)) = value.split_once("..=") else {
        return Generations::Malformed;
    };
    match (min.parse(), max.parse()) {
        (Ok(min), Ok(max)) if min <= max => Generations::Valid(min, max),
        _ => Generations::Malformed,
    }
}

/// Refuse to replace a daemon whose window starts above this client's.
///
/// The deploy decision is a *hash* comparison and it runs before any handshake,
/// so without this a client meeting a newer daemon reads "different bytes" and
/// pushes its own older ones over sessions it could never have talked to. The
/// error is a [`GenerationSkew`], so the caller that surfaces it lands on
/// [`Outdated::Client`] — update the client — with no prose to parse.
fn refuse_downgrade(report: &EnsureReport) -> Result<()> {
    let daemon = match report.generations {
        Generations::Absent => return Ok(()),
        Generations::Valid(min, _) if min <= proto::MAX_GENERATION => return Ok(()),
        Generations::Valid(min, max) => Some((min, max)),
        // No numbers anybody may trust: the refusal carries none, and the
        // direction falls to the same client-too-old reading a range-less
        // typed refusal takes.
        Generations::Malformed => None,
    };
    Err(anyhow::Error::new(GenerationSkew {
        offered: (proto::MIN_GENERATION, proto::MAX_GENERATION),
        daemon,
    })
    .context("refusing to replace a daemon this client cannot prove is older"))
}

/// One remote host's single ssh connection, and everything needed to bring it
/// back.
#[derive(Debug)]
struct HostLink {
    host: ade_session::SshHost,
    /// The forward's local end on this machine, chosen once so that the attach
    /// argv is stable before there is a forward to serve it.
    local: LocalEndpoint,
    state: Mutex<HostLinkState>,
    /// Whether the host's daemon is behind this client's:
    /// `FRESHNESS_UNKNOWN` until a hash comparison says otherwise, and still
    /// that for a daemon too old to report a hash at all.
    ///
    /// Written only by [`HostLink::note_hash_verdict`], from the two places
    /// that compare the host's binary against ours: the connect-time pass and
    /// the operator's own upgrade. Read by the sidebar, to decide whether to
    /// offer that upgrade.
    ///
    /// **Deliberately not in [`HostLinkState`], and deliberately lock-free.**
    /// That mutex is held across `--ensure`, ssh round trips and a possible
    /// cross-build, so writing this from inside those would deadlock the
    /// writer against itself — `Mutex` is not reentrant — and reading it from
    /// a render would park the UI thread behind the whole connect.
    daemon_freshness: AtomicU8,
    /// Who to tell when [`Self::daemon_freshness`] changes, so the sidebar
    /// redraws on the probe rather than on the user's next unrelated click.
    freshness_observers: FreshnessObservers,
}

/// No probe has said anything yet, which is not the same as "up to date" but
/// draws the same: nothing.
const FRESHNESS_UNKNOWN: u8 = 0;
const FRESHNESS_CURRENT: u8 = 1;
const FRESHNESS_STALE: u8 = 2;

/// The registered [`DaemonFreshnessObserver`]s, in a type a `#[derive(Debug)]`
/// struct can hold: a callback has no useful debug form, so this prints how
/// many there are.
#[derive(Default)]
struct FreshnessObservers(Mutex<Vec<DaemonFreshnessObserver>>);

impl FreshnessObservers {
    fn add(&self, observer: DaemonFreshnessObserver) {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(observer);
    }

    /// Never with the lock held: an observer that registered another one — or
    /// that simply ran long — would otherwise block every writer behind it.
    fn announce(&self) {
        let observers = self
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        for observer in observers {
            observer();
        }
    }
}

impl std::fmt::Debug for FreshnessObservers {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .len();
        write!(formatter, "FreshnessObservers({count})")
    }
}

#[derive(Debug, Default)]
struct HostLinkState {
    /// Asked for once — `$HOME` is not knowable from here — and kept, because
    /// it cannot change under a running host without a new login anyway.
    paths: Option<RemotePaths>,
    /// The ssh process every channel rides. `None` until the first operation.
    forward: Option<ade_session::HostForward>,
    /// Whether the daemon has been ensured since the last failure.
    daemon_ensured: bool,
}

impl HostLink {
    fn new(host: ade_session::SshHost, local: LocalEndpoint) -> Self {
        Self {
            host,
            local,
            state: Mutex::new(HostLinkState::default()),
            daemon_freshness: AtomicU8::new(FRESHNESS_UNKNOWN),
            freshness_observers: FreshnessObservers::default(),
        }
    }

    /// A link whose remote paths are already known, so the `$HOME` query is
    /// skipped. See [`DaemonBackend::remote_at`], which is its only caller and
    /// carries the same gate.
    #[cfg(all(test, unix))]
    fn with_paths(host: ade_session::SshHost, local: LocalEndpoint, paths: RemotePaths) -> Self {
        Self {
            host,
            local,
            state: Mutex::new(HostLinkState {
                paths: Some(paths),
                ..HostLinkState::default()
            }),
            daemon_freshness: AtomicU8::new(FRESHNESS_UNKNOWN),
            freshness_observers: FreshnessObservers::default(),
        }
    }

    /// Make the local socket a working way to reach the host's daemon.
    ///
    /// **Ensure first, forward second, always.** A forward whose far end nobody
    /// has bound establishes fine and then fails one channel at a time with a
    /// bare EOF, so a forward is only worth trusting once the daemon behind it
    /// is known to be listening.
    ///
    /// Idempotent and cheap on the happy path: a live forward that has already
    /// ensured its daemon costs one `waitpid` and nothing else.
    fn ensure_ready(&self) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());

        let forward_alive = state
            .forward
            .as_mut()
            .is_some_and(ade_session::HostForward::is_alive);
        if forward_alive && state.daemon_ensured {
            return Ok(());
        }

        let paths = match state.paths.clone() {
            Some(paths) => paths,
            None => {
                let paths = self.remote_paths()?;
                state.paths = Some(paths.clone());
                paths
            }
        };

        if !state.daemon_ensured {
            let version = self.ensure_daemon(&paths)?;
            log::debug!("{} is running {version}", self.host.destination);
            state.daemon_ensured = true;
        }

        if !forward_alive {
            // Dropped before the replacement is spawned, so a host never
            // carries two ssh connections at once.
            state.forward = None;
            state.forward = Some(
                ade_session::HostForward::establish(&self.host, &paths.socket, self.local.clone())
                    .with_context(|| {
                        format!("forwarding the daemon socket on {}", self.host.destination)
                    })?,
            );
        }
        Ok(())
    }

    /// Start the host's daemon, deploying the binary first if the host has
    /// none. Answers with the version line the daemon reported.
    ///
    /// The happy path is untouched: one `--ensure` over ssh, and if a daemon is
    /// listening (or a binary is there to start one) that is the whole call.
    /// Deployment is reached only by [`EnsureOutcome::NotInstalled`] — exit
    /// 127, i.e. the remote shell found nothing to run — so a host that is
    /// merely unreachable, misconfigured or refusing never triggers a build.
    ///
    /// Exactly one retry. If `--ensure` still finds nothing after a successful
    /// upload, something is wrong with the *binary* — wrong triple, noexec
    /// mount — and looping would only build it again.
    ///
    /// One more thing rides the ensure line: **binary identity**. A daemon
    /// that reports its hash and declares itself upgrade-ready (nothing held
    /// but tombstones and idle shells) is compared against the binary this
    /// client would deploy, and on a mismatch it is asked to exit
    /// ([`Frame::Shutdown`]), replaced, and ensured again — see
    /// [`Self::upgrade_if_stale`]. A daemon holding a session with work in it,
    /// or one too old to report a hash, is left exactly where it was; the way
    /// past that is the operator's own "upgrade host daemon"
    /// ([`Self::upgrade_on_demand`]), which forces the exit rather than asking
    /// politely.
    fn ensure_daemon(&self, paths: &RemotePaths) -> Result<String> {
        let line = self.ensure_installed(paths)?;
        if self.upgrade_if_stale(paths, &line) {
            // The daemon that reported the line is gone and fresh bytes are
            // at `paths.bin`; this `--ensure` is the one that starts them.
            return self.ensure_after_upgrade(paths);
        }
        Ok(line)
    }

    /// Record what a hash comparison proved: the host's daemon is, or is not,
    /// running the bytes this client would deploy.
    ///
    /// The only signal there is. The version on the `--ensure` line cannot
    /// answer this — see [`EnsureReport::hash`].
    ///
    /// A verdict that repeats the one already held is dropped: announcing it
    /// costs a repaint of every sidebar in the process, and a reconnect that
    /// re-confirms four unchanged hosts must not cost four.
    fn note_hash_verdict(&self, stale: bool) {
        let value = if stale {
            FRESHNESS_STALE
        } else {
            FRESHNESS_CURRENT
        };
        if self.daemon_freshness.swap(value, Ordering::Relaxed) == value {
            return;
        }
        log::debug!(
            "{}: daemon is {}",
            self.host.destination,
            if stale { "behind" } else { "current" }
        );
        self.freshness_observers.announce();
    }

    /// Whether this host's daemon is known to be behind the client. `false`
    /// while nothing knows, so an unanswered question never draws a control
    /// that claims an update exists.
    fn daemon_stale(&self) -> bool {
        self.daemon_freshness.load(Ordering::Relaxed) == FRESHNESS_STALE
    }

    /// See [`SessionBackend::observe_daemon_freshness`].
    fn observe_daemon_freshness(&self, observer: DaemonFreshnessObserver) {
        self.freshness_observers.add(observer);
    }

    /// Wake the sidebar for something other than the hash verdict — the
    /// negotiated generation moving, which colours the arrow on the same rows.
    /// One channel, because a sidebar redraws all its rows or none.
    fn announce_freshness(&self) {
        self.freshness_observers.announce();
    }

    /// One `--ensure` over ssh, and nothing else. The raw question: is a daemon
    /// listening, and what does it say about itself?
    fn ensure_once(&self, paths: &RemotePaths) -> Result<EnsureOutcome> {
        ade_session::ensure_remote_daemon(&self.host, &paths.bin, &paths.socket, &paths.state_dir)
            .with_context(|| format!("starting the session daemon on {}", self.host.destination))
    }

    /// [`Self::ensure_once`] plus the one deploy-and-retry, with no upgrade
    /// decision attached. Both the connect-time path and the operator's own
    /// "upgrade host daemon" start here; only what they do with the line
    /// differs.
    fn ensure_installed(&self, paths: &RemotePaths) -> Result<String> {
        match self.ensure_once(paths)? {
            EnsureOutcome::Listening(line) => Ok(line),
            EnsureOutcome::NotInstalled => {
                self.deploy_daemon(paths)?;
                match self.ensure_once(paths)? {
                    EnsureOutcome::Listening(line) => Ok(line),
                    EnsureOutcome::NotInstalled => bail!(
                        "ade-daemon was deployed to {} on {} but still will not run there",
                        paths.bin,
                        self.host.destination,
                    ),
                }
            }
        }
    }

    /// The `--ensure` that starts the binary an upgrade just installed. No
    /// deploy-and-retry: the bytes are provably there, so anything but
    /// "listening" is a bad binary and re-uploading it would only produce the
    /// same one.
    fn ensure_after_upgrade(&self, paths: &RemotePaths) -> Result<String> {
        match self.ensure_once(paths)? {
            EnsureOutcome::Listening(line) => Ok(line),
            EnsureOutcome::NotInstalled => bail!(
                "the upgraded ade-daemon at {} will not run on {}",
                paths.bin,
                self.host.destination,
            ),
        }
    }

    /// Upgrade this host's daemon because a human asked for it, rather than
    /// because a connect happened to find it both stale and idle.
    ///
    /// Deliberately not [`Self::ensure_daemon`]: that one swallows the whole
    /// decision into a `bool` nobody sees, and short-circuits on
    /// `daemon_ensured` so a second click would do nothing at all. This always
    /// re-runs the probe and answers with what it found, because the operator
    /// clicked a button and is owed a sentence about it.
    ///
    /// Errors here are *reported*, not warned away: an upgrade nobody asked
    /// for must never fail a connection, but one somebody asked for must never
    /// fail silently.
    ///
    /// `upgrade_ready` is deliberately not consulted: the click is the
    /// consent, and the shutdown goes out forced. A daemon that would have
    /// declined is upgraded over, its sessions coming back as lost rows the
    /// reconcile pass recreates — which is what the operator asked for by
    /// clicking a button whose whole purpose is the way past a busy daemon.
    fn upgrade_on_demand(&self) -> Result<DaemonUpgradeOutcome> {
        // `ensure_ready` uses this same host-wide guard, so endpoint clones,
        // automatic upgrades and repeated clicks cannot overlap shutdown or
        // deployment.
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let paths = match state.paths.clone() {
            Some(paths) => paths,
            None => {
                let paths = self.remote_paths()?;
                state.paths = Some(paths.clone());
                paths
            }
        };
        let line = self.ensure_installed(&paths)?;
        let outcome = self.upgrade_to_local_binary(&paths, &EnsureReport::parse(&line), true)?;
        if outcome == DaemonUpgradeOutcome::Upgraded {
            self.ensure_after_upgrade(&paths)?;
        }
        Ok(outcome)
    }

    /// Upgrade the host's daemon in place if — and only if — it is provably
    /// stale and provably holding nothing. `true` means it was replaced and
    /// `--ensure` must run again.
    ///
    /// Every failure inside is a `warn` and `false`, never an error: a stale
    /// daemon is a perfectly usable daemon, and the connection it serves must
    /// not fail because an upgrade attempt did.
    fn upgrade_if_stale(&self, paths: &RemotePaths, ensure_line: &str) -> bool {
        let report = EnsureReport::parse(ensure_line);
        let Some(remote_hash) = report.hash.as_deref() else {
            log::debug!(
                "{}: daemon predates binary identity; leaving it alone",
                self.host.destination
            );
            return false;
        };
        if !report.upgrade_ready {
            // Holding a session with work in it, or an exited session's last
            // screen. Never disturbed on a connect — the operator's own
            // "upgrade host daemon" is the way through, and it says so.
            // Asked anyway, though nothing is done about it here: this is
            // precisely the host whose only way forward is the operator's own
            // upgrade button, and that button is drawn only for a daemon known
            // to be stale.
            match self.local_binary() {
                Ok((_, local_hash)) => self.note_hash_verdict(local_hash != remote_hash),
                Err(error) => log::warn!(
                    "{}: cannot tell whether the daemon is behind: {error:#}",
                    self.host.destination
                ),
            }

            //
            // Said out loud, and at `info` like the deploy lines below, because
            // the silent version of this is indistinguishable from a broken
            // cross-compile: no build is attempted, so the log shows *nothing*
            // for the host, and the operator concludes the toolchain failed
            // rather than that the daemon declined. Once per host per connect.
            log::info!(
                "{}: daemon runs build {}… and holds sessions with work in them; \
                 leaving it alone — \"upgrade host daemon\" forces it",
                self.host.destination,
                // `get`, not a byte slice: the hash is remote input, and a
                // multi-byte char straddling offset 12 would panic here.
                remote_hash.get(..12).unwrap_or(&remote_hash),
            );
            return false;
        }
        match self.upgrade_to_local_binary(paths, &report, false) {
            Ok(outcome) => outcome == DaemonUpgradeOutcome::Upgraded,
            Err(err) => {
                log::warn!(
                    "{}: daemon upgrade attempt failed; staying on the running daemon: {err:#}",
                    self.host.destination
                );
                false
            }
        }
    }

    /// This client's own daemon binary for the host's platform, with its hash
    /// — what every "is the host behind?" question is answered against.
    ///
    /// One ssh round trip for the platform and one `daemon_binary`, which is a
    /// cargo build that is a no-op unless the daemon's sources changed. Never
    /// on the main thread: every caller already runs off it.
    fn local_binary(&self) -> Result<(Vec<u8>, String)> {
        let platform = ade_session::HostPlatform::probe(&self.host)
            .with_context(|| format!("asking {} what platform it is", self.host.destination))?;
        let binary = ade_session::daemon_binary(&platform).with_context(|| {
            format!(
                "getting an ade-daemon binary for {}",
                platform.target_triple()
            )
        })?;
        let hash = ade_session::sha256_hex(&binary);
        Ok((binary, hash))
    }

    /// The actual swap: build/obtain our binary, compare hashes, ask the
    /// daemon to exit, install over it.
    ///
    /// The build ([`ade_session::daemon_binary`]) is the expensive step, and
    /// the hash comparison cannot happen without it — so a daemon already
    /// running these bytes answers [`DaemonUpgradeOutcome::UpToDate`] and is
    /// never disturbed, whatever `force` says. Cargo makes the repeat of that
    /// build a no-op; only the first call after an edit pays anything.
    ///
    /// `force` reaches exactly one place: the [`Frame::Shutdown`] this sends.
    /// The connect-time caller passes `false` and has already checked
    /// readiness itself, so a daemon that changed its mind in between declines
    /// and the attempt is abandoned — as it should be, nobody asked for it.
    /// The operator's own click passes `true` and is never declined.
    ///
    /// The single funnel for the destructive half, so the two guards that must
    /// never be bypassed live here: a daemon newer than this client is refused
    /// outright ([`refuse_downgrade`]) before the cross-build is even started,
    /// and one too old to report a hash cannot be compared and is left alone.
    fn upgrade_to_local_binary(
        &self,
        paths: &RemotePaths,
        report: &EnsureReport,
        force: bool,
    ) -> Result<DaemonUpgradeOutcome> {
        refuse_downgrade(report)?;
        let Some(remote_hash) = report.hash.as_deref() else {
            bail!(
                "the daemon on {} predates binary identity and cannot be upgraded in place; \
                 stop it by hand",
                self.host.destination
            );
        };
        let (binary, local_hash) = self.local_binary()?;
        self.note_hash_verdict(local_hash != remote_hash);
        if local_hash == remote_hash {
            log::debug!(
                "{}: daemon is exactly this build ({})",
                self.host.destination,
                &local_hash[..12]
            );
            return Ok(DaemonUpgradeOutcome::UpToDate);
        }
        log::info!(
            "{}: daemon runs build {}…, this client would deploy {}…; upgrading{}",
            self.host.destination,
            // Same reason as the decline log: remote input, `get` or panic.
            remote_hash.get(..12).unwrap_or(remote_hash),
            &local_hash[..12],
            if force { " (forced)" } else { "" },
        );
        self.request_shutdown(paths, force)
            .context("asking the daemon to exit for the upgrade")?;
        let config = deploy_config(binary, paths);
        ade_session::replace_daemon(&self.host, &config).with_context(|| {
            format!(
                "installing the fresh ade-daemon on {}",
                self.host.destination
            )
        })?;
        log::info!(
            "{}: ade-daemon upgraded at {}",
            self.host.destination,
            paths.bin
        );
        // These are this client's own bytes, so the question is settled until
        // the next probe asks it again.
        self.note_hash_verdict(false);
        Ok(DaemonUpgradeOutcome::Upgraded)
    }

    /// One short-lived protocol channel — `ssh <host> <bin> --stdio-proxy` —
    /// carrying exactly a handshake and a [`Frame::Shutdown`].
    ///
    /// Unforced, the daemon re-checks the shutdown condition itself; an
    /// [`Frame::Error`] back means it is no longer safe (a session with work in
    /// it appeared) and the upgrade is abandoned. `force` — the operator's own
    /// click — skips that check on the daemon side, so the only answer left is
    /// the ack or a dead channel.
    ///
    /// A *pre-cut* daemon cannot receive [`Frame::Shutdown`] at all: the
    /// handshake it would ride is exactly what the cut broke, so it fails with
    /// [`PRE_CUT_DIAGNOSIS`] — and a forced shutdown, whose whole purpose is
    /// replacing such daemons, falls back to terminating the process out of
    /// band ([`Self::kill_pre_cut_daemon`]). Only forced: the unforced
    /// connect-time path keeps propagating the diagnosis, because nothing
    /// without a human's click may hard-kill a daemon.
    fn request_shutdown(&self, paths: &RemotePaths, force: bool) -> Result<()> {
        let remote = vec![
            paths.bin.clone(),
            "--stdio-proxy".to_owned(),
            "--socket".to_owned(),
            paths.socket.clone(),
            "--state-dir".to_owned(),
            paths.state_dir.clone(),
        ];
        let argv = self.host.run_argv(&remote);
        smol::block_on(async {
            // The same connect-and-handshake unit every other connection here
            // gets, retry included: this channel is a fresh `--stdio-proxy`
            // child, so §6.1's rebuild is a second spawn. A host still running
            // a pre-cut daemon is exactly the host an upgrade is aimed at, so
            // the forced path treats the diagnosis as its cue to act rather
            // than a sentence to report.
            let mut connection = match handshaken(|| async {
                Ok(DaemonConnection::Proxied(
                    ChildConnection::spawn(&argv).context("spawning the shutdown channel")?,
                ))
            })
            .await
            {
                Ok((connection, _ack)) => connection,
                Err(error) if force && is_pre_cut_daemon(&error) => {
                    return self.kill_pre_cut_daemon(paths);
                }
                Err(error) => {
                    return Err(error).context("handshaking for the shutdown request");
                }
            };
            connection
                .send(&Frame::Shutdown {
                    force,
                    request_id: Some(1),
                })
                .await
                .context("sending Shutdown")?;
            await_shutdown_ack(&mut connection).await
        })
    }

    /// §6.2's forced path across the cut: terminate a pre-cut daemon out of
    /// band, because no frame this client can send will reach it.
    ///
    /// The script reads the daemon's own pidfile (`<state_dir>/daemon.pid`),
    /// refuses to signal a pid that is not an `ade-daemon`, terminates it —
    /// TERM first, KILL if it lingers — and removes the socket file, because
    /// binary deployment refuses to overwrite while one exists. Live PTYs die
    /// with the process; that cost was stated at the click this is reached
    /// from, and nowhere else reaches it.
    fn kill_pre_cut_daemon(&self, paths: &RemotePaths) -> Result<()> {
        use ade_session::deploy::HostExec as _;

        let output = self
            .host
            .run(&[
                "sh".to_owned(),
                "-c".to_owned(),
                pre_cut_kill_script(&paths.state_dir, &paths.socket),
            ])
            .with_context(|| {
                format!(
                    "terminating the pre-cut daemon on {}",
                    self.host.destination
                )
            })?;
        if !output.success() {
            bail!(
                "terminating the pre-cut daemon on {}: {}",
                self.host.destination,
                output.stderr.trim(),
            );
        }
        log::info!(
            "{}: the pre-cut daemon was terminated out of band for the forced upgrade",
            self.host.destination
        );
        Ok(())
    }

    fn recover_stale_daemon_processes(&self, directory: &Path) -> Result<()> {
        use ade_session::deploy::HostExec as _;

        let output = self
            .host
            .run(&[
                "sh".to_owned(),
                "-c".to_owned(),
                stale_daemon_recovery_script(directory)?,
            ])
            .with_context(|| {
                format!(
                    "checking {} for terminal processes left in {}",
                    self.host.destination,
                    directory.display()
                )
            })?;
        if !output.success() {
            bail!(
                "recovering terminal processes in {} on {}: {}",
                directory.display(),
                self.host.destination,
                output.stderr.trim(),
            );
        }
        Ok(())
    }

    /// Put a daemon binary on the host.
    ///
    /// Everything here is `info`, not `debug`: a cold cross-compile takes
    /// minutes on first connect, and these lines are the only thing the
    /// operator has to watch in `Zed.log` while it happens.
    fn deploy_daemon(&self, paths: &RemotePaths) -> Result<()> {
        let destination = &self.host.destination;
        let platform = ade_session::HostPlatform::probe(&self.host)
            .with_context(|| format!("asking {destination} what platform it is"))?;
        let triple = platform.target_triple();
        log::info!("{destination} has no ade-daemon; deploying one for {triple}");

        let binary = ade_session::daemon_binary(&platform)
            .with_context(|| format!("getting an ade-daemon binary for {triple}"))?;
        log::info!("uploading ade-daemon to {}:{}", destination, paths.bin);

        let config = deploy_config(binary, paths);
        let endpoint = ade_session::deploy::ensure_daemon(&self.host, &config)
            .with_context(|| format!("deploying ade-daemon to {destination}"))?;
        log::info!(
            "{destination} ade-daemon deployment: {:?} at {}",
            endpoint.outcome,
            endpoint.bin_path
        );
        Ok(())
    }

    /// Re-run `--ensure` and rebuild the forward before the next operation.
    /// Process liveness does not prove an SSH connection can still carry a
    /// channel; attach clients reconnect after the replacement binds locally.
    fn needs_ensure(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.daemon_ensured = false;
        state.forward = None;
    }

    /// What the host is, and where its daemon's files go.
    ///
    /// `ssh host command` is not a login shell, so the host is asked for
    /// `$HOME` through a plain `sh -c` the same way [`ade_session::deploy`]
    /// does — proven against a real connection by that crate's loopback tests.
    fn remote_paths(&self) -> Result<RemotePaths> {
        use ade_session::deploy::HostExec as _;

        let destination = &self.host.destination;
        let output = self
            .host
            .run(&[
                "sh".to_owned(),
                "-c".to_owned(),
                "printf %s \"$HOME\"".to_owned(),
            ])
            .with_context(|| format!("asking {destination} for $HOME"))?;
        if !output.success() || output.stdout.trim().is_empty() {
            bail!(
                "could not read $HOME on {destination}: {}",
                output.stderr.trim(),
            );
        }
        let home = output.stdout.trim().trim_end_matches('/').to_owned();
        Ok(RemotePaths {
            bin: expand_remote(ade_session::deploy::DEFAULT_BIN_PATH, &home),
            socket: expand_remote(DEFAULT_SOCKET_PATH, &home),
            state_dir: expand_remote(DEFAULT_STATE_DIR, &home),
        })
    }
}

/// `--view-id <id>`, or nothing for a caller that has no view to name — the
/// workspace-level attach, whose terminal never claims focus.
fn view_argv(view_id: &str) -> Vec<String> {
    match view_id.is_empty() {
        true => Vec::new(),
        false => vec!["--view-id".to_owned(), view_id.to_owned()],
    }
}

/// What to deploy where, for the host these paths belong to.
fn deploy_config(binary: Vec<u8>, paths: &RemotePaths) -> ade_session::DeployConfig {
    ade_session::DeployConfig::new(binary, ade_session::daemon_version())
        .with_bin_path(paths.bin.clone())
        .with_socket_path(paths.socket.clone())
        .with_state_dir(paths.state_dir.clone())
}

/// `~/x` against a host's `$HOME`. Anything else is already absolute.
fn expand_remote(path: &str, home: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => format!("{home}/{rest}"),
        None => path.to_owned(),
    }
}

/// The control connection, however it happens to be carried.
enum DaemonConnection {
    Proxied(ChildConnection),
    #[cfg(unix)]
    Socket(ade_session::Connection<smol::net::unix::UnixStream>),
    Tcp(ade_session::Connection<smol::net::TcpStream>),
}

impl DaemonConnection {
    /// Handshakes and answers with the ack's `degraded` flag — see
    /// [`DaemonBackend::degraded`].
    async fn handshake(&mut self) -> Result<HelloAck> {
        let ack = self
            .handshake_at(Hello::current())
            .await
            .context("handshaking with the session daemon")?;
        // The *generation* is policed, and not here: since the cut,
        // `Connection::handshake` verifies the daemon's selection lies inside
        // this build's range and fails legibly if it does not (§3.1). What is
        // left for this side is the record — which daemon, which generation,
        // how much of the optional surface it claims — and the one flag that
        // changes what a success means.
        log::debug!(
            "session daemon {} on {}: generation {}, {} capabilit{}, degraded={}",
            ack.daemon_version,
            ack.host_os,
            ack.generation,
            ack.capabilities.len(),
            if ack.capabilities.len() == 1 {
                "y"
            } else {
                "ies"
            },
            ack.degraded,
        );
        if ack.degraded {
            // §8.5: the daemon found a ledger written by a newer schema and
            // treats it as read-only, so its acks describe memory rather than
            // disk. A warn is this MR's floor — the SHOULD is a per-host banner
            // in the sidebar, and there is no seam carrying per-host status to
            // it yet (see the TODO in [`stream_status`], which wants the same
            // seam for a stream that died).
            log::warn!(
                "session daemon {} on {} cannot write its ledger (it was written by a newer \
                 schema): sessions and layouts will not survive a daemon restart until that \
                 host runs a daemon at least as new as its ledger",
                ack.daemon_version,
                ack.host_os,
            );
        }
        Ok(ack)
    }

    /// Send `hello` and read the ack, verified against the range `hello` itself
    /// announced.
    ///
    /// Deliberately **not** [`ade_session::Connection::handshake`], for two
    /// reasons that are the same reason: that one flattens the daemon's refusal
    /// code into prose, and the one caller that must branch on the code —
    /// generation skew, [`Endpoint::retire_outdated_local_daemon`] — is also
    /// the caller whose ack is legitimately outside this build's range. Its
    /// §6.1 EOF signature survives, because the read is the same raw one.
    async fn handshake_at(&mut self, hello: Hello) -> Result<HelloAck> {
        let (min, max) = (hello.min_generation, hello.max_generation);
        self.send(&Frame::Hello(hello)).await?;
        match self.recv().await? {
            Frame::HelloAck(ack) => {
                if ack.generation < min || ack.generation > max {
                    // Typed, because the UI has to choose between offering to
                    // replace the daemon and telling the user to update the
                    // client, and both ranges are here in numbers.
                    return Err(anyhow::Error::new(GenerationSkew {
                        offered: (min, max),
                        daemon: Some((ack.min_generation, ack.max_generation)),
                    })
                    .context(format!(
                        "daemon {} selected protocol generation {}, outside the {min}..={max} \
                         this connection offered",
                        bounded(&ack.daemon_version),
                        ack.generation,
                    )));
                }
                Ok(ack)
            }
            Frame::Error { code, message, .. } => Err(anyhow::Error::new(DaemonRefusal {
                code: bounded(&code).into_owned(),
                message: bounded(&message).into_owned(),
            })
            .context("the session daemon refused the handshake")),
            other => bail!("expected HelloAck, got {}", bounded_debug(&other)),
        }
    }

    async fn send(&mut self, frame: &Frame) -> Result<()> {
        match self {
            Self::Proxied(connection) => connection.send(frame).await,
            #[cfg(unix)]
            Self::Socket(connection) => connection.send(frame).await,
            Self::Tcp(connection) => connection.send(frame).await,
        }
    }

    async fn recv(&mut self) -> std::result::Result<Frame, ReadFrameError> {
        match self {
            Self::Proxied(connection) => connection.recv().await,
            #[cfg(unix)]
            Self::Socket(connection) => connection.recv().await,
            Self::Tcp(connection) => connection.recv().await,
        }
    }

    /// Read the next frame this client can act on, surviving the decode
    /// failures that are scoped to one request rather than to the connection.
    ///
    /// This is §2's repeal, from the client side. A daemon sending an `op` or a
    /// `body` this build cannot read is a §3.3 violation — the daemon is meant
    /// to send nothing above the negotiated generation — but the answer to a
    /// violation is a rejection frame and the next `recv`, never a dropped
    /// connection: dropping it would take every pending request and the whole
    /// status stream with it, which is exactly the defect the envelope exists
    /// to end. With no `rid` there is nobody to answer, so it is logged instead
    /// (`rejection_frame` decides which, so both ends cannot drift).
    ///
    /// The two that *do* end the connection are transport failure and a
    /// malformed envelope. Only the first is forced. The second is **policy**,
    /// and §2 says so by making the close a MAY: the stream is not desynced by
    /// it — `read_frame` takes the length prefix, checks it against
    /// `MAX_FRAME_BYTES`, and `read_exact`s the whole payload *before*
    /// `decode_frame` ever runs (`crates/ade_session/src/framing.rs:393`), so a
    /// malformed envelope leaves the reader exactly where the next prefix
    /// begins, and the reads that really would lose the length (an oversize
    /// prefix, a short read) are classified `Transport` instead.
    ///
    /// What is true of it is smaller and still enough. An unreadable envelope
    /// carries no readable `rid` (`ReadFrameError::rid` returns `None` for it),
    /// so there is no request to charge it to and no way to make it
    /// request-scoped; and the layer that failed is the one every frame on this
    /// connection shares, so nothing here says the next one will parse either.
    /// A control connection has one request in flight and no attach on it —
    /// giving up costs a reconnect and no session — so this side spends that
    /// cheap reconnect rather than keep reading a peer that has already shown
    /// it cannot frame. The attach client's opposite choice is equally
    /// conformant. Either way the peer is answered first, because §2 does
    /// require the receiver to say what happened.
    async fn recv_decodable(&mut self) -> Result<Frame> {
        loop {
            match self.receive(None).await? {
                Received::Frame(frame) => return Ok(frame),
                // A discard is nothing to this caller — it has no request to
                // fail — and with no deadline nothing can expire.
                Received::Discarded | Received::Expired => continue,
            }
        }
    }

    /// One frame, or what arrived instead of one — see [`Received`].
    ///
    /// `deadline` is the caller's liveness rule ([`ANSWER_TIMEOUT`]). `None`
    /// waits for as long as the daemon stays quiet, which is what the status
    /// stream does between events; the only answers it can then get are a frame
    /// and a discard.
    ///
    /// **Two mechanisms, because a peer can fail to answer in two shapes.** The
    /// clock is read here, before the read is even started, for the peer that
    /// keeps *talking*: `smol::future::or` returns as soon as its first future
    /// is `Ready` and never polls the second, [`DaemonConnection`] reads
    /// straight off the stream with no buffering in front of it, and a peer
    /// streaming frames keeps the kernel receive buffer non-empty — so every
    /// read completes on its first poll and the wakeup below is never reached.
    /// The wakeup is for the peer that goes *quiet* after arming the clock,
    /// where there is no next read to check it against.
    ///
    /// A frame already in the buffer therefore loses to a deadline that has
    /// just passed, where it used to win. That is the right way round: by then
    /// the daemon has spent the whole window on frames that were not the
    /// answer, which is the case this rule exists to end.
    ///
    /// **A caller that gets [`Received::Expired`] must drop the connection.**
    /// The read is abandoned wherever it had got to, and `ade_session`'s
    /// `read_frame` consumes the length prefix before the payload, so a stream
    /// given up on mid-frame is out of sync with its own next length. Nothing
    /// is lost by dropping it: the daemon reads a closed control connection as
    /// a detach, and sessions outlive detaches.
    async fn receive(&mut self, deadline: Option<&mut AnswerDeadline>) -> Result<Received> {
        let read = self.receive_now();
        let Some(deadline) = deadline else {
            return read.await;
        };
        if Instant::now() >= deadline.at {
            return Ok(Received::Expired);
        }
        let expiry = async {
            (&mut deadline.wakeup).await;
            Ok(Received::Expired)
        };
        smol::future::or(read, expiry).await
    }

    /// [`Self::receive`] without the deadline: one read, and §2's answer to a
    /// frame that could not be decoded.
    async fn receive_now(&mut self) -> Result<Received> {
        let error = match self.recv().await {
            Ok(frame) => return Ok(Received::Frame(frame)),
            Err(error) => error,
        };
        if !error.is_request_scoped() {
            if let Some(complaint) = rejection_frame(&error) {
                // Best effort: the connection is being dropped either way, and
                // a peer that just sent an unframeable byte stream may well not
                // be reading.
                let _ = self.send(&complaint).await;
            }
            return Err(match error {
                // Already an `anyhow::Error` carrying the io kind a caller
                // downcasts for §6.1, so it is passed through rather than
                // re-wrapped.
                ReadFrameError::Transport(error) => error,
                other => anyhow::Error::new(other)
                    .context("the session daemon sent an unframeable message"),
            });
        }
        match rejection_frame(&error) {
            Some(complaint) => self
                .send(&complaint)
                .await
                .context("rejecting a frame this client cannot read")?,
            None => log::warn!("ignoring a frame this client cannot read: {error}"),
        }
        Ok(Received::Discarded)
    }
}

/// One request's [`ANSWER_TIMEOUT`], armed: when it runs out, and the one thing
/// that can wake a read blocked past it.
///
/// It is a value the caller keeps rather than something
/// [`DaemonConnection::receive`] makes per read, and that is the whole point of
/// the type. [`sleep`] is `smol::unblock` around `std::thread::sleep`, which
/// `blocking` schedules eagerly and which no drop can interrupt — the thread
/// stays parked until the sleep it was given actually elapses. Built per read,
/// a daemon trickling frames for the whole window would park one pool thread
/// per frame, on a process-wide pool capped at 500 that Zed's `smol::fs` shares.
/// Built once, the whole window costs one.
///
/// The parked thread at all is [`sleep`]'s trade: `smol::Timer::after` is
/// disallowed by `clippy.toml`, and the replacement it names
/// (`gpui::BackgroundExecutor::timer`) is not reachable from inside the
/// `smol::block_on` this runs in.
///
/// **[`Self::at`] is the deadline; the wakeup only has to arrive at it.** The
/// two are set from different clocks and must not be: `at` is fixed when the
/// deadline is armed, while the wakeup's body — including the `smol::unblock`
/// that parks the thread — does not run until something first polls it, and the
/// only thing that ever does is a read going `Pending`. A daemon that bursts
/// unusable frames for most of the window keeps every read `Ready`, so the
/// wakeup is first polled near the end of it. Given the full timeout to sleep
/// at that point it would fire at nearly twice [`ANSWER_TIMEOUT`], and a
/// documented bound the code does not enforce is not a bound. So the sleep is
/// for whatever is *left* of the window when it is first polled, which lands it
/// on `at` however late that is.
struct AnswerDeadline {
    /// When the request stops waiting. Read before every read
    /// ([`DaemonConnection::receive`]), which is what bounds the talking peer.
    at: Instant,
    /// Fires at [`Self::at`], for the peer that goes quiet with no next read to
    /// check the clock against. Polled lazily, built once — see the type's doc.
    wakeup: Pin<Box<dyn Future<Output = ()>>>,
}

impl AnswerDeadline {
    fn armed(timeout: Duration) -> Self {
        let at = Instant::now() + timeout;
        Self {
            at,
            // `saturating_duration_since` rather than a subtraction: first
            // polled past `at` (possible in principle, though `receive` checks
            // the clock before it polls anything) this is a zero-length sleep
            // and not a panic.
            //
            // Note that a request that arms this and then *succeeds* leaves the
            // thread parked until `at` regardless: dropping the task cannot
            // interrupt a `std::thread::sleep` already running. One pool thread
            // for the rest of the window is the price of not having a
            // cancellable timer here.
            wakeup: Box::pin(
                async move { sleep(at.saturating_duration_since(Instant::now())).await },
            ),
        }
    }
}

/// What one read on the control connection produced.
enum Received {
    /// A frame, for the caller to make of what it will.
    Frame(Frame),
    /// A frame this client could not read, already answered per §2. It is not
    /// nothing: the daemon may have spent a pending request's answer on it,
    /// which is what starts [`ANSWER_TIMEOUT`].
    Discarded,
    /// The caller's deadline passed with neither of the above.
    Expired,
}

/// Find the daemon binary without requiring an install step.
///
/// In order: the [`DAEMON_BIN_ENV`] override, the installed name beside this
/// executable, cargo's name for it beside this executable (a dev build runs
/// from `target/debug`, where the daemon is `ade_session_daemon`), and finally
/// the bare name for `PATH` to resolve — which is also what names it in the
/// spawn error if nothing is there.
fn resolve_binary() -> PathBuf {
    if let Some(override_path) = std::env::var_os(DAEMON_BIN_ENV)
        && !override_path.is_empty()
    {
        return PathBuf::from(override_path);
    }
    if let Ok(executable) = std::env::current_exe()
        && let Some(directory) = executable.parent()
    {
        for name in [DAEMON_BIN, DAEMON_BIN_IN_TARGET] {
            let candidate = directory.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    PathBuf::from(DAEMON_BIN)
}

fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => util::paths::home_dir().join(rest),
        None => PathBuf::from(path),
    }
}

/// The translation from per-session frames to per-workspace events, which needs
/// no daemon and so runs everywhere the crate does.
#[cfg(test)]
mod aggregation {
    use super::*;

    const WORKSPACE: &str = "ade-plural-000001";

    fn info(id: &str, workspace_id: &str, status: SessionStatus) -> proto::SessionInfo {
        proto::SessionInfo {
            id: proto::SessionId::new(id),
            workspace_id: workspace_id.to_owned(),
            agent_kind: "shell".to_owned(),
            instance_label: EXTRA_SESSION_LABEL.to_owned(),
            cwd: "/repos/zed".to_owned(),
            created_at: 0,
            status,
        }
    }

    fn change(frame: Frame, join: &mut SessionJoin) -> SessionChange {
        match status_event(frame, join) {
            Some(DaemonEvent::Session(event)) => {
                assert_eq!(
                    event.id.as_str(),
                    WORKSPACE,
                    "events are keyed by workspace"
                );
                event.change
            }
            other => panic!("expected a session event, got {other:?}"),
        }
    }

    /// **One session's death is not the workspace's.** Two terminals in one
    /// workspace, one exits: the row must stay running, because the other is
    /// still attachable. Only the last death is news.
    #[test]
    fn a_workspace_goes_non_running_only_when_its_last_session_does() {
        let mut join = SessionJoin::from_listing(vec![
            info("s1", WORKSPACE, SessionStatus::Idle),
            info("s2", WORKSPACE, SessionStatus::Working),
        ]);

        assert_eq!(
            change(
                Frame::Exited {
                    session_id: proto::SessionId::new("s1"),
                    exit_code: None,
                },
                &mut join,
            ),
            SessionChange::Status(WorkspaceStatus::Running),
            "a sibling is still running, so the workspace is"
        );

        assert_eq!(
            change(
                Frame::Exited {
                    session_id: proto::SessionId::new("s2"),
                    exit_code: None,
                },
                &mut join,
            ),
            SessionChange::Exited,
            "the last process to go takes the workspace with it"
        );
    }

    /// The same for the kill half: closing one terminal tab removes one session
    /// row, and the workspace is only `Removed` once nothing is left in it.
    #[test]
    fn removing_one_session_leaves_a_workspace_its_siblings_hold_up() {
        let mut join = SessionJoin::from_listing(vec![
            info("s1", WORKSPACE, SessionStatus::Idle),
            info("s2", WORKSPACE, SessionStatus::Idle),
        ]);

        assert_eq!(
            change(
                Frame::Removed {
                    session_id: proto::SessionId::new("s2")
                },
                &mut join,
            ),
            SessionChange::Status(WorkspaceStatus::Running)
        );
        assert_eq!(
            change(
                Frame::Removed {
                    session_id: proto::SessionId::new("s1")
                },
                &mut join,
            ),
            SessionChange::Removed
        );
    }

    /// `Created` says a workspace appeared, so only its first live session may
    /// carry it; the second terminal is a status move on a row that is already
    /// there.
    #[test]
    fn only_the_first_session_of_a_workspace_reports_as_created() {
        let mut join = SessionJoin::default();

        assert_eq!(
            change(
                Frame::Created {
                    session: info("s1", WORKSPACE, SessionStatus::Working),
                    persisted: true,
                    request_id: None,
                },
                &mut join,
            ),
            SessionChange::Created(WorkspaceStatus::Running)
        );
        assert_eq!(
            change(
                Frame::Created {
                    session: info("s2", WORKSPACE, SessionStatus::Working),
                    persisted: true,
                    request_id: None,
                },
                &mut join,
            ),
            SessionChange::Status(WorkspaceStatus::Running)
        );

        // And a workspace that lost every session reports the next one as new
        // again, which is what a recreate is.
        change(
            Frame::Removed {
                session_id: proto::SessionId::new("s1"),
            },
            &mut join,
        );
        change(
            Frame::Removed {
                session_id: proto::SessionId::new("s2"),
            },
            &mut join,
        );
        assert_eq!(
            change(
                Frame::Created {
                    session: info("s3", WORKSPACE, SessionStatus::Working),
                    persisted: true,
                    request_id: None,
                },
                &mut join,
            ),
            SessionChange::Created(WorkspaceStatus::Running)
        );
    }

    /// Two workspaces on one host share the stream but not the aggregate: a
    /// death in one is never held up by a live session in the other.
    #[test]
    fn a_live_session_only_holds_up_its_own_workspace() {
        let mut join = SessionJoin::from_listing(vec![
            info("s1", WORKSPACE, SessionStatus::Idle),
            info("s2", "ade-other-000002", SessionStatus::Idle),
        ]);

        assert_eq!(
            change(
                Frame::Exited {
                    session_id: proto::SessionId::new("s1"),
                    exit_code: None,
                },
                &mut join,
            ),
            SessionChange::Exited
        );
    }
}

/// The control connection's own rules — what may answer a request, and what
/// §6.1 does about a handshake that ends in silence — against a daemon that is
/// a script rather than a process.
///
/// Over loopback and not a Unix socket, deliberately: that is the one transport
/// every platform has, so these run on the Windows client too, where the
/// real-daemon tests further down cannot.
#[cfg(test)]
mod control_connection {
    use super::*;

    /// What the scripted daemon does with one accepted connection.
    enum Script {
        /// Read the client's `Hello` and close without answering — §6.1's
        /// pre-cut signature. The `Hello` is *read* first on purpose: closing
        /// before it lands makes the client fail on the write instead, which
        /// is a different failure with a different rule.
        EofDuringHandshake,
        /// Answer the generation-1 handshake and one `ListSessions` request.
        /// This is the daemon a restarted post-cut app finds when a live agent
        /// made an automatic upgrade unsafe.
        LegacySessionList(Vec<proto::SessionInfo>),
        /// Handshake, wait for one request, then send these frames in order.
        AnswerWith(Vec<Frame>),
        /// [`Script::AnswerWith`], but say nothing at all for this long first —
        /// the slow-but-correct daemon §8.2 permits, which must not be given up
        /// on however long it takes.
        AnswerAfter(Duration, Vec<Frame>),
        /// Answer the three requests a status subscription opens with, then
        /// close so the next script is its reconnect.
        SubscriptionSnapshot {
            sessions: Vec<proto::SessionInfo>,
            workspaces: Vec<proto::WorkspaceInfo>,
            pending: Vec<Frame>,
            /// §8.5 in the handshake: this snapshot may be missing workspaces.
            degraded: bool,
        },
        /// Refuse the handshake the way a daemon older than this client does:
        /// no generation in common, one `Error`, connection over.
        RefuseGeneration,
        /// The connection that outdated daemon *can* read: a generation-2
        /// handshake carrying a polite `Shutdown`. Asserts both, then either
        /// acks or declines — a daemon holding live work says no, and no client
        /// may override that.
        RetireAtGenerationTwo { declining: bool },
        /// Handshake at `generation` and then answer whatever comes, recording
        /// every op the client sent.
        ///
        /// What the generation-gated tests read is that recording: which frames
        /// the client *chose* to put on the wire, which is the sender half of
        /// the contract a receiver-side refusal cannot prove.
        Dialogue {
            generation: u32,
            /// What `list_sessions` answers with. A generation-2 daemon's
            /// combined create has already put the workspace's first session
            /// here; a generation-3 one has not.
            sessions: Vec<proto::SessionInfo>,
            ops: Arc<Mutex<Vec<String>>>,
        },
        /// Handshake, wait for one request, then write this frame over and
        /// over, in pre-encoded bursts, until the client hangs up.
        ///
        /// Pre-encoded and in bursts because the defect this reproduces needs
        /// the client's *read* to be `Ready` on its first poll every single
        /// time, and one `Connection::send` per frame is not that: it
        /// serializes and flushes per frame, so the writer is about as slow as
        /// the reader and the receive buffer keeps running dry. A burst of
        /// already-encoded bytes makes the writer a memcpy and the reader a
        /// JSON decode, which is the asymmetry the real case has.
        KeepTalking(Frame),
    }

    fn ack() -> proto::HelloAck {
        proto::HelloAck {
            daemon_version: "0.0.0-scripted".to_owned(),
            protocol_version: proto::MAX_GENERATION,
            host_os: "test".to_owned(),
            min_generation: proto::MIN_GENERATION,
            max_generation: proto::MAX_GENERATION,
            generation: proto::MAX_GENERATION,
            capabilities: Vec::new(),
            degraded: false,
            instance_id: Some("scripted-daemon".to_owned()),
            binary_hash: None,
            upgrade_ready: None,
            request_id: None,
        }
    }

    /// The wire's own name for a frame's operation — read back off the
    /// envelope rather than matched here, so a test asserts on what was sent.
    fn op_of(frame: &Frame) -> String {
        let payload = ade_session::encode_frame(frame).expect("encoding a client frame");
        let envelope: serde_json::Value =
            serde_json::from_slice(&payload).expect("decoding a client envelope");
        envelope["op"]
            .as_str()
            .expect("every frame names its op")
            .to_owned()
    }

    /// The whole daemon [`Script::Dialogue`] plays: an answer per request, or
    /// `None` for the fire-and-forget ops that are answered with silence.
    fn dialogue_reply(request: &Frame, sessions: &[proto::SessionInfo]) -> Option<Frame> {
        match request {
            Frame::CreateWorkspace {
                root,
                name,
                request_id,
                ..
            } => Some(Frame::Workspace {
                workspace: proto::WorkspaceInfo {
                    id: DIALOGUE_WORKSPACE.to_owned(),
                    name: name.clone().unwrap_or_else(|| "scripted".to_owned()),
                    project_root: root.clone(),
                    created_at: 0,
                    layout_rev: 0,
                    layout: ade_session::LayoutDoc::default(),
                },
                // A generation-2 combined create answers with the session it
                // made; a generation-3 one has none to name.
                sessions: sessions.to_vec(),
                persisted: true,
                request_id: *request_id,
            }),
            Frame::ListSessions { request_id } => Some(Frame::SessionList {
                sessions: sessions.to_vec(),
                request_id: *request_id,
            }),
            Frame::CreateSession { request_id, .. } => Some(Frame::Created {
                session: dialogue_session(),
                persisted: true,
                request_id: *request_id,
            }),
            _ => None,
        }
    }

    const DIALOGUE_WORKSPACE: &str = "ws-1";

    fn dialogue_session() -> proto::SessionInfo {
        proto::SessionInfo {
            id: proto::SessionId::new("session-1"),
            workspace_id: DIALOGUE_WORKSPACE.to_owned(),
            agent_kind: "shell".to_owned(),
            instance_label: DIALOGUE_WORKSPACE.to_owned(),
            cwd: "/repos/zed".to_owned(),
            created_at: 0,
            status: SessionStatus::Idle,
        }
    }

    fn error(code: &str, message: &str, request_id: Option<u64>) -> Frame {
        Frame::Error {
            session_id: None,
            workspace_id: None,
            code: code.to_owned(),
            message: message.to_owned(),
            request_id,
        }
    }

    /// `frame`, on the wire, enough times over that the client cannot drain the
    /// socket faster than [`Script::KeepTalking`] refills it.
    ///
    /// Sized so that returning to the top of the write loop still leaves more
    /// than a socket buffer's worth of frames already queued: it is the gap
    /// between two writes that would let a read go pending, and a pending read
    /// is what the defect needs never to happen.
    fn burst_of(frame: &Frame) -> Vec<u8> {
        let payload = ade_session::encode_frame(frame).expect("encoding a scripted frame");
        let mut burst = Vec::new();
        for _ in 0..4096 {
            burst.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            burst.extend_from_slice(&payload);
        }
        burst
    }

    async fn read_legacy_payload(stream: &mut smol::net::TcpStream) -> serde_json::Value {
        use smol::io::AsyncReadExt as _;

        let mut length = [0; 4];
        stream
            .read_exact(&mut length)
            .await
            .expect("reading a legacy frame length");
        let mut payload = vec![0; u32::from_be_bytes(length) as usize];
        stream
            .read_exact(&mut payload)
            .await
            .expect("reading a legacy frame payload");
        serde_json::from_slice(&payload).expect("decoding a legacy frame")
    }

    async fn write_legacy_payload(stream: &mut smol::net::TcpStream, payload: serde_json::Value) {
        use smol::io::AsyncWriteExt as _;

        let payload = serde_json::to_vec(&payload).expect("encoding a legacy frame");
        stream
            .write_all(&(payload.len() as u32).to_be_bytes())
            .await
            .expect("writing a legacy frame length");
        stream
            .write_all(&payload)
            .await
            .expect("writing a legacy frame payload");
        stream.flush().await.expect("flushing a legacy frame");
    }

    /// [`ANSWER_TIMEOUT`] for these tests. A script on loopback sends when it
    /// is told to and nothing here is slow by accident, so a second past an
    /// unusable frame is already the failure the rule exists to bound — and the
    /// production ten would only be ten seconds of test. [`Script::AnswerAfter`]
    /// is the one deliberate delay, and it is measured in this value.
    const SCRIPTED_ANSWER_TIMEOUT: Duration = Duration::from_secs(1);

    /// Ceiling on a whole call into the backend, [`SCRIPTED_ANSWER_TIMEOUT`]
    /// and a handshake retry included.
    const CALL_TIMEOUT: Duration = Duration::from_secs(10);

    /// Run `call` on its own thread and fail if it has not finished within
    /// [`CALL_TIMEOUT`].
    ///
    /// Every call into the backend blocks its caller, so what a regression here
    /// looks like is a test thread that never returns: without this the harness
    /// kills the whole run on its own timeout and says nothing about which
    /// assertion never got made.
    fn bounded<T: Send + 'static>(what: &str, call: impl FnOnce() -> T + Send + 'static) -> T {
        let (finished, result) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("ade-bounded-call".to_owned())
            .spawn(move || {
                // A closed channel means the test already gave up and is
                // panicking about it; there is nobody left to report to.
                let _ = finished.send(call());
            })
            .expect("spawning the call under test");
        result
            .recv_timeout(CALL_TIMEOUT)
            .unwrap_or_else(|_| panic!("{what} did not finish within {CALL_TIMEOUT:?}"))
    }

    /// A backend pointed at a loopback daemon that plays `scripts`, one per
    /// accepted connection.
    fn scripted_daemon(scripts: Vec<Script>) -> DaemonBackend {
        let (port_sender, port) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("ade-scripted-daemon".to_owned())
            .spawn(move || {
                smol::block_on(async move {
                    let listener = smol::net::TcpListener::bind((LOOPBACK_ADDRESS, 0))
                        .await
                        .expect("binding a loopback listener");
                    port_sender
                        .send(listener.local_addr().expect("the bound address").port())
                        .expect("the test is waiting for the port");
                    for script in scripts {
                        let (stream, _) = listener.accept().await.expect("a client");
                        let mut raw = stream.clone();
                        if let Script::LegacySessionList(sessions) = script {
                            let hello = read_legacy_payload(&mut raw).await;
                            assert_eq!(hello["type"], "Hello");
                            assert_eq!(hello["protocol_version"], 1);
                            write_legacy_payload(
                                &mut raw,
                                serde_json::json!({
                                    "type": "HelloAck",
                                    "daemon_version": "0.1.0-pre-cut",
                                    "protocol_version": 1,
                                    "host_os": "linux",
                                    "binary_hash": "legacy",
                                    "upgrade_ready": false
                                }),
                            )
                            .await;
                            let request = read_legacy_payload(&mut raw).await;
                            assert_eq!(request["type"], "ListSessions");
                            write_legacy_payload(
                                &mut raw,
                                serde_json::json!({
                                    "type": "SessionList",
                                    "sessions": sessions,
                                    "request_id": request["request_id"]
                                }),
                            )
                            .await;
                            continue;
                        }
                        let mut daemon = ade_session::Connection::new(stream);
                        let hello = daemon.recv().await;
                        match &script {
                            // Fatal to the connection at the daemon end, which
                            // is the whole difficulty: dropping `daemon` here is
                            // that close.
                            Script::RefuseGeneration => {
                                daemon
                                    .send(&error(
                                        proto::error_code::UNSUPPORTED_GENERATION,
                                        "this daemon serves generation 2 only",
                                        None,
                                    ))
                                    .await
                                    .expect("refusing the handshake");
                                continue;
                            }
                            Script::RetireAtGenerationTwo { declining } => {
                                let Ok(Frame::Hello(hello)) = hello else {
                                    panic!("the retirement channel must open with a Hello");
                                };
                                assert_eq!(
                                    (hello.min_generation, hello.max_generation),
                                    (RETIRING_GENERATION, RETIRING_GENERATION),
                                    "the polite shutdown rides a generation-2 handshake"
                                );
                                daemon
                                    .send(&Frame::HelloAck(proto::HelloAck {
                                        protocol_version: RETIRING_GENERATION,
                                        min_generation: RETIRING_GENERATION,
                                        max_generation: RETIRING_GENERATION,
                                        generation: RETIRING_GENERATION,
                                        ..ack()
                                    }))
                                    .await
                                    .expect("acking the generation-2 handshake");
                                match daemon.recv().await.expect("the shutdown request") {
                                    Frame::Shutdown { force, .. } => assert!(
                                        !force,
                                        "nothing without a human's click may force a shutdown"
                                    ),
                                    other => panic!("expected Shutdown, got {other:?}"),
                                }
                                let answer = if *declining {
                                    error(proto::error_code::DECLINED, "a session is busy", Some(1))
                                } else {
                                    Frame::ShutdownAck {
                                        request_id: Some(1),
                                    }
                                };
                                daemon.send(&answer).await.expect("answering the shutdown");
                                continue;
                            }
                            Script::Dialogue {
                                generation,
                                sessions,
                                ops,
                            } => {
                                let Ok(Frame::Hello(hello)) = hello else {
                                    panic!("a dialogue opens with a Hello");
                                };
                                assert!(
                                    (hello.min_generation..=hello.max_generation)
                                        .contains(generation),
                                    "the client offered {}..={}, which does not include {generation}",
                                    hello.min_generation,
                                    hello.max_generation,
                                );
                                daemon
                                    .send(&Frame::HelloAck(proto::HelloAck {
                                        protocol_version: *generation,
                                        min_generation: proto::MIN_GENERATION,
                                        max_generation: *generation,
                                        generation: *generation,
                                        ..ack()
                                    }))
                                    .await
                                    .expect("acking the dialogue handshake");
                                while let Ok(request) = daemon.recv().await {
                                    ops.lock()
                                        .unwrap_or_else(|error| error.into_inner())
                                        .push(op_of(&request));
                                    if let Some(reply) = dialogue_reply(&request, sessions) {
                                        daemon.send(&reply).await.expect("answering a request");
                                    }
                                }
                                continue;
                            }
                            _ => {}
                        }
                        if let Script::SubscriptionSnapshot {
                            sessions,
                            workspaces,
                            pending,
                            degraded,
                        } = &script
                        {
                            daemon
                                .send(&Frame::HelloAck(proto::HelloAck {
                                    degraded: *degraded,
                                    ..ack()
                                }))
                                .await
                                .expect("sending the ack");
                            assert!(matches!(
                                daemon.recv().await.expect("Subscribe"),
                                Frame::Subscribe {
                                    request_id: Some(1)
                                }
                            ));
                            assert!(matches!(
                                daemon.recv().await.expect("ListSessions"),
                                Frame::ListSessions {
                                    request_id: Some(2)
                                }
                            ));
                            assert!(matches!(
                                daemon.recv().await.expect("ListWorkspaces"),
                                Frame::ListWorkspaces {
                                    request_id: Some(3)
                                }
                            ));
                            for frame in pending {
                                daemon
                                    .send(frame)
                                    .await
                                    .expect("sending a queued subscription event");
                            }
                            // Reverse the two replies to prove the client waits
                            // for both rather than depending on their order.
                            daemon
                                .send(&Frame::WorkspaceList {
                                    workspaces: workspaces.clone(),
                                    request_id: Some(3),
                                })
                                .await
                                .expect("sending WorkspaceList");
                            daemon
                                .send(&Frame::SessionList {
                                    sessions: sessions.clone(),
                                    request_id: Some(2),
                                })
                                .await
                                .expect("sending SessionList");
                            continue;
                        }
                        let (quiet_for, frames, repeated) = match script {
                            Script::EofDuringHandshake => continue,
                            Script::AnswerWith(frames) => (Duration::ZERO, frames, None),
                            Script::AnswerAfter(quiet_for, frames) => (quiet_for, frames, None),
                            Script::KeepTalking(frame) => (Duration::ZERO, Vec::new(), Some(frame)),
                            Script::LegacySessionList(_)
                            | Script::SubscriptionSnapshot { .. }
                            | Script::RefuseGeneration
                            | Script::Dialogue { .. }
                            | Script::RetireAtGenerationTwo { .. } => unreachable!(),
                        };
                        daemon
                            .send(&Frame::HelloAck(ack()))
                            .await
                            .expect("sending the ack");
                        let _request = daemon.recv().await;
                        sleep(quiet_for).await;
                        for frame in frames {
                            daemon.send(&frame).await.expect("sending a scripted reply");
                        }
                        if let Some(frame) = repeated {
                            let burst = burst_of(&frame);
                            // A failed write is the client hanging up, which is
                            // what this script is waiting for rather than an
                            // error to report.
                            while smol::io::AsyncWriteExt::write_all(&mut raw, &burst)
                                .await
                                .is_ok()
                            {}
                        }
                        // Held open until the client hangs up: closing here
                        // would race the client's read of the last frame.
                        let _ = daemon.recv().await;
                    }
                })
            })
            .expect("spawning the scripted daemon");
        let port = port.recv().expect("the scripted daemon's port");
        let mut backend = DaemonBackend::with_endpoint(Endpoint {
            bin_path: PathBuf::from(DAEMON_BIN),
            address: Address::Named(LocalEndpoint::Loopback(port)),
            state_dir: PathBuf::new(),
            transport: Transport::Direct,
        });
        backend.answer_timeout = SCRIPTED_ANSWER_TIMEOUT;
        backend
    }

    /// A backend on a daemon that serves `generation`, with the ops it receives
    /// recorded for the caller.
    fn dialogue(
        generation: u32,
        sessions: Vec<proto::SessionInfo>,
    ) -> (DaemonBackend, Arc<Mutex<Vec<String>>>) {
        let ops = Arc::new(Mutex::new(Vec::new()));
        let backend = scripted_daemon(vec![Script::Dialogue {
            generation,
            sessions,
            ops: ops.clone(),
        }]);
        (backend, ops)
    }

    fn recorded(ops: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        ops.lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// **The generation-2 create is the combined one.** The daemon answers
    /// `create_workspace` with the record *and* its first session, and the
    /// client's attach-or-create finds that session rather than making a
    /// second one — one shell in the workspace, not two.
    #[test]
    fn a_generation_two_create_leaves_the_daemons_own_first_session_alone() {
        let (backend, ops) = dialogue(2, vec![dialogue_session()]);

        let calls = bounded("the generation-2 create", move || {
            let workspace = backend
                .create_workspace(Path::new("/repos/zed"), Some("zed"))
                .expect("the combined create");
            let spec = SessionSpec::new(workspace.id.clone(), "/repos/zed");
            let attached = backend.attach(&spec).expect("attaching to what it made");
            (workspace, attached)
        });

        assert_eq!(calls.0.id, DIALOGUE_WORKSPACE);
        assert_eq!(calls.1.session_id, "session-1");
        assert_eq!(
            recorded(&ops),
            vec!["create_workspace", "list_sessions"],
            "a second create_session would be a second shell in the workspace"
        );
    }

    /// The generation-3 mirror: the record is minted alone, so the same attach
    /// has to create the first session itself.
    #[test]
    fn a_generation_three_create_still_makes_the_first_session_itself() {
        let (backend, ops) = dialogue(3, Vec::new());

        bounded("the generation-3 create", move || {
            let workspace = backend
                .create_workspace(Path::new("/repos/zed"), Some("zed"))
                .expect("the record-only create");
            let spec = SessionSpec::new(workspace.id, "/repos/zed");
            backend.attach(&spec).expect("attaching to what it made");
        });

        assert!(
            recorded(&ops).contains(&"create_session".to_owned()),
            "{:?}",
            recorded(&ops)
        );
    }

    /// Generation 2 has no focus notion, and the sender's half of that contract
    /// is not to emit the frame — a daemon refusing it is the receiver's half.
    #[test]
    fn focus_is_never_sent_on_a_generation_two_connection() {
        for (generation, expected) in [
            (2, vec!["list_sessions"]),
            (3, vec!["focus_session", "list_sessions"]),
        ] {
            let (backend, ops) = dialogue(generation, Vec::new());
            bounded("focusing then listing", move || {
                backend
                    .focus_session("session-1", "view-1", false)
                    .expect("focus never fails the caller");
                // The list is what proves the focus decision has been made: the
                // daemon reads in order, so anything sent before it is recorded
                // by the time its answer comes back.
                backend.list().expect("listing");
            });
            assert_eq!(recorded(&ops), expected, "at generation {generation}");
        }
    }

    /// The arrow's other fact: the negotiated generation, remembered across the
    /// disconnect that follows.
    #[test]
    fn the_negotiated_generation_is_remembered() {
        let (backend, _ops) = dialogue(2, Vec::new());
        assert_eq!(
            backend.daemon_generation(),
            None,
            "nothing is known before the first handshake"
        );

        let backend = bounded("one request", move || {
            backend.list().expect("listing");
            backend
        });

        assert_eq!(backend.daemon_generation(), Some(2));
        *backend
            .connection
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
        assert_eq!(
            backend.daemon_generation(),
            Some(2),
            "the last known generation outlives the connection"
        );
    }

    /// **The status connection feeds the arrow too.** It handshakes on its own
    /// connection and reconnects on its own schedule, so a daemon upgraded
    /// while the user sits still is met there first — and §6.4's "update on
    /// every successful reconnect" would otherwise wait for a control request
    /// that may never come.
    #[test]
    fn a_status_reconnect_refreshes_the_remembered_generation() {
        let backend = scripted_daemon(vec![Script::SubscriptionSnapshot {
            sessions: Vec::new(),
            workspaces: Vec::new(),
            pending: Vec::new(),
            degraded: false,
        }]);
        // What a control handshake against the daemon this one replaced left
        // behind.
        backend.remembered.generation.store(2, Ordering::Relaxed);

        let (sender, _receiver) = smol::channel::unbounded();
        let mut known = HashMap::new();
        let mut has_snapshot = false;
        let mut announced = None;
        let mut subscribed = false;
        smol::block_on(stream_status_once(
            &backend.endpoint,
            &backend.remembered,
            &sender,
            &mut subscribed,
            &mut announced,
            &mut known,
            &mut has_snapshot,
        ))
        .expect_err("the scripted connection closes after its snapshot");

        assert_eq!(
            backend.daemon_generation(),
            Some(proto::MAX_GENERATION),
            "the subscription's own handshake is as current as a control one"
        );
        assert_eq!(
            backend.instance_id().as_deref(),
            Some("scripted-daemon"),
            "and so is everything else the ack says"
        );
        assert!(
            backend.connection.lock().unwrap().is_none(),
            "no control connection was opened to learn it"
        );
    }

    /// The stream's own state is news only when it *changes*: a reconnect loop
    /// failing the same way every 30 seconds must not re-open a dialog, and a
    /// recovery must not go unsaid.
    #[test]
    fn a_stream_announces_transitions_and_not_retries() {
        let (sender, receiver) = smol::channel::unbounded();
        let mut announced = None;
        let down = |outdated| StreamState::Down {
            message: "no protocol generation is common".to_owned(),
            outdated,
        };

        announce(&sender, &mut announced, down(Some(Outdated::Client)));
        announce(&sender, &mut announced, down(Some(Outdated::Client)));
        announce(&sender, &mut announced, down(Some(Outdated::Daemon)));
        announce(&sender, &mut announced, StreamState::Up);
        announce(&sender, &mut announced, StreamState::Up);

        let events: Vec<_> = std::iter::from_fn(|| receiver.try_recv().ok()).collect();
        assert_eq!(
            events,
            vec![
                DaemonEvent::Down {
                    message: "no protocol generation is common".to_owned(),
                    outdated: Some(Outdated::Client),
                },
                DaemonEvent::Down {
                    message: "no protocol generation is common".to_owned(),
                    outdated: Some(Outdated::Daemon),
                },
                DaemonEvent::Up,
            ]
        );
    }

    /// **The local daemon's one upgrade path**, exercised directly because
    /// nothing this client can meet today proves the direction it needs (see
    /// [`Endpoint::connect`]'s gate and the test below). A daemon older than
    /// this client refuses the handshake and closes, so the polite exit cannot
    /// ride the refused connection: it goes down a fresh one, handshaken at the
    /// generation that daemon does speak.
    #[test]
    fn an_outdated_local_daemon_is_asked_to_exit() {
        let backend = scripted_daemon(vec![Script::RetireAtGenerationTwo { declining: false }]);

        bounded("retiring an outdated local daemon", move || {
            smol::block_on(backend.endpoint.retire_outdated_local_daemon())
        })
        .expect("the daemon accepts the polite shutdown");
    }

    /// A declined shutdown is the daemon's answer, not an obstacle: it holds
    /// live work, and nothing without a human's click may take that away.
    #[test]
    fn an_outdated_local_daemon_that_declines_to_exit_is_left_alone() {
        let backend = scripted_daemon(vec![Script::RetireAtGenerationTwo { declining: true }]);

        let failure = bounded("retiring a daemon that will not exit", move || {
            smol::block_on(backend.endpoint.retire_outdated_local_daemon())
        })
        .expect_err("a daemon that would not exit cannot be retired");

        assert!(
            format!("{failure:#}").contains("declined"),
            "the daemon's own reason reaches the caller: {failure:#}"
        );
    }

    /// **Direction, not prose, decides who gets asked to leave.** A typed
    /// `unsupported_generation` carries no ranges, so it classifies as
    /// client-too-old — and a client that answered *that* with a shutdown
    /// request would be pushing older bytes at a newer daemon. It fails closed:
    /// no second connection, and the refusal reaches the caller intact.
    #[test]
    fn a_daemon_that_may_be_newer_is_never_asked_to_exit() {
        let backend = scripted_daemon(vec![Script::RefuseGeneration]);

        let failure = bounded("listing against a daemon that refuses", move || {
            backend.list()
        })
        .expect_err("a daemon with no common generation cannot be listed");

        assert_eq!(
            refusal_code(&failure),
            Some(proto::error_code::UNSUPPORTED_GENERATION),
            "the daemon's own refusal is what reaches the caller: {failure:#}"
        );
        assert_eq!(
            incompatible_daemon(&failure),
            Some(Outdated::Client),
            "a range-less refusal is read as the client being behind: {failure:#}"
        );
    }

    /// The handshake's `degraded` flag reaches [`DaemonBackend::degraded`] and
    /// stays there for the calls that follow on the same connection — the
    /// plumbing [`ade_workspaces::lifecycle`]'s reconcile drop-guard reads.
    ///
    /// Self-contained rather than [`scripted_daemon`]: that helper's `ack()`
    /// is shared by every other script here, and giving it a knob just for
    /// this one case would touch every call site for no reader's benefit.
    #[test]
    fn a_degraded_handshake_is_remembered_on_the_backend() {
        let (port_sender, port) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("ade-degraded-daemon".to_owned())
            .spawn(move || {
                smol::block_on(async move {
                    let listener = smol::net::TcpListener::bind((LOOPBACK_ADDRESS, 0))
                        .await
                        .expect("binding a loopback listener");
                    port_sender
                        .send(listener.local_addr().expect("the bound address").port())
                        .expect("the test is waiting for the port");
                    let (stream, _) = listener.accept().await.expect("a client");
                    let mut daemon = ade_session::Connection::new(stream);
                    let _hello = daemon.recv().await;
                    daemon
                        .send(&Frame::HelloAck(proto::HelloAck {
                            degraded: true,
                            ..ack()
                        }))
                        .await
                        .expect("sending the degraded ack");
                    let request = daemon.recv().await.expect("ListWorkspaces");
                    let Frame::ListWorkspaces { request_id } = request else {
                        panic!("expected ListWorkspaces, got {request:?}");
                    };
                    daemon
                        .send(&Frame::WorkspaceList {
                            workspaces: Vec::new(),
                            request_id,
                        })
                        .await
                        .expect("sending WorkspaceList");
                })
            })
            .expect("spawning the degraded daemon");
        let port = port.recv().expect("the degraded daemon's port");

        let mut backend = DaemonBackend::with_endpoint(Endpoint {
            bin_path: PathBuf::from(DAEMON_BIN),
            address: Address::Named(LocalEndpoint::Loopback(port)),
            state_dir: PathBuf::new(),
            transport: Transport::Direct,
        });
        backend.answer_timeout = SCRIPTED_ANSWER_TIMEOUT;

        assert!(!backend.degraded(), "nothing is known before a handshake");
        bounded("listing workspaces on a degraded daemon", move || {
            backend.list_workspaces().expect("listing succeeds");
            assert!(
                backend.degraded(),
                "the handshake's degraded flag must reach the backend"
            );
        });
    }

    /// **An error frame is only this request's answer when it carries this
    /// request's `rid`.** One with no `rid` is legal at any time — the daemon
    /// emits one when a fire-and-forget `write` fails — and one with somebody
    /// else's is a daemon bug; neither may resolve a request the daemon has
    /// not answered yet, or a listing turns into "a write failed" for reasons
    /// no user can connect to what they clicked.
    #[test]
    fn an_error_without_this_requests_rid_is_not_its_answer() {
        let backend = scripted_daemon(vec![Script::AnswerWith(vec![
            error("internal", "a write failed", None),
            error("not_found", "some other request's problem", Some(4242)),
            Frame::SessionList {
                sessions: Vec::new(),
                request_id: Some(1),
            },
        ])]);

        let listed = bounded("the listing", move || backend.daemon_sessions());
        assert!(
            listed
                .expect("the listing, not the unsolicited error")
                .is_empty()
        );
    }

    /// **A reply that can never match must not be waited on forever.** Waiting
    /// past it is right — the daemon may still be about to answer — but the
    /// daemon has spent a reply on it, so the wait needs an end: it happens
    /// under the connection lock, and a request that never returns takes every
    /// later request to that host with it. §3.3 forbids the case that produces
    /// this (an `op` above the negotiated generation), which is exactly why a
    /// client must not depend on it.
    #[test]
    fn a_reply_that_can_never_be_this_requests_answer_ends_it_in_bounded_time() {
        // Somebody else's `rid` stands in for the reply a newer daemon sends
        // and this build cannot decode: both are frames `want` can never
        // accept, both arm the same rule, and after both there is nothing else
        // coming.
        let backend = scripted_daemon(vec![Script::AnswerWith(vec![Frame::SessionList {
            sessions: Vec::new(),
            request_id: Some(4242),
        }])]);

        let failure = bounded("the request", move || {
            format!(
                "{:#}",
                backend
                    .daemon_sessions()
                    .expect_err("a reply that cannot match is not an answer to wait on")
            )
        });
        assert!(
            failure.contains("could not answer request"),
            "giving up has to say what went unanswered: {failure}"
        );
    }

    /// **And a daemon that never stops talking runs out of time too.** The
    /// likelier bug shape by far: something answers a request with one frame
    /// this client cannot use, then keeps writing — unsolicited `internal`
    /// errors are legal at any time (§2) and a daemon in a failure loop emits
    /// them back to back.
    ///
    /// It has to be its own test because a wakeup alone does not cover it.
    /// `smol::future::or` returns on its first future without ever polling its
    /// second, [`DaemonConnection`] reads straight off the stream, and a peer
    /// writing faster than this side decodes keeps the receive buffer
    /// non-empty — so every read is `Ready` on its first poll, the wakeup is
    /// never reached, and the request waits forever holding the connection
    /// lock. [`DaemonConnection::receive`] reads the clock itself for exactly
    /// this.
    #[test]
    fn a_daemon_that_never_stops_talking_runs_out_of_time_as_well() {
        let backend = scripted_daemon(vec![Script::KeepTalking(error(
            "internal",
            "a write failed",
            None,
        ))]);

        let failure = bounded("the request", move || {
            format!(
                "{:#}",
                backend
                    .daemon_sessions()
                    .expect_err("a daemon that only ever talks past a request is not answering it")
            )
        });
        assert!(
            failure.contains("could not answer request"),
            "giving up has to say what went unanswered: {failure}"
        );
    }

    /// **And the window is the one the constant names, not up to twice it.**
    /// The two shapes above are the ends of a range, and in the middle is a
    /// daemon that talks for part of the window and then stops: nothing polls
    /// the wakeup while it talks, so the wakeup is first polled late. Given the
    /// whole timeout to sleep at that point it fires that much *after* the
    /// deadline, and a request the docs bound at [`ANSWER_TIMEOUT`] is bounded
    /// at nearly two of them.
    ///
    /// Tested on [`AnswerDeadline`] rather than through the scripted daemon
    /// because it is a property of the two clocks and not of any traffic
    /// pattern: the late first poll is the whole input, and spelling it as a
    /// sleep makes the failure a wide margin instead of a race against how fast
    /// loopback happens to be today.
    #[test]
    fn a_deadline_first_polled_late_still_expires_when_it_said_it_would() {
        let timeout = Duration::from_secs(1);
        let mut deadline = AnswerDeadline::armed(timeout);

        // The talking peer: every read was `Ready`, so nothing touched the
        // wakeup until three quarters of the window had already gone.
        std::thread::sleep(timeout / 4 * 3);
        smol::block_on(&mut deadline.wakeup);

        let overshoot = Instant::now().saturating_duration_since(deadline.at);
        assert!(
            overshoot < timeout / 2,
            "the wakeup has to land on the deadline, not a whole timeout past \
             where it was first polled: {overshoot:?} late"
        );
    }

    /// **The clock starts at the first unusable frame, not at the request.**
    /// The distinction is the whole of [`ANSWER_TIMEOUT`]'s carve-out: §8.2 puts
    /// a FIFO persist worker in front of an ack, so a daemon can legitimately
    /// take longer than this bound to answer and still be answering. Armed at
    /// the request instead, this listing would fail; armed at a frame that never
    /// came, it waits and gets its answer.
    #[test]
    fn the_clock_starts_at_the_first_unusable_frame_and_not_at_the_request() {
        let backend = scripted_daemon(vec![Script::AnswerAfter(
            SCRIPTED_ANSWER_TIMEOUT * 2,
            vec![Frame::SessionList {
                sessions: Vec::new(),
                request_id: Some(1),
            }],
        )]);

        let listed = bounded("the slow listing", move || backend.daemon_sessions());
        assert!(listed.expect("a slow answer is still an answer").is_empty());
    }

    /// **Giving up drops the connection, and the next call reconnects.** It
    /// cannot reuse it: `read_frame` takes the length prefix and the payload in
    /// two reads, so a read abandoned between them leaves the stream unable to
    /// find its own next frame, and the answer this request stopped waiting for
    /// would land on the next one. The daemon reads the close as a detach, which
    /// kills nothing — so the cost is one reconnect and the host keeps working.
    #[test]
    fn giving_up_on_a_request_drops_the_connection_and_the_next_one_reconnects() {
        let backend = scripted_daemon(vec![
            // Somebody else's `rid`: a frame `want` can never accept, and
            // nothing after it.
            Script::AnswerWith(vec![Frame::SessionList {
                sessions: Vec::new(),
                request_id: Some(4242),
            }]),
            // The second connection, and so the second request id.
            Script::AnswerWith(vec![Frame::SessionList {
                sessions: Vec::new(),
                request_id: Some(2),
            }]),
        ]);

        let backend = bounded("the abandoned request", move || {
            backend
                .daemon_sessions()
                .expect_err("a reply that cannot match is not an answer to wait on");
            backend
        });
        let listed = bounded("the listing after it", move || backend.daemon_sessions());
        assert!(
            listed
                .expect("the host is still usable after one request gave up")
                .is_empty()
        );
    }

    #[test]
    fn a_failed_upgrade_invalidates_the_control_connection_and_host_ensure() {
        let source = scripted_daemon(vec![Script::AnswerWith(vec![Frame::SessionList {
            sessions: Vec::new(),
            request_id: Some(1),
        }])]);
        source
            .daemon_sessions()
            .expect("establishing the control connection");
        let connection = source
            .connection
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
            .expect("the listing keeps its connection cached");

        let backend = DaemonBackend::remote("upgrade-error.test")
            .expect("constructing a remote backend contacts no host");
        let Transport::Forwarded(link) = &backend.endpoint.transport else {
            panic!("a remote backend uses a forwarded transport");
        };
        link.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .daemon_ensured = true;
        let mut slot = backend
            .connection
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *slot = Some(connection);

        let error = backend
            .finish_upgrade(
                &mut slot,
                Err(anyhow::anyhow!("deployment failed after shutdown")),
            )
            .expect_err("the upgrade error is preserved");
        assert_eq!(format!("{error:#}"), "deployment failed after shutdown");
        assert!(slot.is_none(), "the old daemon connection is unusable");
        drop(slot);
        assert!(
            !link
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .daemon_ensured,
            "the next request must ensure the replacement daemon"
        );
    }

    /// And the matching one is the answer, code included: `not_found` and
    /// `internal` are operationally different answers, so the code travels
    /// with the prose rather than being dropped on the floor.
    #[test]
    fn an_error_carrying_this_requests_rid_fails_it_with_its_code() {
        let backend = scripted_daemon(vec![Script::AnswerWith(vec![error(
            "not_found",
            "no such session",
            Some(1),
        )])]);

        let failure = format!(
            "{:#}",
            backend
                .daemon_sessions()
                .expect_err("an error against this rid answers the request")
        );
        assert!(
            failure.contains("not_found: no such session"),
            "the code has to reach the caller: {failure}"
        );
    }

    /// §6.1: a handshake that ends in EOF buys exactly one retry, on a *fresh*
    /// connection. A daemon that was merely still binding its socket is what
    /// this recovers, and it must recover it silently.
    #[test]
    fn a_handshake_that_ends_in_eof_is_retried_once() {
        let backend = scripted_daemon(vec![
            Script::EofDuringHandshake,
            Script::AnswerWith(vec![Frame::SessionList {
                sessions: Vec::new(),
                request_id: Some(1),
            }]),
        ]);

        let listed = bounded("the retried handshake", move || backend.daemon_sessions());
        assert!(listed.expect("the retry gets a healthy daemon").is_empty());
    }

    /// **A degraded daemon's silence is not a kill.** Its ledger is read-only
    /// and its listing may omit live workspaces, and the removal a resubscribe
    /// would synthesize from that deletes the client's row with none of the
    /// reconcile sweep's guards.
    ///
    /// So: nothing on the degraded snapshot, the omitted id retained across it,
    /// exactly one removal when a *healthy* snapshot still omits it — and the
    /// daemon's own explicit removal, queued behind the degraded snapshot,
    /// through untouched.
    #[test]
    fn a_degraded_snapshot_synthesizes_no_removals() {
        fn workspace(id: &str) -> proto::WorkspaceInfo {
            proto::WorkspaceInfo {
                id: id.to_owned(),
                name: id.to_owned(),
                project_root: "/worktree".to_owned(),
                created_at: 1,
                layout_rev: 1,
                layout: LayoutDoc::empty(),
            }
        }

        let backend = scripted_daemon(vec![
            Script::SubscriptionSnapshot {
                sessions: Vec::new(),
                workspaces: vec![workspace("omitted"), workspace("explicitly-removed")],
                pending: Vec::new(),
                degraded: false,
            },
            // The read-only ledger: both are missing, and one of them really
            // was removed — which only the explicit event may say.
            Script::SubscriptionSnapshot {
                sessions: Vec::new(),
                workspaces: Vec::new(),
                pending: vec![Frame::WorkspaceRemoved {
                    workspace_id: "explicitly-removed".to_owned(),
                    persisted: true,
                    request_id: None,
                }],
                degraded: true,
            },
            Script::SubscriptionSnapshot {
                sessions: Vec::new(),
                workspaces: Vec::new(),
                pending: Vec::new(),
                degraded: false,
            },
        ]);
        let (sender, receiver) = smol::channel::unbounded();
        let mut known = HashMap::new();
        let mut has_snapshot = false;
        let mut after_degraded = Vec::new();
        let remembered = Remembered::default();
        let mut announced = None;

        for pass in 0..3 {
            let mut subscribed = false;
            smol::block_on(stream_status_once(
                &backend.endpoint,
                &remembered,
                &sender,
                &mut subscribed,
                &mut announced,
                &mut known,
                &mut has_snapshot,
            ))
            .expect_err("the scripted connection closes after its snapshot");
            assert!(subscribed);
            if pass == 1 {
                after_degraded = std::iter::from_fn(|| receiver.try_recv().ok()).collect();
                assert!(
                    known.contains_key("omitted"),
                    "an id a degraded listing did not name is still known: {known:?}"
                );
            }
        }
        let after_healthy: Vec<_> = std::iter::from_fn(|| receiver.try_recv().ok()).collect();

        let removals = |events: &[DaemonEvent], id: &str| {
            events
                .iter()
                .filter(|event| {
                    matches!(event, DaemonEvent::WorkspaceRemoved { workspace_id }
                        if workspace_id == id)
                })
                .count()
        };
        assert_eq!(
            removals(&after_degraded, "omitted"),
            0,
            "a degraded snapshot may not delete a row: {after_degraded:?}"
        );
        assert_eq!(
            removals(&after_degraded, "explicitly-removed"),
            1,
            "the daemon's own removal still passes: {after_degraded:?}"
        );
        assert_eq!(
            removals(&after_healthy, "omitted"),
            1,
            "the first authoritative absence is the removal: {after_healthy:?}"
        );
        assert_eq!(
            removals(&after_healthy, "explicitly-removed"),
            0,
            "the explicit removal took it out of `known`: {after_healthy:?}"
        );
    }

    #[test]
    fn reconnect_repairs_missed_layouts_and_workspace_removals() {
        fn workspace(id: &str, rev: u64, path: &str) -> proto::WorkspaceInfo {
            proto::WorkspaceInfo {
                id: id.to_owned(),
                name: id.to_owned(),
                project_root: "/worktree".to_owned(),
                created_at: 1,
                layout_rev: rev,
                layout: LayoutDoc::new(ade_session::LayoutNode::leaf(vec![
                    ade_session::Tab::Editor {
                        path: path.to_owned(),
                    },
                ])),
            }
        }

        let backend = scripted_daemon(vec![
            Script::SubscriptionSnapshot {
                sessions: Vec::new(),
                workspaces: Vec::new(),
                pending: vec![Frame::WorkspaceRemoved {
                    workspace_id: "removed-before-first-list".to_owned(),
                    persisted: true,
                    request_id: None,
                }],
                degraded: false,
            },
            Script::SubscriptionSnapshot {
                sessions: Vec::new(),
                workspaces: vec![
                    workspace("removed", 1, "/old"),
                    workspace("changed", 1, "/before"),
                    workspace("recreated", 9, "/old-incarnation"),
                    workspace("repaired", 3, "/before-repair"),
                ],
                pending: Vec::new(),
                degraded: false,
            },
            Script::SubscriptionSnapshot {
                sessions: Vec::new(),
                workspaces: vec![
                    workspace("changed", 2, "/after"),
                    workspace("recreated", 1, "/new-incarnation"),
                    workspace("repaired", 3, "/after-repair"),
                ],
                pending: vec![
                    Frame::LayoutChanged {
                        workspace_id: "changed".to_owned(),
                        layout: LayoutDoc::empty(),
                        rev: 1,
                        persisted: true,
                        request_id: None,
                    },
                    Frame::WorkspaceRemoved {
                        workspace_id: "removed".to_owned(),
                        persisted: true,
                        request_id: None,
                    },
                ],
                degraded: false,
            },
        ]);
        let (sender, receiver) = smol::channel::unbounded();
        let mut known = HashMap::new();
        let mut has_snapshot = false;

        let remembered = Remembered::default();
        let mut announced = None;
        for _ in 0..3 {
            let mut subscribed = false;
            smol::block_on(stream_status_once(
                &backend.endpoint,
                &remembered,
                &sender,
                &mut subscribed,
                &mut announced,
                &mut known,
                &mut has_snapshot,
            ))
            .expect_err("the scripted connection closes after its snapshot");
            assert!(subscribed);
        }

        let events: Vec<_> = std::iter::from_fn(|| receiver.try_recv().ok()).collect();
        assert_eq!(
            events
                .iter()
                .filter(|event| **event == DaemonEvent::Up)
                .count(),
            1,
            "the stream says it is up once, after the snapshot that brought it back: {events:?}"
        );
        assert!(events.contains(&DaemonEvent::WorkspaceRemoved {
            workspace_id: "removed".to_owned(),
        }));
        assert!(events.contains(&DaemonEvent::WorkspaceRemoved {
            workspace_id: "removed-before-first-list".to_owned(),
        }));
        assert!(events.iter().any(|event| matches!(
            event,
            DaemonEvent::Layout(LayoutEvent {
                workspace_id,
                rev: 2,
                ..
            }) if workspace_id == "changed"
        )));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    DaemonEvent::Layout(LayoutEvent {
                        workspace_id,
                        ..
                    }) if workspace_id == "changed"
                ))
                .count(),
            2,
            "the queued pre-snapshot revision must not be replayed"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    DaemonEvent::WorkspaceRemoved { workspace_id }
                        if workspace_id == "removed"
                ))
                .count(),
            1,
            "a removal already represented by a reconnect snapshot is not repeated"
        );
        // These are incarnation boundaries, not kills. The reset carries the
        // authoritative lower/equal revision without discarding the window's
        // workspace identity between two events.
        for (workspace_id, expected_rev, expected_path) in [
            ("recreated", 1, "/new-incarnation"),
            ("repaired", 3, "/after-repair"),
        ] {
            let expected_layout = workspace(workspace_id, expected_rev, expected_path).layout;
            assert!(
                events.iter().any(|event| matches!(
                    event,
                    DaemonEvent::WorkspaceReset(LayoutEvent {
                        workspace_id: reset,
                        layout,
                        rev,
                    }) if reset == workspace_id
                        && *rev == expected_rev
                        && layout == &expected_layout
                )),
                "a replacement must reset the existing window atomically"
            );
        }
        assert_eq!(
            known
                .iter()
                .map(|(workspace_id, workspace)| (workspace_id.as_str(), workspace.rev))
                .collect::<HashMap<_, _>>(),
            HashMap::from([("changed", 2), ("recreated", 1), ("repaired", 3)])
        );
    }

    /// A busy pre-cut daemon cannot be used by this client and cannot be
    /// replaced without terminating its sessions. The connect flow needs this
    /// error so it can ask for that consent instead of opening a competing
    /// plain shell.
    #[test]
    fn a_busy_pre_cut_daemon_requires_a_destructive_upgrade() {
        let session = proto::SessionInfo {
            id: proto::SessionId::new("still-running"),
            workspace_id: "ade-live-workspace".to_owned(),
            agent_kind: "codex".to_owned(),
            instance_label: "main".to_owned(),
            cwd: "/worktree".to_owned(),
            created_at: 1,
            status: SessionStatus::Working,
        };
        let backend = scripted_daemon(vec![
            Script::EofDuringHandshake,
            Script::EofDuringHandshake,
            Script::LegacySessionList(vec![session]),
        ]);

        let failure = bounded("the incompatible reconnect", move || {
            backend.daemon_sessions()
        })
        .expect_err("the incompatible daemon must be surfaced to the connect flow");
        assert_eq!(
            incompatible_daemon(&failure),
            Some(Outdated::Daemon),
            "unexpected failure: {failure:#}"
        );
    }
}

#[cfg(test)]
mod ensure_report_tests {
    use super::{EnsureReport, Generations, proto};

    /// A daemon from before binary identity: two tokens, nothing else. The
    /// conservative reading — no hash, not ready — is what keeps the upgrade
    /// path away from it.
    #[test]
    fn a_legacy_ensure_line_reads_as_untouchable() {
        let report = EnsureReport::parse("ade-daemon 0.1.0");
        assert_eq!(
            report,
            EnsureReport {
                hash: None,
                upgrade_ready: false,
                generations: Generations::Absent,
            }
        );
    }

    #[test]
    fn a_full_ensure_line_carries_hash_and_readiness() {
        let report = EnsureReport::parse(&format!(
            "ade-daemon 0.1.0 hash={} upgrade_ready=true",
            "ab".repeat(32)
        ));
        assert_eq!(report.hash.as_deref(), Some("ab".repeat(32).as_str()));
        assert!(report.upgrade_ready);
    }

    #[test]
    fn a_busy_daemon_reports_not_ready() {
        let report = EnsureReport::parse("ade-daemon 0.1.0 hash=abc123 upgrade_ready=false");
        assert_eq!(report.hash.as_deref(), Some("abc123"));
        assert!(!report.upgrade_ready);
    }

    /// Unknown tokens are someone newer talking; they must never break the
    /// parse, and an empty hash value counts as no hash at all.
    #[test]
    fn stray_and_empty_tokens_are_ignored() {
        let report = EnsureReport::parse("ade-daemon 0.1.0 hash= upgrade_ready=true fleet=blue");
        assert_eq!(
            report,
            EnsureReport {
                hash: None,
                upgrade_ready: true,
                generations: Generations::Absent,
            }
        );
    }

    /// The token a daemon of this generation or newer appends, and the shapes
    /// that are not it. A range nobody can parse is **malformed**, which is not
    /// absence — see [`Generations`].
    #[test]
    fn the_generation_range_is_read_when_the_daemon_names_one() {
        assert_eq!(
            EnsureReport::parse("ade-daemon 0.1.0 hash=abc upgrade_ready=true generations=2..=3")
                .generations,
            Generations::Valid(2, 3)
        );
        for line in [
            "ade-daemon 0.1.0 generations=",
            "ade-daemon 0.1.0 generations=3",
            "ade-daemon 0.1.0 generations=3..=2",
            "ade-daemon 0.1.0 generations=x..=y",
            "ade-daemon 0.1.0 generations=4294967296..=4294967297",
            // Two tokens: readable one by one, and unreadable as a window.
            "ade-daemon 0.1.0 generations=2..=3 generations=4..=5",
        ] {
            assert_eq!(
                EnsureReport::parse(line).generations,
                Generations::Malformed,
                "unusable range accepted from {line:?}"
            );
        }
    }

    /// The deploy guard. A daemon whose floor is above this client's ceiling is
    /// newer, and replacing it with these bytes would be a downgrade that
    /// destroys the sessions it holds.
    #[test]
    fn a_daemon_newer_than_this_client_is_never_deployed_over() {
        let refused = super::refuse_downgrade(&EnsureReport::parse(&format!(
            "ade-daemon 0.1.0 hash=abc upgrade_ready=true generations={}..={}",
            proto::MAX_GENERATION + 1,
            proto::MAX_GENERATION + 2
        )))
        .expect_err("a newer daemon must not be replaced");
        assert_eq!(
            super::incompatible_daemon(&refused),
            Some(super::Outdated::Client),
            "the refusal must surface as client-too-old, never as an upgrade offer"
        );

        // Absent range: a daemon predating the token, definitionally older.
        // Overlapping and older ranges: an upgrade, as before.
        for line in [
            "ade-daemon 0.1.0 hash=abc upgrade_ready=true".to_owned(),
            format!(
                "ade-daemon 0.1.0 hash=abc upgrade_ready=true generations={}..={}",
                proto::MIN_GENERATION,
                proto::MAX_GENERATION
            ),
            format!(
                "ade-daemon 0.1.0 hash=abc upgrade_ready=true generations={}..={}",
                proto::MIN_GENERATION,
                proto::MIN_GENERATION
            ),
        ] {
            super::refuse_downgrade(&EnsureReport::parse(&line))
                .unwrap_or_else(|error| panic!("deploy wrongly refused for {line:?}: {error:#}"));
        }
    }

    /// Fail closed on a token this build cannot read: only a daemon newer than
    /// this one can spell the window in a way this one does not know, so the
    /// unreadable case takes the same no-deploy exit as a proven newer range.
    #[test]
    fn a_generation_token_that_does_not_parse_refuses_the_deploy() {
        for line in [
            "ade-daemon 0.1.0 hash=abc upgrade_ready=true generations=4..=5,6",
            "ade-daemon 0.1.0 hash=abc upgrade_ready=true generations=nonsense",
            "ade-daemon 0.1.0 hash=abc upgrade_ready=true generations=2..=3 generations=2..=3",
        ] {
            let refused = super::refuse_downgrade(&EnsureReport::parse(line))
                .expect_err("an unreadable window must not be deployed over");
            assert_eq!(
                super::incompatible_daemon(&refused),
                Some(super::Outdated::Client),
                "{line:?} must surface as client-too-old, never as an upgrade offer"
            );
        }
    }
}

/// What the sidebar's "upgrade host daemon" arrow is drawn from.
#[cfg(test)]
mod daemon_freshness_tests {
    use super::*;

    fn link() -> HostLink {
        HostLink::new(
            ade_session::SshHost::new("fevm1"),
            LocalEndpoint::Loopback(0),
        )
    }

    /// The three states the arrow is drawn from, in the order a host moves
    /// through them. Unknown is the one that matters: a host nobody has
    /// compared claims no update, rather than an arrow on every row from the
    /// first frame.
    #[test]
    fn only_a_hash_mismatch_claims_an_update() {
        let link = link();
        assert!(!link.daemon_stale(), "nothing is known before a probe");
        link.note_hash_verdict(true);
        assert!(link.daemon_stale());
        link.note_hash_verdict(false);
        assert!(!link.daemon_stale(), "the upgrade puts the arrow away");
    }

    /// The observer hears every change and nothing else.
    ///
    /// Both halves matter. Missing a change leaves the arrow showing the last
    /// answer a render happened to catch; announcing a repeat costs a repaint
    /// of every sidebar in the process, and a reconnect re-confirms every host
    /// it touches.
    #[test]
    fn the_observer_hears_changes_and_only_changes() {
        let link = link();
        let announcements = Arc::new(AtomicU64::new(0));
        link.observe_daemon_freshness(Arc::new({
            let announcements = announcements.clone();
            move || {
                announcements.fetch_add(1, Ordering::Relaxed);
            }
        }));
        let announced = || announcements.load(Ordering::Relaxed);

        link.note_hash_verdict(true);
        assert_eq!(announced(), 1, "the first verdict is news");
        link.note_hash_verdict(true);
        assert_eq!(announced(), 1, "a probe that confirms it is not");
        link.note_hash_verdict(false);
        assert_eq!(announced(), 2, "and the upgrade is news again");
    }

    /// The verdict is already recorded when the observer runs, because the
    /// observer's whole job is to make something re-read it. Announcing first
    /// would have a sidebar repaint from the answer it was already showing.
    #[test]
    fn the_observer_runs_after_the_verdict_is_readable() {
        let link = Arc::new(link());
        let seen = Arc::new(Mutex::new(None));
        link.observe_daemon_freshness(Arc::new({
            // Weak, because the link owns the observer: a strong handle back
            // would be a cycle, and this test would leak a `HostLink` per run.
            let link = Arc::downgrade(&link);
            let seen = seen.clone();
            move || {
                if let Some(link) = link.upgrade() {
                    *seen.lock().unwrap_or_else(|error| error.into_inner()) =
                        Some(link.daemon_stale());
                }
            }
        }));

        link.note_hash_verdict(true);
        assert_eq!(
            *seen.lock().unwrap_or_else(|error| error.into_inner()),
            Some(true)
        );
    }

    /// The negotiated generation rides the same seam, so a stationary sidebar learns
    /// about it — and, like the arrow's verdict, only when it *changes*.
    #[test]
    fn a_new_generation_wakes_the_sidebar_and_a_repeated_one_does_not() {
        let backend = DaemonBackend::with_endpoint(Endpoint {
            bin_path: PathBuf::from(DAEMON_BIN),
            address: Address::Named(LocalEndpoint::Loopback(0)),
            state_dir: PathBuf::new(),
            transport: Transport::Forwarded(Arc::new(HostLink::new(
                ade_session::SshHost::new("fevm1"),
                LocalEndpoint::Loopback(0),
            ))),
        });
        let woken = Arc::new(AtomicU64::new(0));
        backend.observe_daemon_freshness(Arc::new({
            let woken = woken.clone();
            move || {
                woken.fetch_add(1, Ordering::Relaxed);
            }
        }));

        let ack = |generation| proto::HelloAck {
            daemon_version: "0.0.0".to_owned(),
            protocol_version: generation,
            host_os: "test".to_owned(),
            min_generation: proto::MIN_GENERATION,
            max_generation: generation,
            generation,
            capabilities: Vec::new(),
            degraded: false,
            instance_id: None,
            binary_hash: None,
            upgrade_ready: None,
            request_id: None,
        };

        assert_eq!(backend.daemon_generation(), None);
        backend.remember(&ack(2));
        assert_eq!(backend.daemon_generation(), Some(2));
        assert_eq!(woken.load(Ordering::Relaxed), 1);
        backend.remember(&ack(2));
        assert_eq!(woken.load(Ordering::Relaxed), 1, "a reconnect is not news");
        backend.remember(&ack(3));
        assert_eq!(backend.daemon_generation(), Some(3));
        assert_eq!(woken.load(Ordering::Relaxed), 2, "the arrow has to clear");
    }
}

#[cfg(test)]
mod pre_cut_fallback_tests {
    use super::{
        DaemonRefusal, GenerationSkew, Outdated, incompatible_daemon, is_pre_cut_daemon,
        pre_cut_kill_script, proto,
    };
    use ade_session::PRE_CUT_DIAGNOSIS;
    use anyhow::anyhow;

    /// The diagnosis is produced as a context string ([`super::handshaken`]),
    /// so the detector has to find it anywhere in the chain — and must not
    /// fire on an ordinary handshake failure.
    #[test]
    fn the_detector_matches_the_diagnosis_and_nothing_else() {
        let diagnosed = anyhow!("reading frame length: unexpected end of file")
            .context(PRE_CUT_DIAGNOSIS)
            .context("handshaking for the shutdown request");
        assert!(is_pre_cut_daemon(&diagnosed));
        assert_eq!(incompatible_daemon(&diagnosed), Some(Outdated::Daemon));

        let ordinary = anyhow!("connection refused").context("spawning the shutdown channel");
        assert!(!is_pre_cut_daemon(&ordinary));
        assert_eq!(
            incompatible_daemon(&ordinary),
            None,
            "a host that could not be reached still gets today's fallback"
        );
    }

    /// The typed refusal, classified structurally — the code, never the prose,
    /// and never the code of some *other* refusal.
    #[test]
    fn a_typed_generation_refusal_blames_the_client() {
        let refused = anyhow::Error::new(DaemonRefusal {
            code: proto::error_code::UNSUPPORTED_GENERATION.to_owned(),
            message: "no protocol generation is common".to_owned(),
        })
        .context("the session daemon refused the handshake")
        .context("listing the workspaces on winbox");
        // A refusal names no range, and a daemon under this build's floor
        // could not have sent one: what refused us is newer, and nothing may
        // deploy older bytes over it.
        assert_eq!(incompatible_daemon(&refused), Some(Outdated::Client));

        let declined = anyhow::Error::new(DaemonRefusal {
            code: proto::error_code::DECLINED.to_owned(),
            message: "a session is busy".to_owned(),
        });
        assert_eq!(incompatible_daemon(&declined), None);
    }

    /// Both directions, off the numbers the ack carried.
    #[test]
    fn a_named_range_decides_which_end_is_behind() {
        let daemon_behind = anyhow::Error::new(GenerationSkew {
            offered: (4, 5),
            daemon: Some((2, 3)),
        })
        .context("handshaking with the session daemon");
        assert_eq!(
            incompatible_daemon(&daemon_behind),
            Some(Outdated::Daemon),
            "a window that ends below ours is the one an upgrade fixes"
        );

        let client_behind = anyhow::Error::new(GenerationSkew {
            offered: (2, 3),
            daemon: Some((4, 5)),
        });
        assert_eq!(incompatible_daemon(&client_behind), Some(Outdated::Client));
    }

    /// The script's guards are load-bearing: the pidfile check, the
    /// command-name check before any signal, and the socket removal that
    /// unlocks deployment. Pin them so a rewrite cannot silently drop one.
    #[test]
    fn the_kill_script_guards_and_quotes() {
        let script = pre_cut_kill_script(
            "/home/user name/.ade/daemon",
            "/home/user name/.ade/daemon.sock",
        );
        assert!(script.contains("pidfile='/home/user name/.ade/daemon/daemon.pid'"));
        assert!(script.contains("socket='/home/user name/.ade/daemon.sock'"));
        assert!(script.contains("stop the daemon by hand"));
        assert!(script.contains("*ade-daemon*"));
        assert!(script.contains("rm -f \"$socket\""));
    }

    #[test]
    #[cfg(unix)]
    fn stale_daemon_recovery_is_scoped_to_terminal_groups_in_the_worktree() {
        let script =
            super::stale_daemon_recovery_script(std::path::Path::new("/home/user name/repo"))
                .expect("the remote worktree path should produce a script");

        assert!(script.contains("root='/home/user name/repo'"));
        assert!(script.contains("\"$root\"|\"$root\"/*"));
        assert!(script.contains("tty=$5"));
        assert!(script.contains("kill -HUP -\"$group\""));
        assert!(script.contains("kill -KILL -\"$group\""));
        assert!(script.contains("refusing to recover every terminal on the host"));
        assert!(
            smol::block_on(
                smol::process::Command::new("sh")
                    .args(["-n", "-c", &script])
                    .status()
            )
            .expect("sh should parse the recovery script")
            .success()
        );
    }
}

/// The Windows shape of this machine's endpoint, which is the whole of what
/// this crate had to learn for local workspaces there: a proxy, a state dir,
/// and no endpoint flag anywhere — because `--socket` is refused by name on
/// that binary and `--pipe` would only repeat what it derives.
///
/// String-level and daemon-free on purpose: what can go wrong here is an argv
/// the daemon rejects, and that is decided before anything is spawned.
#[cfg(all(test, windows))]
mod windows_local_tests {
    use super::*;

    #[test]
    fn the_local_endpoint_is_a_proxy_that_names_no_pipe() {
        let endpoint = Endpoint::local();

        assert!(matches!(endpoint.transport, Transport::Proxy));
        assert_eq!(endpoint.address, Address::DefaultPipe);
        assert_eq!(
            endpoint.proxy_argv(),
            vec![
                endpoint.bin_path.display().to_string(),
                "--stdio-proxy".to_owned(),
                "--state-dir".to_owned(),
                expand_home(DEFAULT_STATE_DIR).display().to_string(),
            ]
        );
        // The flag this platform's `ade-daemon` refuses by name, and the one it
        // would only ever be handed its own derivation of.
        assert!(
            !endpoint
                .proxy_argv()
                .iter()
                .any(|argument| argument == "--socket" || argument == "--pipe"),
            "the Windows proxy argv named an endpoint: {:?}",
            endpoint.proxy_argv()
        );
    }

    /// The attach argv a terminal is opened with: the binary, the mode, the id
    /// and the view it draws, and nothing else. Anything more would have to be
    /// `--pipe`, and the only name this side could put there is the one
    /// `attach` derives.
    #[test]
    fn the_attach_argv_is_the_binary_the_mode_the_session_id_and_the_view() {
        let backend = DaemonBackend::new();
        let argv = backend
            .session_argv(&proto::SessionId::new("session-1"), "view-1")
            .expect("a local Windows endpoint has no host paths to fail on");

        assert_eq!(
            argv,
            vec![
                backend.endpoint.bin_path.display().to_string(),
                "attach".to_owned(),
                "session-1".to_owned(),
                "--view-id".to_owned(),
                "view-1".to_owned(),
            ]
        );
        assert!(client_argv(&backend.endpoint.address).is_empty());
    }

    /// The workspace-level attach names no view — its terminal never claims
    /// the pty — and must not leave a dangling flag behind.
    #[test]
    fn an_attach_with_no_view_carries_no_view_flag() {
        let backend = DaemonBackend::new();
        let argv = backend
            .session_argv(&proto::SessionId::new("session-1"), "")
            .expect("a local Windows endpoint has no host paths to fail on");

        assert!(!argv.iter().any(|word| word == "--view-id"));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use ade_session_daemon::{Server, ServerConfig};
    use std::time::Instant;
    use tempfile::TempDir;

    /// A backend talking to a real daemon running in this process.
    ///
    /// The proxy transport is not in the loop here — it has its own tests in
    /// `ade_session_daemon`, and it needs a built binary this crate cannot name
    /// at compile time — so what these cover is the mapping this file owns.
    fn backend() -> (TempDir, ade_session_daemon::RunningServer, DaemonBackend) {
        let dir = TempDir::new().expect("temp dir");
        let config = ServerConfig::new(dir.path().join("daemon.sock"), dir.path().join("state"));
        let server = Server::spawn(config).expect("spawning the daemon");
        let backend = DaemonBackend::connected_to(server.socket_path(), "/opt/ade/ade-daemon");
        (dir, server, backend)
    }

    /// A spec naming a **real** workspace record: the daemon refuses a session
    /// in one it does not hold, so the row is created first — which is what the
    /// client's own row creation does. The name is the test's; the id is the
    /// daemon's.
    fn spec(name: &str, directory: &TempDir, backend: &DaemonBackend) -> SessionSpec {
        let workspace = backend
            .create_workspace(directory.path(), Some(name))
            .expect("creating the workspace record");
        SessionSpec::new(workspace.id, directory.path())
    }

    /// Poll `condition` until it holds. Statuses are derived by a sweep on the
    /// daemon's own cadence, so a test may not assume the first answer is the
    /// settled one.
    fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn create_list_exists_and_kill_over_the_seam() {
        let (dir, _server, backend) = backend();
        let spec = spec("ade-main-000001", &dir, &backend);

        // Creating hands back the *caller's* id, so the registry can go on
        // caching the name it derived.
        assert_eq!(backend.create(&spec).unwrap(), spec.id);
        assert_eq!(
            backend.list().unwrap(),
            vec![SessionInfo {
                id: spec.id.clone()
            }]
        );
        assert!(backend.exists(&spec.id).unwrap());
        assert!(
            !backend
                .exists(&SessionId::from("ade-other-000002"))
                .unwrap()
        );

        // Creating twice is refused rather than silently duplicated.
        assert!(backend.create(&spec).is_err());

        // Detaching is a no-op that leaves everything running.
        backend.detach(&spec.id).unwrap();
        assert!(backend.exists(&spec.id).unwrap());

        backend.kill(&spec.id).unwrap();
        assert!(!backend.exists(&spec.id).unwrap());
        assert!(backend.list().unwrap().is_empty());
        // And killing what is already gone is the state the caller asked for.
        backend.kill(&spec.id).unwrap();
    }

    #[test]
    fn attach_names_our_own_client_and_creates_what_is_missing() {
        let (dir, _server, backend) = backend();
        let spec = spec("ade-main-000003", &dir, &backend);

        // Attach-or-create: nothing exists yet, and the argv still works.
        let attached = backend.attach(&spec).unwrap();
        let argv = &attached.argv;
        assert!(backend.exists(&spec.id).unwrap());
        assert_eq!(argv[0], "/opt/ade/ade-daemon");
        assert_eq!(argv[1], "attach");
        assert_eq!(argv[3], "--socket");
        assert!(argv[4].ends_with("daemon.sock"), "{argv:?}");

        // The id in the argv is the daemon's own, which is the only name the
        // client can attach by — and it comes back beside the argv, because a
        // creating attach is the one caller that cannot know it in advance.
        let session = backend.live_session(&spec.id).unwrap().unwrap();
        assert_eq!(argv[2], session.id.to_string());
        assert_eq!(attached.session_id, session.id.to_string());

        // Attaching again reattaches rather than creating a second session.
        assert_eq!(backend.attach(&spec).unwrap(), attached);
        assert_eq!(backend.list().unwrap().len(), 1);
    }

    #[test]
    fn a_new_backend_reattaches_after_the_app_disconnects_without_duplicating() {
        let (dir, server, backend) = backend();
        let spec = spec("ade-reconnect-000004", &dir, &backend);
        let first = backend.attach(&spec).expect("the first app attaches");
        drop(backend);

        let reopened = DaemonBackend::connected_to(server.socket_path(), "/opt/ade/ade-daemon");
        let second = reopened
            .attach(&spec)
            .expect("the restarted app reattaches");

        assert_eq!(second, first);
        assert_eq!(
            reopened
                .daemon_sessions()
                .expect("listing after reconnect")
                .into_iter()
                .filter(|session| session.workspace_id == spec.id.as_str())
                .count(),
            1,
            "reattach must adopt the daemon-owned session, not create another"
        );
    }

    /// **A workspace holds N sessions.** The first one is `create`'s, every
    /// later one is a tab the user opened, and the seam above stays keyed by
    /// the workspace: one row in `list`, still `exists` while any of them lives,
    /// and a layout may name all of them at once.
    #[test]
    fn a_workspace_holds_more_than_one_session() {
        let (dir, _server, backend) = backend();
        let spec = spec("ade-plural-000020", &dir, &backend);
        backend.create(&spec).unwrap();
        let first = backend.live_session(&spec.id).unwrap().unwrap().id;

        let second = backend
            .create_session_in_workspace(spec.id.as_str(), dir.path())
            .unwrap();
        assert_ne!(second, first.to_string(), "the daemon mints a fresh id");

        // Both are the daemon's, both are in the workspace, and neither reaping
        // nor a one-live-session guard took the other.
        let held: Vec<String> = backend
            .daemon_sessions()
            .unwrap()
            .into_iter()
            .filter(|session| session.workspace_id == spec.id.as_str())
            .map(|session| session.id.to_string())
            .collect();
        assert_eq!(held.len(), 2, "{held:?}");
        assert!(held.contains(&first.to_string()) && held.contains(&second));

        // The seam above is keyed by the workspace, so N sessions are still one
        // row and one dot.
        assert!(backend.exists(&spec.id).unwrap());
        assert_eq!(
            backend.list().unwrap(),
            vec![SessionInfo {
                id: spec.id.clone()
            }]
        );

        // A document naming both is accepted: the daemon validates that every
        // terminal tab is a session it owns, and both are.
        let stored = backend.open_workspace(spec.id.as_str()).unwrap();
        let both = LayoutDoc::new(ade_session::LayoutNode::leaf(vec![
            ade_session::Tab::Terminal {
                session_id: first.clone(),
            },
            ade_session::Tab::Terminal {
                session_id: proto::SessionId::new(&second),
            },
        ]));
        backend
            .update_layout(spec.id.as_str(), &both, stored.rev + 1)
            .unwrap();
        assert_eq!(
            backend.open_workspace(spec.id.as_str()).unwrap().layout,
            both
        );

        // Closing one tab takes one session. The sibling keeps running and the
        // workspace record — layout and all — is untouched, which is what makes
        // this different from `kill`.
        backend.kill_session(&second).unwrap();
        assert!(backend.exists(&spec.id).unwrap());
        let left: Vec<String> = backend
            .daemon_sessions()
            .unwrap()
            .into_iter()
            .filter(|session| session.workspace_id == spec.id.as_str())
            .map(|session| session.id.to_string())
            .collect();
        assert_eq!(left, vec![first.to_string()]);
        assert!(backend.open_workspace(spec.id.as_str()).is_ok());

        let replacement = backend
            .create_session_in_workspace(spec.id.as_str(), dir.path())
            .unwrap();
        let sibling = SessionSpec::new(
            backend
                .create_workspace(dir.path(), Some("ade-plural-sibling-000021"))
                .unwrap()
                .id,
            dir.path(),
        );
        backend.create(&sibling).unwrap();

        backend.kill(&spec.id).unwrap();

        let remaining = backend.daemon_sessions().unwrap();
        assert!(
            !remaining
                .iter()
                .any(|session| session.workspace_id == spec.id.as_str()),
            "the workspace kill must take its primary and extra sessions: {remaining:?}"
        );
        assert!(
            remaining
                .iter()
                .any(|session| session.workspace_id == sibling.id.as_str()),
            "a sibling workspace must remain alive: {remaining:?}"
        );
        assert!(
            backend.open_workspace(spec.id.as_str()).is_ok(),
            "killing sessions must keep the workspace record"
        );
        assert!(
            backend.open_workspace(sibling.id.as_str()).is_ok(),
            "killing one workspace's sessions must keep its sibling"
        );
        assert!(!backend.exists(&spec.id).unwrap());

        assert_eq!(backend.create(&spec).unwrap(), spec.id);
        assert!(backend.exists(&spec.id).unwrap());
        assert!(
            backend
                .daemon_sessions()
                .unwrap()
                .iter()
                .all(|session| session.id.to_string() != replacement),
            "recreating the workspace must not revive an old extra session"
        );
    }

    /// The whole layout contract over the seam, against a real daemon: a
    /// workspace exists because a session named it, its arrangement comes back
    /// with a revision, a write must beat that revision, and a stale one is
    /// refused rather than quietly winning.
    #[test]
    fn a_layout_is_stored_against_a_revision_and_a_stale_write_loses() {
        let (dir, _server, backend) = backend();
        let spec = spec("ade-layout-000010", &dir, &backend);
        backend.create(&spec).unwrap();
        let session = backend.live_session(&spec.id).unwrap().unwrap();

        // The record was made empty and a session does not touch it: the
        // arrangement is the client's to write, from revision zero up.
        let stored = backend.open_workspace(spec.id.as_str()).unwrap();
        assert_eq!(
            stored.layout,
            LayoutDoc::empty(),
            "the daemon invented a layout for a session"
        );
        assert_eq!(stored.rev, 0);

        // A write has to be one past what is stored.
        let split = LayoutDoc::new(ade_session::LayoutNode::Split {
            dir: ade_session::SplitDir::Horizontal,
            ratio: 0.4,
            children: Box::new([
                ade_session::LayoutNode::leaf(vec![ade_session::Tab::Terminal {
                    session_id: session.id,
                }]),
                ade_session::LayoutNode::leaf(vec![ade_session::Tab::Editor {
                    path: "/repos/zed/main.rs".to_owned(),
                }]),
            ]),
        });
        backend
            .update_layout(spec.id.as_str(), &split, stored.rev + 1)
            .unwrap();

        let reread = backend.open_workspace(spec.id.as_str()).unwrap();
        assert_eq!(reread.layout, split);
        assert_eq!(reread.rev, stored.rev + 1);

        // The same revision again is a client writing from a view it has been
        // told is out of date. It loses, and learns that it lost.
        let error = backend
            .update_layout(spec.id.as_str(), &split, stored.rev + 1)
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("stale"),
            "a refused write must say why: {error:#}"
        );
        // And the refusal changed nothing.
        assert_eq!(backend.open_workspace(spec.id.as_str()).unwrap(), reread);

        // A workspace nobody ever made has no layout to render.
        assert!(backend.open_workspace("ade-nothing-000011").is_err());
    }

    /// An accepted layout reaches this client's *event* stream, because that is
    /// a different connection from the control one the daemon excluded — which
    /// is exactly why `layout::broadcast_action` exists.
    #[test]
    fn an_accepted_layout_comes_back_on_the_event_stream() {
        let (dir, _server, backend) = backend();
        let spec = spec("ade-layout-000012", &dir, &backend);
        backend.create(&spec).unwrap();
        let session = backend.live_session(&spec.id).unwrap().unwrap();

        let events = backend.subscribe_events().unwrap();
        // Drains the subscribe snapshot and proves the stream is live.
        smol::block_on(next_session(&events));

        let stored = backend.open_workspace(spec.id.as_str()).unwrap();
        let layout = LayoutDoc::new(ade_session::LayoutNode::leaf(vec![
            ade_session::Tab::Terminal {
                session_id: session.id,
            },
            ade_session::Tab::Editor {
                path: "/repos/zed/main.rs".to_owned(),
            },
        ]));
        backend
            .update_layout(spec.id.as_str(), &layout, stored.rev + 1)
            .unwrap();

        let event = smol::block_on(async {
            loop {
                if let DaemonEvent::Layout(event) = events.recv().await.unwrap() {
                    return event;
                }
            }
        });
        assert_eq!(event.workspace_id, spec.id.as_str());
        assert_eq!(event.rev, stored.rev + 1);
        assert_eq!(event.layout, layout);
    }

    /// The workspace-level kill, end to end against a real daemon: one frame
    /// takes every session in the workspace *and* the record holding their
    /// layout, and says so on the event stream so other clients can stop.
    #[test]
    fn killing_a_workspace_takes_its_sessions_and_its_record() {
        let (dir, _server, backend) = backend();
        let spec = spec("ade-layout-000014", &dir, &backend);
        backend.create(&spec).unwrap();
        assert!(backend.open_workspace(spec.id.as_str()).is_ok());

        let events = backend.subscribe_events().unwrap();
        // Drains the subscribe snapshot and proves the stream is live.
        smol::block_on(next_session(&events));

        backend.kill_workspace(spec.id.as_str()).unwrap();

        // The sessions are gone, and so is the workspace they were in — unlike
        // `kill`, which would leave the record behind holding dead tabs.
        assert!(backend.list().unwrap().is_empty());
        assert!(!backend.exists(&spec.id).unwrap());
        assert!(
            backend.open_workspace(spec.id.as_str()).is_err(),
            "a killed workspace has no layout left to open"
        );

        // And every other client is told, by workspace rather than by session.
        let removed = smol::block_on(async {
            loop {
                if let DaemonEvent::WorkspaceRemoved { workspace_id } = events.recv().await.unwrap()
                {
                    return workspace_id;
                }
            }
        });
        assert_eq!(removed, spec.id.as_str());

        // Killing what is already gone is refused rather than silently
        // succeeding: the caller above it falls back to the session kill, which
        // is what finishes the registry's side of the job.
        assert!(backend.kill_workspace(spec.id.as_str()).is_err());
    }

    /// What adoption reads: the workspaces the daemon holds, whether or not
    /// this client has ever heard of them — and *not* the ones it has killed.
    ///
    /// The second half is the guard on adoption itself. If a killed workspace
    /// stayed in this listing, every reconciliation would write its registry
    /// row straight back and the kill would undo itself; the daemon deletes the
    /// record, so it cannot.
    #[test]
    fn workspaces_are_listed_for_adoption_and_a_killed_one_is_not() {
        let (dir, _server, backend) = backend();
        assert!(backend.list_workspaces().unwrap().workspaces.is_empty());

        let spec = spec("ade-adopt-000020", &dir, &backend);
        backend.create(&spec).unwrap();

        let listed = backend.list_workspaces().unwrap();
        assert_eq!(listed.workspaces.len(), 1);
        // A healthy daemon's listing is authoritative, which is what licenses
        // reconcile to drop what it does not name.
        assert!(!listed.degraded);
        let workspace = &listed.workspaces[0];
        // Keyed by the seam's id, which is what an adopted row records as its
        // `terminal_session_id` and addresses the workspace by ever after.
        assert_eq!(workspace.id, spec.id.as_str());
        assert_eq!(workspace.project_root, dir.path().display().to_string());
        assert!(workspace.created_at > 0);

        // A rename is the daemon's to own, so adoption sees the new name.
        backend
            .rename_workspace(&workspace.id, "vector DB")
            .unwrap();
        assert_eq!(
            backend.list_workspaces().unwrap().workspaces[0].name,
            "vector DB"
        );

        // Killed: gone from the listing, so there is nothing to adopt back.
        backend.kill_workspace(spec.id.as_str()).unwrap();
        assert!(backend.list_workspaces().unwrap().workspaces.is_empty());
    }

    /// The layout's own attach: an argv for a session the daemon already has,
    /// which never creates one.
    #[test]
    fn attaching_by_session_id_names_the_client_without_creating_anything() {
        let (dir, _server, backend) = backend();
        let spec = spec("ade-layout-000013", &dir, &backend);
        backend.create(&spec).unwrap();
        let session = backend.live_session(&spec.id).unwrap().unwrap();

        let argv = backend
            .attach_session(session.id.as_str(), "view-1")
            .unwrap();
        assert_eq!(argv[0], "/opt/ade/ade-daemon");
        assert_eq!(argv[1], "attach");
        assert_eq!(argv[2], session.id.to_string());
        assert_eq!(argv[3], "--socket");

        // A session id nobody owns still produces an argv — the client is what
        // discovers that, and nothing is created here either way.
        assert_eq!(backend.list().unwrap().len(), 1);
        backend.attach_session("not-a-session", "view-1").unwrap();
        assert_eq!(backend.list().unwrap().len(), 1);

        // And killing by the daemon's own id takes that one session.
        backend.kill_session(session.id.as_str()).unwrap();
        assert!(backend.list().unwrap().is_empty());
    }

    #[test]
    fn a_session_whose_process_is_gone_is_not_a_live_session() {
        let (dir, server, backend) = backend();
        let spec = spec("ade-main-000004", &dir, &backend);
        backend.create(&spec).unwrap();
        let session = backend.live_session(&spec.id).unwrap().unwrap();

        // The shell exits: the daemon keeps the row, and this seam stops
        // reporting it, so the workspace reads as disconnected upstairs.
        smol::block_on(server.sessions().write(&session.id, b"exit\n")).unwrap();
        eventually("the session to be reported dead", || {
            !backend.exists(&spec.id).unwrap()
        });
        assert!(backend.list().unwrap().is_empty());

        // Recreating replaces the tombstone instead of piling up beside it.
        backend.create(&spec).unwrap();
        assert!(backend.exists(&spec.id).unwrap());
        assert_eq!(server.sessions().list().len(), 1);
    }

    #[test]
    fn status_is_pushed_and_named_by_the_seams_ids() {
        let (dir, _server, backend) = backend();
        assert_eq!(backend.status_delivery(), StatusDelivery::Push);

        // A session that already exists when the stream opens: subscribing
        // pushes a snapshot of it, and receiving that is also what proves the
        // subscription is live before anything below depends on it.
        let existing = spec("ade-existing-000005", &dir, &backend);
        backend.create(&existing).unwrap();
        let events = backend.subscribe_events().unwrap();
        let snapshot = smol::block_on(next_session(&events));
        assert_eq!(snapshot.id, existing.id);
        assert_eq!(
            snapshot.change,
            SessionChange::Status(WorkspaceStatus::Running)
        );

        // And one that appears afterwards, which the daemon announces itself.
        let fresh = spec("ade-fresh-000006", &dir, &backend);
        backend.create(&fresh).unwrap();
        let created = smol::block_on(next_for(&events, &fresh.id));
        assert_eq!(
            created.change,
            SessionChange::Created(WorkspaceStatus::Running)
        );
        assert_eq!(created.change.status(), Some(WorkspaceStatus::Running));

        // Killing takes the row out, and that is pushed too.
        backend.kill(&fresh.id).unwrap();
        let removed = smol::block_on(next_for(&events, &fresh.id));
        assert_eq!(removed.change, SessionChange::Removed);
        assert_eq!(removed.change.status(), None);
    }

    /// The next session event, ignoring layouts.
    async fn next_session(events: &Receiver<DaemonEvent>) -> StatusEvent {
        loop {
            if let DaemonEvent::Session(event) =
                events.recv().await.expect("the status stream stays open")
            {
                return event;
            }
        }
    }

    /// The next event about `id`, ignoring whatever else the daemon is saying.
    async fn next_for(events: &Receiver<DaemonEvent>, id: &SessionId) -> StatusEvent {
        loop {
            let event = next_session(events).await;
            if &event.id == id {
                return event;
            }
        }
    }

    #[test]
    fn daemon_statuses_map_onto_the_dots_the_sidebar_draws() {
        assert_eq!(
            workspace_status(SessionStatus::Working),
            WorkspaceStatus::Running
        );
        assert_eq!(
            workspace_status(SessionStatus::NeedsInput),
            WorkspaceStatus::Running
        );
        assert_eq!(
            workspace_status(SessionStatus::Idle),
            WorkspaceStatus::Running
        );
        assert_eq!(
            workspace_status(SessionStatus::Exited),
            WorkspaceStatus::Disconnected
        );
    }

    #[test]
    fn the_proxy_argv_is_the_one_the_deploy_module_defines() {
        let endpoint = Endpoint {
            bin_path: PathBuf::from("/opt/ade/ade-daemon"),
            address: Address::Named(LocalEndpoint::Socket(PathBuf::from("/run/ade/daemon.sock"))),
            state_dir: PathBuf::from("/var/lib/ade"),
            transport: Transport::Proxy,
        };
        assert_eq!(
            endpoint.proxy_argv(),
            vec![
                "/opt/ade/ade-daemon",
                "--stdio-proxy",
                "--socket",
                "/run/ade/daemon.sock",
                "--state-dir",
                "/var/lib/ade",
            ]
        );
    }

    /// A remote endpoint is entirely *local* paths: our own attach client, and
    /// a socket under `~/.ade/hosts` that the forward makes point at the host.
    /// Nothing is contacted to work this out, which is why it is testable
    /// without an ssh server.
    #[test]
    fn a_remote_endpoint_names_this_machines_client_and_its_own_socket() {
        let endpoint = Endpoint::remote("user@build-box", Vec::new()).expect("a remote endpoint");

        assert!(matches!(endpoint.transport, Transport::Forwarded(_)));
        assert_eq!(endpoint.bin_path, resolve_binary());
        let Address::Named(LocalEndpoint::Socket(socket_path)) = &endpoint.address else {
            panic!(
                "a unix client forwards to a socket, got {:?}",
                endpoint.address
            );
        };
        assert!(socket_path.is_absolute());
        assert_eq!(
            socket_path,
            &expand_home(HOST_SOCKET_DIR).join("user_build-box.sock")
        );
        // One socket per destination, so two hosts never collide and the same
        // host always finds its own.
        assert_ne!(
            endpoint.address,
            Endpoint::remote("build-box", Vec::new()).unwrap().address
        );
        assert_eq!(
            endpoint.address,
            Endpoint::remote("user@build-box", Vec::new())
                .unwrap()
                .address
        );
    }

    /// The Windows transport, forced on here because Linux is where the tests
    /// are: the attach argv names a loopback address instead of a path, and it
    /// is fixed before anything is contacted — the port is reserved when the
    /// endpoint is built, not when the forward comes up.
    #[test]
    fn a_tcp_endpoint_hands_out_a_stable_loopback_address_up_front() {
        let backend = DaemonBackend::remote_over_tcp_at(
            "build-box",
            Vec::new(),
            (
                "/home/x/.ade/bin/ade-daemon".to_owned(),
                "/home/x/.ade/daemon.sock".to_owned(),
                "/home/x/.ade/daemon".to_owned(),
            ),
        )
        .expect("a tcp-mode backend");

        let Address::Named(LocalEndpoint::Loopback(port)) = backend.endpoint.address else {
            panic!(
                "expected a loopback address, got {:?}",
                backend.endpoint.address
            );
        };
        assert_ne!(port, 0);
        assert_eq!(
            client_argv(&backend.endpoint.address),
            ["--tcp".to_owned(), format!("127.0.0.1:{port}")]
        );
        // The link forwards to the very address the client is told about;
        // nothing re-derives it later.
        let Transport::Forwarded(link) = &backend.endpoint.transport else {
            panic!("a remote endpoint is forwarded");
        };
        assert_eq!(Address::Named(link.local.clone()), backend.endpoint.address);
    }

    #[test]
    fn a_socket_endpoint_is_named_to_the_client_as_a_socket() {
        assert_eq!(
            client_argv(&Address::Named(LocalEndpoint::Socket(PathBuf::from(
                "/run/ade/daemon.sock"
            )))),
            ["--socket".to_owned(), "/run/ade/daemon.sock".to_owned()]
        );
    }

    #[test]
    fn a_destination_becomes_one_filename() {
        assert_eq!(sanitize_host("build-box"), "build-box");
        assert_eq!(sanitize_host("user@build-box"), "user_build-box");
        assert_eq!(sanitize_host("user@host:2222"), "user_host_2222");
        assert_eq!(sanitize_host("a/b"), "a_b");
    }

    /// The daemon's paths on the host are absolute, built from *its* `$HOME`
    /// rather than this machine's — the client has to name the same socket the
    /// daemon binds.
    #[test]
    fn remote_paths_are_absolute_against_the_hosts_home() {
        assert_eq!(
            expand_remote(DEFAULT_SOCKET_PATH, "/home/kingii"),
            "/home/kingii/.ade/daemon.sock"
        );
        assert_eq!(
            expand_remote(ade_session::deploy::DEFAULT_BIN_PATH, "/Users/k"),
            "/Users/k/.ade/bin/ade-daemon"
        );
        assert_eq!(
            expand_remote(DEFAULT_STATE_DIR, "/home/kingii"),
            "/home/kingii/.ade/daemon"
        );
        assert_eq!(
            expand_remote("/already/absolute", "/home/x"),
            "/already/absolute"
        );
    }

    /// A failure that never resolves must stop costing a process spawn a
    /// second: the schedule doubles and then sits at the cap, and a subscribe
    /// that worked is what puts it back to the short delay (which
    /// [`stream_status`] does, not this function).
    #[test]
    fn consecutive_failures_back_off_to_a_ceiling() {
        let mut delay = FIRST_RESUBSCRIBE_DELAY;
        let mut schedule = vec![delay];
        for _ in 0..7 {
            delay = next_resubscribe_delay(delay);
            schedule.push(delay);
        }
        assert_eq!(
            schedule,
            [1, 2, 4, 8, 16, 30, 30, 30].map(Duration::from_secs)
        );
        assert_eq!(
            next_resubscribe_delay(MAX_RESUBSCRIBE_DELAY),
            MAX_RESUBSCRIBE_DELAY
        );
    }

    /// A log line has to say which daemon it is about, or several backends'
    /// failures read as one.
    #[test]
    fn an_endpoint_names_itself_by_its_host_or_by_its_binary() {
        let local = Endpoint {
            bin_path: PathBuf::from("/opt/ade/ade-daemon"),
            address: Address::Named(LocalEndpoint::Socket(PathBuf::from("/run/ade/daemon.sock"))),
            state_dir: PathBuf::from("/var/lib/ade"),
            transport: Transport::Proxy,
        };
        assert_eq!(
            local.to_string(),
            "/opt/ade/ade-daemon at /run/ade/daemon.sock"
        );

        let remote = Endpoint::remote("user@build-box", Vec::new()).expect("a remote endpoint");
        assert!(
            remote.to_string().starts_with("user@build-box via "),
            "{remote}"
        );
    }

    #[test]
    fn the_binary_is_resolved_without_an_install_step() {
        // Whatever is found, it is named `ade-daemon` or is a path to one.
        let resolved = resolve_binary();
        let name = resolved.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with(DAEMON_BIN) || name.starts_with(DAEMON_BIN_IN_TARGET),
            "unexpected daemon binary {resolved:?}"
        );
        assert_eq!(
            expand_home("/absolute/path"),
            PathBuf::from("/absolute/path")
        );
        assert!(expand_home(DEFAULT_SOCKET_PATH).is_absolute());
    }
}
