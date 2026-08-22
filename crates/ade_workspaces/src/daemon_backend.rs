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
//! ids (uuids), while everything above this seam names a session by the id the
//! caller derived from the workspace ([`crate::tmux_session_name`], cached in
//! the registry). Rather than push the daemon's ids up through the registry —
//! which would rewrite what a workspace row means — this backend keeps the
//! caller's id as the seam id and passes it to the daemon as the session's
//! `workspace_id`. Resolving one to the other is a listing, and the seam's ids
//! stay exactly as stable as they were under tmux.
//!
//! **Attach is still an argv**, and deliberately: it names *our own* client,
//! `ade-daemon attach <id> --socket <path>` (or `--tcp <address>`), which Zed's
//! terminal spawns the way it used to spawn `tmux attach`. Closing the terminal
//! kills the client, which is a detach, and the session survives it. See the
//! seam's module docs for why the stream-shaped attach waits for the remote
//! transport.
//!
//! **A dead process is not a live session.** [`Self::list`] and [`Self::exists`]
//! report only sessions the daemon has not seen exit, so a workspace whose agent
//! died reads as disconnected upstairs and gets the sidebar's "gone" row and its
//! Recreate button — including the `(lost)` rows a restarted daemon reports,
//! whose ptys really are unrecoverable. Nothing is hidden: the exited row is
//! still in the daemon's own listing, and [`Self::kill`] takes it with the rest.

use crate::{
    Attached, BackendWorkspace, DaemonEvent, DaemonFreshnessObserver, Identified, LayoutEvent,
    SessionBackend, SessionChange, SessionId, SessionInfo, SessionSpec, StatusDelivery,
    StatusEvent, WorkspaceLayout, WorkspaceStatus,
};
use ade_session::{
    EnsureOutcome, LOOPBACK_ADDRESS, LayoutDoc, LocalEndpoint, PRE_CUT_DIAGNOSIS, ReadFrameError,
    deploy::{DEFAULT_SOCKET_PATH, DEFAULT_STATE_DIR, DaemonEndpoint},
    framing::bounded,
    is_handshake_eof,
    proto::{self, Frame, Hello, SessionStatus},
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
        atomic::{AtomicU8, AtomicU64, Ordering},
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

/// A control connection paired with its own handshake identity.
struct Control {
    connection: DaemonConnection,
    instance_id: Option<String>,
}

/// `None` is permissive for rows created before daemon identities existed.
fn identity_admits(expected: Option<&str>, actual: Option<&str>) -> bool {
    expected.is_none_or(|expected| actual == Some(expected))
}

/// Sessions kept alive by the ADE session daemon on this machine.
pub struct DaemonBackend {
    endpoint: Endpoint,
    /// The control connection, opened on first use. `None` means "not
    /// connected", which is also what a broken connection is reset to — the
    /// next call reconnects, and the proxy restarts the daemon if it really is
    /// gone. Sessions lost that way come back as exited `(lost)` rows rather
    /// than being quietly recreated.
    connection: Mutex<Option<Control>>,
    next_request_id: AtomicU64,
    /// [`ANSWER_TIMEOUT`], except in tests that would otherwise have to sit
    /// through it to reach the failure they are about.
    answer_timeout: Duration,
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
        }
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
            address: local.clone(),
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
            identity: Arc::new(Mutex::new(None)),
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
            address: LocalEndpoint::Socket(socket_path.into()),
            state_dir: PathBuf::new(),
            transport: Transport::Direct,
            identity: Arc::new(Mutex::new(None)),
        })
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
        expected_daemon_id: Option<&str>,
        request_id: u64,
        request: Frame,
        want: impl Fn(&Frame) -> Option<T>,
    ) -> Result<T> {
        self.request_seen(expected_daemon_id, request_id, request, want)
            .map(|(value, _)| value)
    }

    /// [`Self::request`] with the identity of the connection that answered.
    fn request_seen<T>(
        &self,
        expected_daemon_id: Option<&str>,
        request_id: u64,
        request: Frame,
        want: impl Fn(&Frame) -> Option<T>,
    ) -> Result<(T, Option<String>)> {
        let mut slot = self.connection.lock().unwrap_or_else(|e| e.into_inner());
        let outcome = smol::block_on(async {
            // Reconnect once before sending; a cached route may have moved.
            if slot.as_ref().is_some_and(|control| {
                !identity_admits(expected_daemon_id, control.instance_id.as_deref())
            }) {
                *slot = None;
                self.endpoint.on_connection_lost();
            }
            let control = match slot.as_mut() {
                Some(control) => control,
                None => slot.insert(self.endpoint.connect().await?),
            };
            // Authorization and send share the connection lock.
            if !identity_admits(expected_daemon_id, control.instance_id.as_deref()) {
                return anyhow::Ok(Err(format!(
                    "this operation belongs to daemon {}, and {} answers as {}",
                    bounded(expected_daemon_id.unwrap_or_default()),
                    self.endpoint,
                    bounded(
                        control
                            .instance_id
                            .as_deref()
                            .unwrap_or("an unnamed daemon")
                    ),
                )));
            }
            let seen = control.instance_id.clone();
            let connection = &mut control.connection;
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
                    return anyhow::Ok(Ok((value, seen.clone())));
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
                        // different answers and a log nobody can grep is how
                        // that distinction gets lost. Both are `bounded`: this
                        // string becomes the `bail!` below and then the ADE
                        // sidebar's failure text, and a `message` is a frame
                        // field the peer sizes up to `MAX_FRAME_BYTES`.
                        Some(id) if id == request_id => {
                            return anyhow::Ok(Err(format!(
                                "{}: {}",
                                bounded(&code),
                                bounded(&message)
                            )));
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
            Ok(Err(message)) => bail!(message),
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
        connection: &mut Option<Control>,
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

    /// Ask to be told when [`Self::daemon_stale`] changes its answer.
    ///
    /// A local daemon registers nothing: its answer is the constant `false`, so
    /// an observer here would be one that never fires.
    pub fn observe_daemon_freshness(&self, observer: DaemonFreshnessObserver) {
        if let Transport::Forwarded(link) = &self.endpoint.transport {
            link.observe_daemon_freshness(observer);
        }
    }

    /// The identity from the last successful handshake on either connection.
    ///
    /// Observational only; operations use [`Control::instance_id`].
    pub fn instance_id(&self) -> Option<String> {
        self.endpoint
            .identity
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Every session the daemon holds, exited ones included.
    fn daemon_sessions(&self, expected_daemon_id: Option<&str>) -> Result<Vec<proto::SessionInfo>> {
        Ok(self.daemon_sessions_seen(expected_daemon_id)?.0)
    }

    /// [`Self::daemon_sessions`] with the identity that answered it.
    fn daemon_sessions_seen(
        &self,
        expected_daemon_id: Option<&str>,
    ) -> Result<(Vec<proto::SessionInfo>, Option<String>)> {
        let id = self.request_id();
        self.request_seen(
            expected_daemon_id,
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

    /// Every workspace record the daemon holds, with the identity that answered.
    fn daemon_workspaces_seen(&self) -> Result<(Vec<BackendWorkspace>, Option<String>)> {
        let request_id = self.request_id();
        let (workspaces, seen) = self
            .request_seen(
                // Discovery has no persisted owner yet.
                None,
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
        Ok((
            workspaces
                .into_iter()
                .map(|workspace| BackendWorkspace {
                    id: workspace.id,
                    name: workspace.name,
                    project_root: workspace.project_root,
                    created_at: workspace.created_at,
                })
                .collect(),
            seen,
        ))
    }

    /// The live session carrying `id` as its workspace id, newest first.
    ///
    /// Newest because a workspace can legitimately have a tombstone and a
    /// replacement (recreate after a crash); the one still running is the one
    /// the caller means.
    fn live_session(
        &self,
        id: &SessionId,
        expected_daemon_id: Option<&str>,
    ) -> Result<Option<proto::SessionInfo>> {
        Ok(newest_live(&self.daemon_sessions(expected_daemon_id)?, id))
    }

    /// Create the daemon session for `spec`, reaping any tombstone it replaces.
    fn create_session(
        &self,
        spec: &SessionSpec,
        expected_daemon_id: Option<&str>,
    ) -> Result<(proto::SessionInfo, Option<String>)> {
        let existing = self.daemon_sessions(expected_daemon_id)?;
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
            self.kill_daemon_session(&tombstone.id, expected_daemon_id)?;
        }

        let request_id = self.request_id();
        self.request_seen(
            expected_daemon_id,
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
    fn session_argv(&self, id: &proto::SessionId, expected_daemon_id: Option<&str>) -> Vec<String> {
        let mut argv = vec![
            self.endpoint.bin_path.display().to_string(),
            "attach".to_owned(),
            id.to_string(),
        ];
        argv.extend(client_argv(&self.endpoint.address));
        // The attach client owns a separate connection.
        if let Some(expected) = expected_daemon_id {
            argv.push("--expected-daemon-id".to_owned());
            argv.push(expected.to_owned());
        }
        argv
    }

    fn kill_daemon_session(
        &self,
        id: &proto::SessionId,
        expected_daemon_id: Option<&str>,
    ) -> Result<()> {
        let request_id = self.request_id();
        self.request(
            expected_daemon_id,
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
    fn create(&self, spec: &SessionSpec, expected_daemon_id: Option<&str>) -> Result<SessionId> {
        self.create_session(spec, expected_daemon_id)?;
        // The seam's id, not the daemon's: the registry caches this, and it has
        // to be the same string `attach` and `exists` are called with later.
        Ok(spec.id.clone())
    }

    fn create_identified(
        &self,
        spec: &SessionSpec,
        expected_daemon_id: Option<&str>,
    ) -> Result<(SessionId, Option<String>)> {
        let (_, daemon_id) = self.create_session(spec, expected_daemon_id)?;
        Ok((spec.id.clone(), daemon_id))
    }

    /// A sibling session in a workspace the daemon already holds.
    ///
    /// Deliberately without [`Self::create_session`]'s bookkeeping: no
    /// one-live-session guard, because a second live session is the point, and
    /// no tombstone reaping, because the rows it would take belong to siblings
    /// the caller never mentioned. The daemon's `ensure_workspace` leaves an
    /// existing workspace record — and its layout — untouched, so the new
    /// session enters the document only when this window captures it.
    fn create_session_in_workspace(
        &self,
        workspace_id: &str,
        cwd: &Path,
        expected_daemon_id: Option<&str>,
    ) -> Result<String> {
        let request_id = self.request_id();
        let session = self
            .request(
                expected_daemon_id,
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
        Ok(self.list_identified()?.items)
    }

    fn list_identified(&self) -> Result<Identified<SessionInfo>> {
        // Global discovery, so no fence — and the identity of the connection
        // that answered comes back with the rows, because reading it afterwards
        // could name the status connection's daemon instead.
        let (sessions, daemon_id) = self.daemon_sessions_seen(None)?;
        let mut seen = HashSet::new();
        let items = sessions
            .into_iter()
            .filter(|session| session.status != SessionStatus::Exited)
            .filter(|session| !session.workspace_id.is_empty())
            .filter(|session| seen.insert(session.workspace_id.clone()))
            .map(|session| SessionInfo {
                id: SessionId::from(session.workspace_id),
            })
            .collect();
        Ok(Identified { daemon_id, items })
    }

    fn exists(&self, id: &SessionId, expected_daemon_id: Option<&str>) -> Result<bool> {
        Ok(self.live_session(id, expected_daemon_id)?.is_some())
    }

    /// Every workspace record the daemon holds, sessions or no sessions.
    ///
    /// Not derived from [`Self::list`]: the daemon's workspace records outlive
    /// the sessions in them — a restarted daemon keeps the record and reports
    /// its ptys as lost — so a workspace with nothing running is exactly the
    /// case a listing of *sessions* cannot see, and exactly the one an empty
    /// registry most needs told about.
    fn list_workspaces(&self) -> Result<Vec<BackendWorkspace>> {
        Ok(self.list_workspaces_identified()?.items)
    }

    fn list_workspaces_identified(&self) -> Result<Identified<BackendWorkspace>> {
        let (items, daemon_id) = self.daemon_workspaces_seen()?;
        Ok(Identified { daemon_id, items })
    }

    fn attach(&self, spec: &SessionSpec, expected_daemon_id: Option<&str>) -> Result<Attached> {
        // Attach-or-create, like tmux's `new-session -A`: the first open of a
        // workspace that has no session is one step, and reopening a pane on a
        // live one reattaches to everything still running in it.
        // Both the listing and possible create are fenced.
        let session = match self.live_session(&spec.id, expected_daemon_id)? {
            Some(session) => session,
            None => self.create_session(spec, expected_daemon_id)?.0,
        };
        Ok(Attached {
            argv: self.session_argv(&session.id, expected_daemon_id),
            session_id: session.id.to_string(),
        })
    }

    fn attach_session(
        &self,
        session_id: &str,
        expected_daemon_id: Option<&str>,
    ) -> Result<Vec<String>> {
        Ok(self.session_argv(&proto::SessionId::new(session_id), expected_daemon_id))
    }

    fn open_workspace(
        &self,
        workspace_id: &str,
        expected_daemon_id: Option<&str>,
    ) -> Result<WorkspaceLayout> {
        let request_id = self.request_id();
        let workspace = self
            .request(
                expected_daemon_id,
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

    fn update_layout(
        &self,
        workspace_id: &str,
        layout: &LayoutDoc,
        rev: u64,
        expected_daemon_id: Option<&str>,
    ) -> Result<()> {
        let request_id = self.request_id();
        self.request(
            expected_daemon_id,
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

    fn kill_session(&self, session_id: &str, expected_daemon_id: Option<&str>) -> Result<()> {
        self.kill_daemon_session(&proto::SessionId::new(session_id), expected_daemon_id)
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
    fn rename_workspace(
        &self,
        workspace_id: &str,
        name: &str,
        expected_daemon_id: Option<&str>,
    ) -> Result<()> {
        let request_id = self.request_id();
        self.request(
            expected_daemon_id,
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
    fn kill_workspace(&self, workspace_id: &str, expected_daemon_id: Option<&str>) -> Result<()> {
        let request_id = self.request_id();
        self.request(
            expected_daemon_id,
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

    /// See [`DaemonBackend::observe_daemon_freshness`].
    fn observe_daemon_freshness(&self, observer: DaemonFreshnessObserver) {
        DaemonBackend::observe_daemon_freshness(self, observer);
    }

    fn instance_id(&self) -> Option<String> {
        DaemonBackend::instance_id(self)
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

    fn kill(&self, id: &SessionId, expected_daemon_id: Option<&str>) -> Result<()> {
        // Include exited rows; the fenced listing cannot aim kills at a peer.
        for session in self
            .daemon_sessions(expected_daemon_id)?
            .iter()
            .filter(|session| session.workspace_id == id.as_str())
        {
            self.kill_daemon_session(&session.id, expected_daemon_id)?;
        }
        Ok(())
    }

    fn reset_workspace_sessions(
        &self,
        id: &SessionId,
        directory: &Path,
        expected_daemon_id: Option<&str>,
    ) -> Result<()> {
        self.kill(id, expected_daemon_id)?;
        if let Transport::Forwarded(link) = &self.endpoint.transport {
            link.recover_stale_daemon_processes(directory, expected_daemon_id)?;
        }
        Ok(())
    }

    fn status_delivery(&self) -> StatusDelivery {
        StatusDelivery::Push
    }

    fn subscribe_events(&self) -> Result<Receiver<DaemonEvent>> {
        let (sender, receiver) = smol::channel::unbounded();
        let endpoint = self.endpoint.clone();
        // A plain thread, not a task: it owns a connection of its own and
        // spends its life blocked on it, which is the one thing an executor
        // thread must not do.
        std::thread::Builder::new()
            .name("ade-daemon-status".to_owned())
            .spawn(move || stream_status(endpoint, sender))
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
fn stream_status(endpoint: Endpoint, sender: Sender<DaemonEvent>) {
    let mut delay = FIRST_RESUBSCRIBE_DELAY;
    let mut last_failure: Option<String> = None;
    let mut known_workspaces = HashMap::new();
    let mut has_workspace_snapshot = false;

    while !sender.is_closed() {
        let mut subscribed = false;
        let outcome = smol::block_on(stream_status_once(
            &endpoint,
            &sender,
            &mut subscribed,
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
            last_failure = Some(message);
            // TODO(#135 follow-up): this warning is the only place a
            // *mid-stream* failure is visible. The sidebar's
            // `status_stream_error` line
            // (`workspace_sidebar::WorkspaceSidebar::new`) is fed once, from
            // the single `Result` that `WorkspaceLifecycleService::
            // subscribe_status` returns, so it can only report a stream that
            // never opened. Carrying a later failure to it needs a seam that
            // does not exist: either a `SessionChange`/`StatusEvent` shape that
            // can say "the stream is down", or a second channel out of
            // `SessionBackend::subscribe_status` — both public-API reshapes,
            // and both would drag `lifecycle.rs` along with them.

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

/// One subscription, from connect to disconnect.
///
/// `subscribed` is set the moment the subscription is known to be live — which
/// is what tells [`stream_status`] that this attempt was not a permanent
/// failure, since every healthy run ends the same way a broken one does: with
/// an error off the connection.
async fn stream_status_once(
    endpoint: &Endpoint,
    sender: &Sender<DaemonEvent>,
    subscribed: &mut bool,
    known_workspaces: &mut HashMap<String, KnownWorkspace>,
    has_workspace_snapshot: &mut bool,
) -> Result<()> {
    let mut connection = endpoint.connect().await?.connection;

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
    for event in workspace_snapshot_events(workspaces.unwrap_or_default(), known_workspaces) {
        if sender.send(event).await.is_err() {
            return Ok(());
        }
    }
    *has_workspace_snapshot = true;

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

fn workspace_snapshot_events(
    workspaces: Vec<proto::WorkspaceInfo>,
    known: &mut HashMap<String, KnownWorkspace>,
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
    for workspace_id in known.keys() {
        if !current.contains_key(workspace_id) {
            events.push(DaemonEvent::WorkspaceRemoved {
                workspace_id: workspace_id.clone(),
            });
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
    *known = current;
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
        DaemonEvent::Session(_) => true,
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
    address: LocalEndpoint,
    state_dir: PathBuf,
    transport: Transport,
    /// Shared by the independently opened control and status connections.
    identity: Arc<Mutex<Option<String>>>,
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
    /// This machine's daemon: our own binary, at the standard socket.
    fn local() -> Self {
        Self {
            bin_path: resolve_binary(),
            address: LocalEndpoint::Socket(expand_home(DEFAULT_SOCKET_PATH)),
            state_dir: expand_home(DEFAULT_STATE_DIR),
            transport: Transport::Proxy,
            identity: Arc::new(Mutex::new(None)),
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
            address: address.clone(),
            // Unused: the daemon's state lives on the host, and the proxy argv
            // this would feed is never built for a forwarded endpoint.
            state_dir: PathBuf::new(),
            transport: Transport::Forwarded(Arc::new(HostLink::new(host, address))),
            identity: Arc::new(Mutex::new(None)),
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
    async fn connect(&self) -> Result<Control> {
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
        let (connection, ack) = handshaken(|| self.open()).await?;
        // Shared for observation only; requests use `Control::instance_id`.
        *self.identity.lock().unwrap_or_else(|e| e.into_inner()) = ack.instance_id.clone();
        Ok(Control {
            connection,
            instance_id: ack.instance_id,
        })
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
            LocalEndpoint::Socket(path) => DaemonConnection::Socket(ade_session::Connection::new(
                smol::net::unix::UnixStream::connect(path)
                    .await
                    .with_context(|| format!("connecting to {}", path.display()))?,
            )),
            #[cfg(not(unix))]
            LocalEndpoint::Socket(path) => bail!(
                "this platform cannot connect to the Unix socket {}",
                path.display()
            ),
            LocalEndpoint::Loopback(port) => DaemonConnection::Tcp(ade_session::Connection::new(
                smol::net::TcpStream::connect((LOOPBACK_ADDRESS, *port))
                    .await
                    .with_context(|| format!("connecting to {LOOPBACK_ADDRESS}:{port}"))?,
            )),
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
        DaemonEndpoint::preinstalled(
            self.bin_path.display().to_string(),
            // Only ever built for [`Transport::Proxy`], which fronts a *local*
            // daemon, and a daemon binds a socket. A loopback address here
            // would be a construction bug; it is passed through so the child
            // says so rather than this panicking.
            self.address.to_string(),
            self.state_dir.display().to_string(),
        )
        .proxy_argv()
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
async fn handshaken<C, F>(open: C) -> Result<(DaemonConnection, proto::HelloAck)>
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
pub(crate) fn is_incompatible_daemon(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.to_string().contains(PRE_CUT_DIAGNOSIS))
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
fn stale_daemon_recovery_script(
    directory: &Path,
    daemon_identity: Option<(&str, &str)>,
) -> Result<String> {
    use ade_session::deploy::shell_quote;

    let directory = directory.to_str().with_context(|| {
        format!(
            "the remote worktree path is not UTF-8: {}",
            directory.display()
        )
    })?;
    let identity_guard = daemon_identity
        .map(|(state_dir, expected_daemon_id)| {
            format!(
                concat!(
                    "instance_file={instance_file}\n",
                    "expected_daemon_id={expected_daemon_id}\n",
                    "if ! actual_daemon_id=$(cat \"$instance_file\" 2>/dev/null) || [ -z \"$actual_daemon_id\" ]; then\n",
                    "  echo \"cannot verify daemon identity at $instance_file\" >&2; exit 3\n",
                    "fi\n",
                    "if [ \"$actual_daemon_id\" != \"$expected_daemon_id\" ]; then\n",
                    "  echo \"daemon identity mismatch at $instance_file\" >&2; exit 3\n",
                    "fi\n",
                ),
                instance_file = shell_quote(&format!("{state_dir}/instance.id")),
                expected_daemon_id = shell_quote(expected_daemon_id),
            )
        })
        .unwrap_or_default();
    Ok(format!(
        concat!(
            "root={root}\n",
            "if ! root=$(cd \"$root\" 2>/dev/null && pwd -P); then\n",
            "  echo \"cannot enter the worktree at $root\" >&2; exit 2\n",
            "fi\n",
            "if [ \"$root\" = / ]; then echo \"refusing to recover every terminal on the host\" >&2; exit 2; fi\n",
            "{identity_guard}",
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
        identity_guard = identity_guard,
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
fn client_argv(address: &LocalEndpoint) -> [String; 2] {
    match address {
        LocalEndpoint::Socket(path) => ["--socket".to_owned(), path.display().to_string()],
        LocalEndpoint::Loopback(port) => ["--tcp".to_owned(), format!("{LOOPBACK_ADDRESS}:{port}")],
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
/// socket the daemon binds, and `~` is only a shell's idea.
#[derive(Clone, Debug)]
struct RemotePaths {
    bin: String,
    socket: String,
    state_dir: String,
}

/// What the `--ensure` line says beyond "a daemon is listening".
///
/// The line is `ade-daemon <version>` followed by optional `key=value` tokens
/// a newer daemon appends: `hash=<hex sha256 of its binary>` and
/// `upgrade_ready=<bool>`. Absent tokens decode to the conservative reading —
/// no hash means a legacy daemon nothing may touch, and readiness defaults to
/// `false` for the same reason.
#[derive(Debug, PartialEq, Eq)]
struct EnsureReport {
    /// The daemon's binary identity, and the *only* thing that can say whether
    /// the host is behind: the version on the same line is `ade_session`'s
    /// crate version, pinned like every crate in this workspace, so comparing
    /// versions would be comparing a constant with itself.
    hash: Option<String>,
    upgrade_ready: bool,
}

impl EnsureReport {
    fn parse(line: &str) -> Self {
        let mut report = Self {
            hash: None,
            upgrade_ready: false,
        };
        for token in line.split_whitespace() {
            if let Some(value) = token.strip_prefix("hash=") {
                if !value.is_empty() {
                    report.hash = Some(value.to_owned());
                }
            } else if let Some(value) = token.strip_prefix("upgrade_ready=") {
                report.upgrade_ready = value == "true";
            }
        }
        report
    }
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
    /// skipped. See [`DaemonBackend::remote_at`].
    // Its only caller is unix-gated, so the plain `test` gate warned on
    // Windows.
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
        let report = EnsureReport::parse(&line);
        let Some(remote_hash) = report.hash else {
            bail!(
                "the daemon on {} predates binary identity and cannot be upgraded in place; \
                 stop it by hand",
                self.host.destination
            );
        };
        let outcome = self.upgrade_to_local_binary(&paths, &remote_hash, true)?;
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
        let Some(remote_hash) = report.hash else {
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
        match self.upgrade_to_local_binary(paths, &remote_hash, false) {
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
    fn upgrade_to_local_binary(
        &self,
        paths: &RemotePaths,
        remote_hash: &str,
        force: bool,
    ) -> Result<DaemonUpgradeOutcome> {
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
        let config = ade_session::DeployConfig::new(binary, ade_session::daemon_version())
            .with_bin_path(paths.bin.clone())
            .with_socket_path(paths.socket.clone())
            .with_state_dir(paths.state_dir.clone());
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
                Err(error) if force && is_incompatible_daemon(&error) => {
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
            loop {
                match connection
                    .recv_decodable()
                    .await
                    .context("waiting for the shutdown answer")?
                {
                    Frame::ShutdownAck { .. } => return Ok(()),
                    // Correlated, like every other request: a declined
                    // shutdown echoes this rid, while an error carrying none is
                    // the daemon reporting something else entirely and must
                    // not be read as a refusal to exit.
                    Frame::Error {
                        code,
                        message,
                        request_id: Some(1),
                        ..
                    } => bail!("{}: {}", bounded(&code), bounded(&message)),
                    other => log::debug!("ignoring {other:?} while waiting for ShutdownAck"),
                }
            }
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

    fn recover_stale_daemon_processes(
        &self,
        directory: &Path,
        expected_daemon_id: Option<&str>,
    ) -> Result<()> {
        use ade_session::deploy::HostExec as _;

        let state_dir = expected_daemon_id
            .map(|_| {
                self.state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .paths
                    .as_ref()
                    .context("the remote daemon paths were not resolved")
                    .map(|paths| paths.state_dir.clone())
            })
            .transpose()?;
        let output = self
            .host
            .run(&[
                "sh".to_owned(),
                "-c".to_owned(),
                stale_daemon_recovery_script(
                    directory,
                    state_dir.as_deref().zip(expected_daemon_id),
                )?,
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

        let config = ade_session::DeployConfig::new(binary, ade_session::daemon_version())
            .with_bin_path(paths.bin.clone())
            .with_socket_path(paths.socket.clone())
            .with_state_dir(paths.state_dir.clone());
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

    /// The host's `$HOME`, expanded into the three paths the daemon uses.
    ///
    /// `ssh host command` is not a login shell, so this asks a plain `sh -c`
    /// for `$HOME` the same way [`ade_session::deploy`] does — proven against a
    /// real connection by that crate's loopback tests.
    fn remote_paths(&self) -> Result<RemotePaths> {
        use ade_session::deploy::HostExec as _;

        let output = self
            .host
            .run(&[
                "sh".to_owned(),
                "-c".to_owned(),
                "printf %s \"$HOME\"".to_owned(),
            ])
            .with_context(|| format!("asking {} for $HOME", self.host.destination))?;
        if !output.success() || output.stdout.trim().is_empty() {
            bail!(
                "could not read $HOME on {}: {}",
                self.host.destination,
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
    async fn handshake(&mut self) -> Result<proto::HelloAck> {
        // Pinned at generation 2: this client still implements gen-2 semantics
        // (auto-created workspaces, the combined create), while the crate can
        // already speak 3. Announcing 3 would have the daemon hold it to the
        // gen-3 meanings. The registry client that speaks 3 is the next commit,
        // and it removes this pin.
        let hello = Hello {
            max_generation: proto::MIN_GENERATION,
            ..Hello::current()
        };
        let ack = match self {
            Self::Proxied(connection) => connection.handshake(hello.clone()).await,
            #[cfg(unix)]
            Self::Socket(connection) => connection.handshake(hello.clone()).await,
            Self::Tcp(connection) => connection.handshake(hello).await,
        }
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
        /// An answer whose handshake carries the specified identity.
        WithIdentity(Option<String>, Vec<Frame>),
        /// Handshake under this identity, then report every frame it receives
        /// to the test and answer a `ListSessions` with nothing.
        ///
        /// What proves a frame was *not* sent: a later request that does arrive
        /// is the first thing the channel yields.
        Watched(Option<String>, std::sync::mpsc::Sender<Frame>),
    }

    fn ack() -> proto::HelloAck {
        proto::HelloAck {
            daemon_version: "0.0.0-scripted".to_owned(),
            protocol_version: proto::MAX_GENERATION,
            host_os: "test".to_owned(),
            min_generation: proto::MIN_GENERATION,
            max_generation: proto::MAX_GENERATION,
            // What a real daemon selects for this client, whose handshake pins
            // its offer at generation 2.
            generation: proto::MIN_GENERATION,
            capabilities: Vec::new(),
            degraded: false,
            binary_hash: None,
            upgrade_ready: None,
            instance_id: None,
            request_id: None,
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
                        let _hello = daemon.recv().await;
                        if let Script::SubscriptionSnapshot {
                            sessions,
                            workspaces,
                            pending,
                        } = &script
                        {
                            daemon
                                .send(&Frame::HelloAck(ack()))
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
                        if let Script::Watched(instance_id, seen) = script {
                            let mut hello_ack = ack();
                            hello_ack.instance_id = instance_id;
                            daemon
                                .send(&Frame::HelloAck(hello_ack))
                                .await
                                .expect("sending the ack");
                            while let Ok(frame) = daemon.recv().await {
                                if let Frame::ListSessions { request_id } = &frame {
                                    daemon
                                        .send(&Frame::SessionList {
                                            sessions: Vec::new(),
                                            request_id: *request_id,
                                        })
                                        .await
                                        .expect("answering the listing");
                                }
                                if seen.send(frame).is_err() {
                                    break;
                                }
                            }
                            continue;
                        }
                        let mut hello_ack = ack();
                        let (quiet_for, frames, repeated) = match script {
                            Script::EofDuringHandshake => continue,
                            Script::AnswerWith(frames) => (Duration::ZERO, frames, None),
                            Script::AnswerAfter(quiet_for, frames) => (quiet_for, frames, None),
                            Script::KeepTalking(frame) => (Duration::ZERO, Vec::new(), Some(frame)),
                            Script::WithIdentity(instance_id, frames) => {
                                hello_ack.instance_id = instance_id;
                                (Duration::ZERO, frames, None)
                            }
                            Script::LegacySessionList(_) => unreachable!(),
                            Script::SubscriptionSnapshot { .. } => unreachable!(),
                            Script::Watched(..) => unreachable!(),
                        };
                        daemon
                            .send(&Frame::HelloAck(hello_ack))
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
            address: LocalEndpoint::Loopback(port),
            state_dir: PathBuf::new(),
            transport: Transport::Direct,
            identity: Arc::new(Mutex::new(None)),
        });
        backend.answer_timeout = SCRIPTED_ANSWER_TIMEOUT;
        backend
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

        let listed = bounded("the listing", move || backend.daemon_sessions(None));
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
                    .daemon_sessions(None)
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
                    .daemon_sessions(None)
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

        let listed = bounded("the slow listing", move || backend.daemon_sessions(None));
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
                .daemon_sessions(None)
                .expect_err("a reply that cannot match is not an answer to wait on");
            backend
        });
        let listed = bounded("the listing after it", move || {
            backend.daemon_sessions(None)
        });
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
            .daemon_sessions(None)
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
                .daemon_sessions(None)
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

        let listed = bounded("the retried handshake", move || {
            backend.daemon_sessions(None)
        });
        assert!(listed.expect("the retry gets a healthy daemon").is_empty());
    }

    fn identity_listing(instance_id: Option<&str>, request_id: u64) -> Script {
        Script::WithIdentity(
            instance_id.map(str::to_owned),
            vec![Frame::SessionList {
                sessions: Vec::new(),
                request_id: Some(request_id),
            }],
        )
    }

    fn listed(backend: DaemonBackend) -> DaemonBackend {
        bounded("the listing", move || {
            backend.daemon_sessions(None).expect("listing");
            backend
        })
    }

    fn force_reconnect(backend: &DaemonBackend) {
        *backend
            .connection
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
    }

    #[test]
    fn instance_id_is_none_before_any_handshake() {
        let backend = scripted_daemon(Vec::new());
        assert_eq!(backend.instance_id(), None);
    }

    #[test]
    fn a_successful_handshake_remembers_the_daemons_identity() {
        let backend = listed(scripted_daemon(vec![identity_listing(
            Some("host-instance-1"),
            1,
        )]));
        assert_eq!(backend.instance_id(), Some("host-instance-1".to_owned()));
    }

    #[test]
    fn a_nameless_successful_handshake_clears_a_stale_identity() {
        let backend = listed(scripted_daemon(vec![
            identity_listing(Some("host-instance-1"), 1),
            identity_listing(None, 2),
        ]));
        assert_eq!(backend.instance_id(), Some("host-instance-1".to_owned()));

        force_reconnect(&backend);
        let backend = listed(backend);
        assert_eq!(backend.instance_id(), None);
    }

    #[test]
    fn a_failed_handshake_retains_the_last_successful_identity() {
        let backend = listed(scripted_daemon(vec![
            identity_listing(Some("host-instance-1"), 1),
            Script::EofDuringHandshake,
            Script::EofDuringHandshake,
        ]));
        assert_eq!(backend.instance_id(), Some("host-instance-1".to_owned()));

        force_reconnect(&backend);
        let backend = bounded("the failed reconnect", move || {
            backend
                .daemon_sessions(None)
                .expect_err("both attempts ended in EOF");
            backend
        });
        assert_eq!(backend.instance_id(), Some("host-instance-1".to_owned()));
    }

    /// **A refused operation costs the wrong daemon nothing.** The frame is
    /// what kills a workspace, so the fence has to stop it before the write —
    /// an error read off the wire afterwards would already be too late.
    #[test]
    fn a_destructive_request_for_another_daemon_is_never_sent() {
        let (seen, received) = std::sync::mpsc::channel();
        let backend = scripted_daemon(vec![Script::Watched(Some("daemon-a".to_owned()), seen)]);

        let backend = bounded("the fenced kill", move || {
            let refused = backend
                .kill_workspace("ade-proj-000001", Some("daemon-b"))
                .expect_err("a workspace held by another daemon");
            assert!(
                format!("{refused:#}").contains("daemon-b"),
                "the refusal names the daemon the operation belongs to: {refused:#}"
            );
            // A permitted request on the same connection, so what the daemon
            // received can be asserted rather than merely awaited.
            backend.list().expect("an unfenced listing");
            backend
        });

        assert!(
            matches!(
                received.recv().expect("the daemon received something"),
                Frame::ListSessions { .. }
            ),
            "the kill reached the wrong daemon"
        );
        drop(backend);
    }

    /// A daemon too old to name itself must not inherit a named daemon's rows:
    /// "no identity" is the answer every pre-identity daemon gives.
    #[test]
    fn an_expected_identity_refuses_a_nameless_daemon() {
        let backend = scripted_daemon(vec![identity_listing(None, 1)]);

        let refused = bounded("the fenced kill", move || {
            backend
                .kill_session("s1", Some("daemon-a"))
                .expect_err("a nameless daemon cannot answer for a named one")
        });
        assert!(
            format!("{refused:#}").contains("an unnamed daemon"),
            "{refused:#}"
        );
    }

    /// A cached connection to the daemon that has since been replaced is worth
    /// one reconnect: nothing was written for this request, so nothing can be
    /// written twice.
    #[test]
    fn a_cached_connection_to_another_daemon_is_reconnected_once() {
        let backend = listed(scripted_daemon(vec![
            identity_listing(Some("daemon-a"), 1),
            Script::WithIdentity(
                Some("daemon-b".to_owned()),
                vec![Frame::WorkspaceRemoved {
                    workspace_id: "ade-proj-000001".to_owned(),
                    persisted: true,
                    request_id: Some(2),
                }],
            ),
        ]));

        bounded("the fenced kill", move || {
            backend
                .kill_workspace("ade-proj-000001", Some("daemon-b"))
                .expect("the replacement daemon answers it");
        });
    }

    /// **The status channel observes; it never authorizes.** Both connections
    /// handshake independently, so the identity a UI reads can already be the
    /// next daemon's while the control connection still holds the last one.
    #[test]
    fn a_status_identity_cannot_authorize_a_control_request() {
        let (seen, received) = std::sync::mpsc::channel();
        let backend = scripted_daemon(vec![
            Script::Watched(Some("daemon-a".to_owned()), seen.clone()),
            Script::Watched(Some("daemon-a".to_owned()), seen),
        ]);

        let backend = bounded("the first listing", move || {
            backend.list().expect("the listing that connects");
            backend
        });
        assert!(matches!(
            received.recv().expect("the listing arrived"),
            Frame::ListSessions { .. }
        ));
        // What a status connection to a replacement daemon records.
        *backend
            .endpoint
            .identity
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some("daemon-b".to_owned());

        let backend = bounded("the fenced kill", move || {
            backend
                .kill_workspace("ade-proj-000001", Some("daemon-b"))
                .expect_err("the control connection is still daemon-a's");
            backend.list().expect("an unfenced listing still works");
            backend
        });
        assert!(matches!(
            received.recv().expect("the second listing arrived"),
            Frame::ListSessions { .. }
        ));
        assert_eq!(
            backend.instance_id(),
            Some("daemon-a".to_owned()),
            "the fresh control handshake replaces the stale status observation"
        );
        drop(backend);
    }

    /// A listing is attributed to the connection that answered it, not to
    /// whatever the endpoint's shared identity says afterwards.
    #[test]
    fn a_listing_carries_the_identity_that_answered_it() {
        let backend = scripted_daemon(vec![
            Script::WithIdentity(
                Some("daemon-a".to_owned()),
                vec![Frame::SessionList {
                    sessions: Vec::new(),
                    request_id: Some(1),
                }],
            ),
            Script::WithIdentity(
                Some("daemon-a".to_owned()),
                vec![Frame::WorkspaceList {
                    workspaces: Vec::new(),
                    request_id: Some(2),
                }],
            ),
        ]);

        let sessions = bounded("the session listing", move || {
            let sessions = backend.list_identified().expect("the session listing");
            // The status channel's answer, landing between the two listings.
            *backend
                .endpoint
                .identity
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some("daemon-b".to_owned());
            force_reconnect(&backend);
            let workspaces = backend
                .list_workspaces_identified()
                .expect("the workspace listing");
            assert_eq!(workspaces.daemon_id.as_deref(), Some("daemon-a"));
            sessions
        });
        assert_eq!(sessions.daemon_id.as_deref(), Some("daemon-a"));
    }

    /// The attach argv is where a persisted identity reaches the attach client,
    /// which connects on its own and cannot be fenced from here.
    #[test]
    fn an_attach_argv_carries_the_expected_daemon_identity() {
        let backend = scripted_daemon(Vec::new());

        let argv = backend
            .attach_session("s1", Some("daemon-a"))
            .expect("the argv is built without a round trip");
        assert_eq!(
            argv.windows(2)
                .find(|pair| pair[0] == "--expected-daemon-id"),
            Some(["--expected-daemon-id".to_owned(), "daemon-a".to_owned()].as_slice()),
        );
        assert!(
            !backend
                .attach_session("s1", None)
                .expect("the unfenced argv")
                .contains(&"--expected-daemon-id".to_owned()),
            "a legacy row names no daemon"
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
            },
        ]);
        let (sender, receiver) = smol::channel::unbounded();
        let mut known = HashMap::new();
        let mut has_snapshot = false;

        for _ in 0..3 {
            let mut subscribed = false;
            smol::block_on(stream_status_once(
                &backend.endpoint,
                &sender,
                &mut subscribed,
                &mut known,
                &mut has_snapshot,
            ))
            .expect_err("the scripted connection closes after its snapshot");
            assert!(subscribed);
        }

        let events: Vec<_> = std::iter::from_fn(|| receiver.try_recv().ok()).collect();
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
            backend.daemon_sessions(None)
        })
        .expect_err("the incompatible daemon must be surfaced to the connect flow");
        assert!(
            is_incompatible_daemon(&failure),
            "unexpected failure: {failure:#}"
        );
    }
}

#[cfg(test)]
mod ensure_report_tests {
    use super::EnsureReport;

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
                upgrade_ready: false
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
                upgrade_ready: true
            }
        );
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
}

#[cfg(test)]
mod pre_cut_fallback_tests {
    use super::{is_incompatible_daemon, pre_cut_kill_script};
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
        assert!(is_incompatible_daemon(&diagnosed));

        let ordinary = anyhow!("connection refused").context("spawning the shutdown channel");
        assert!(!is_incompatible_daemon(&ordinary));
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
        let script = super::stale_daemon_recovery_script(
            std::path::Path::new("/home/user name/repo"),
            Some(("/home/user name/.ade/daemon", "daemon-a")),
        )
        .expect("the remote worktree path should produce a script");

        assert!(script.contains("root='/home/user name/repo'"));
        assert!(script.contains("instance_file='/home/user name/.ade/daemon/instance.id'"));
        assert!(script.contains("expected_daemon_id='daemon-a'"));
        assert!(script.contains("\"$root\"|\"$root\"/*"));
        assert!(script.contains("tty=$5"));
        assert!(script.contains("kill -HUP -\"$group\""));
        assert!(script.contains("kill -KILL -\"$group\""));
        assert!(script.find("actual_daemon_id=").unwrap() < script.find("kill -HUP").unwrap());
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

    #[test]
    #[cfg(unix)]
    fn stale_daemon_recovery_refuses_the_wrong_or_missing_daemon() {
        use std::{fs, process::Command};

        let worktree = tempfile::TempDir::new().expect("temporary worktree");
        let state = tempfile::TempDir::new().expect("temporary daemon state");
        let instance_file = state.path().join("instance.id");
        fs::write(&instance_file, "daemon-a\n").expect("writing daemon identity");

        let run = |expected_daemon_id: Option<&str>| {
            let state_dir = state.path().to_str().expect("UTF-8 state path");
            let script = super::stale_daemon_recovery_script(
                worktree.path(),
                expected_daemon_id.map(|expected| (state_dir, expected)),
            )
            .expect("recovery script");
            Command::new("sh")
                .args(["-c", &script])
                .output()
                .expect("running recovery script")
        };

        assert!(run(Some("daemon-a")).status.success());

        let mismatch = run(Some("daemon-b"));
        assert!(!mismatch.status.success());
        assert!(String::from_utf8_lossy(&mismatch.stderr).contains("daemon identity mismatch"));

        fs::remove_file(instance_file).expect("removing daemon identity");
        let missing = run(Some("daemon-a"));
        assert!(!missing.status.success());
        assert!(String::from_utf8_lossy(&missing.stderr).contains("cannot verify daemon identity"));

        assert!(run(None).status.success());
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

    fn spec(id: &str, directory: &TempDir) -> SessionSpec {
        SessionSpec::new(id, directory.path())
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
        let spec = spec("ade-main-000001", &dir);

        // Creating hands back the *caller's* id, so the registry can go on
        // caching the name it derived.
        assert_eq!(backend.create(&spec, None).unwrap(), spec.id);
        assert_eq!(
            backend.list().unwrap(),
            vec![SessionInfo {
                id: spec.id.clone()
            }]
        );
        assert!(backend.exists(&spec.id, None).unwrap());
        assert!(
            !backend
                .exists(&SessionId::from("ade-other-000002"), None)
                .unwrap()
        );

        // Creating twice is refused rather than silently duplicated.
        assert!(backend.create(&spec, None).is_err());

        // Detaching is a no-op that leaves everything running.
        backend.detach(&spec.id).unwrap();
        assert!(backend.exists(&spec.id, None).unwrap());

        backend.kill(&spec.id, None).unwrap();
        assert!(!backend.exists(&spec.id, None).unwrap());
        assert!(backend.list().unwrap().is_empty());
        // And killing what is already gone is the state the caller asked for.
        backend.kill(&spec.id, None).unwrap();
    }

    #[test]
    fn attach_names_our_own_client_and_creates_what_is_missing() {
        let (dir, _server, backend) = backend();
        let spec = spec("ade-main-000003", &dir);

        // Attach-or-create: nothing exists yet, and the argv still works.
        let attached = backend.attach(&spec, None).unwrap();
        let argv = &attached.argv;
        assert!(backend.exists(&spec.id, None).unwrap());
        assert_eq!(argv[0], "/opt/ade/ade-daemon");
        assert_eq!(argv[1], "attach");
        assert_eq!(argv[3], "--socket");
        assert!(argv[4].ends_with("daemon.sock"), "{argv:?}");

        // The id in the argv is the daemon's own, which is the only name the
        // client can attach by — and it comes back beside the argv, because a
        // creating attach is the one caller that cannot know it in advance.
        let session = backend.live_session(&spec.id, None).unwrap().unwrap();
        assert_eq!(argv[2], session.id.to_string());
        assert_eq!(attached.session_id, session.id.to_string());

        // Attaching again reattaches rather than creating a second session.
        assert_eq!(backend.attach(&spec, None).unwrap(), attached);
        assert_eq!(backend.list().unwrap().len(), 1);
    }

    #[test]
    fn a_new_backend_reattaches_after_the_app_disconnects_without_duplicating() {
        let (dir, server, backend) = backend();
        let spec = spec("ade-reconnect-000004", &dir);
        let first = backend.attach(&spec, None).expect("the first app attaches");
        drop(backend);

        let reopened = DaemonBackend::connected_to(server.socket_path(), "/opt/ade/ade-daemon");
        let second = reopened
            .attach(&spec, None)
            .expect("the restarted app reattaches");

        assert_eq!(second, first);
        assert_eq!(
            reopened
                .daemon_sessions(None)
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
        let spec = spec("ade-plural-000020", &dir);
        backend.create(&spec, None).unwrap();
        let first = backend.live_session(&spec.id, None).unwrap().unwrap().id;

        let second = backend
            .create_session_in_workspace(spec.id.as_str(), dir.path(), None)
            .unwrap();
        assert_ne!(second, first.to_string(), "the daemon mints a fresh id");

        // Both are the daemon's, both are in the workspace, and neither reaping
        // nor a one-live-session guard took the other.
        let held: Vec<String> = backend
            .daemon_sessions(None)
            .unwrap()
            .into_iter()
            .filter(|session| session.workspace_id == spec.id.as_str())
            .map(|session| session.id.to_string())
            .collect();
        assert_eq!(held.len(), 2, "{held:?}");
        assert!(held.contains(&first.to_string()) && held.contains(&second));

        // The seam above is keyed by the workspace, so N sessions are still one
        // row and one dot.
        assert!(backend.exists(&spec.id, None).unwrap());
        assert_eq!(
            backend.list().unwrap(),
            vec![SessionInfo {
                id: spec.id.clone()
            }]
        );

        // A document naming both is accepted: the daemon validates that every
        // terminal tab is a session it owns, and both are.
        let stored = backend.open_workspace(spec.id.as_str(), None).unwrap();
        let both = LayoutDoc::new(ade_session::LayoutNode::leaf(vec![
            ade_session::Tab::Terminal {
                session_id: first.clone(),
            },
            ade_session::Tab::Terminal {
                session_id: proto::SessionId::new(&second),
            },
        ]));
        backend
            .update_layout(spec.id.as_str(), &both, stored.rev + 1, None)
            .unwrap();
        assert_eq!(
            backend
                .open_workspace(spec.id.as_str(), None)
                .unwrap()
                .layout,
            both
        );

        // Closing one tab takes one session. The sibling keeps running and the
        // workspace record — layout and all — is untouched, which is what makes
        // this different from `kill`.
        backend.kill_session(&second, None).unwrap();
        assert!(backend.exists(&spec.id, None).unwrap());
        let left: Vec<String> = backend
            .daemon_sessions(None)
            .unwrap()
            .into_iter()
            .filter(|session| session.workspace_id == spec.id.as_str())
            .map(|session| session.id.to_string())
            .collect();
        assert_eq!(left, vec![first.to_string()]);
        assert!(backend.open_workspace(spec.id.as_str(), None).is_ok());

        let replacement = backend
            .create_session_in_workspace(spec.id.as_str(), dir.path(), None)
            .unwrap();
        let sibling = SessionSpec::new("ade-plural-sibling-000021", dir.path());
        backend.create(&sibling, None).unwrap();

        backend.kill(&spec.id, None).unwrap();

        let remaining = backend.daemon_sessions(None).unwrap();
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
            backend.open_workspace(spec.id.as_str(), None).is_ok(),
            "killing sessions must keep the workspace record"
        );
        assert!(
            backend.open_workspace(sibling.id.as_str(), None).is_ok(),
            "killing one workspace's sessions must keep its sibling"
        );
        assert!(!backend.exists(&spec.id, None).unwrap());

        assert_eq!(backend.create(&spec, None).unwrap(), spec.id);
        assert!(backend.exists(&spec.id, None).unwrap());
        assert!(
            backend
                .daemon_sessions(None)
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
        let spec = spec("ade-layout-000010", &dir);
        backend.create(&spec, None).unwrap();
        let session = backend.live_session(&spec.id, None).unwrap().unwrap();

        // The daemon made the workspace record when the session named it, and
        // seeded it with the one tab that session is.
        let stored = backend.open_workspace(spec.id.as_str(), None).unwrap();
        assert_eq!(
            stored.layout,
            LayoutDoc::single_terminal(session.id.clone()),
            "a fresh workspace is one leaf holding its session"
        );

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
            .update_layout(spec.id.as_str(), &split, stored.rev + 1, None)
            .unwrap();

        let reread = backend.open_workspace(spec.id.as_str(), None).unwrap();
        assert_eq!(reread.layout, split);
        assert_eq!(reread.rev, stored.rev + 1);

        // The same revision again is a client writing from a view it has been
        // told is out of date. It loses, and learns that it lost.
        let error = backend
            .update_layout(spec.id.as_str(), &split, stored.rev + 1, None)
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("stale"),
            "a refused write must say why: {error:#}"
        );
        // And the refusal changed nothing.
        assert_eq!(
            backend.open_workspace(spec.id.as_str(), None).unwrap(),
            reread
        );

        // A workspace nobody ever made has no layout to render.
        assert!(backend.open_workspace("ade-nothing-000011", None).is_err());
    }

    /// An accepted layout reaches this client's *event* stream, because that is
    /// a different connection from the control one the daemon excluded — which
    /// is exactly why `layout::broadcast_action` exists.
    #[test]
    fn an_accepted_layout_comes_back_on_the_event_stream() {
        let (dir, _server, backend) = backend();
        let spec = spec("ade-layout-000012", &dir);
        backend.create(&spec, None).unwrap();
        let session = backend.live_session(&spec.id, None).unwrap().unwrap();

        let events = backend.subscribe_events().unwrap();
        // Drains the subscribe snapshot and proves the stream is live.
        smol::block_on(next_session(&events));

        let stored = backend.open_workspace(spec.id.as_str(), None).unwrap();
        let layout = LayoutDoc::new(ade_session::LayoutNode::leaf(vec![
            ade_session::Tab::Terminal {
                session_id: session.id,
            },
            ade_session::Tab::Editor {
                path: "/repos/zed/main.rs".to_owned(),
            },
        ]));
        backend
            .update_layout(spec.id.as_str(), &layout, stored.rev + 1, None)
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
        let spec = spec("ade-layout-000014", &dir);
        backend.create(&spec, None).unwrap();
        assert!(backend.open_workspace(spec.id.as_str(), None).is_ok());

        let events = backend.subscribe_events().unwrap();
        // Drains the subscribe snapshot and proves the stream is live.
        smol::block_on(next_session(&events));

        backend.kill_workspace(spec.id.as_str(), None).unwrap();

        // The sessions are gone, and so is the workspace they were in — unlike
        // `kill`, which would leave the record behind holding dead tabs.
        assert!(backend.list().unwrap().is_empty());
        assert!(!backend.exists(&spec.id, None).unwrap());
        assert!(
            backend.open_workspace(spec.id.as_str(), None).is_err(),
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
        assert!(backend.kill_workspace(spec.id.as_str(), None).is_err());
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
        assert!(backend.list_workspaces().unwrap().is_empty());

        let spec = spec("ade-adopt-000020", &dir);
        backend.create(&spec, None).unwrap();

        let listed = backend.list_workspaces().unwrap();
        assert_eq!(listed.len(), 1);
        let workspace = &listed[0];
        // Keyed by the seam's id, which is what an adopted row records as its
        // `terminal_session_id` and addresses the workspace by ever after.
        assert_eq!(workspace.id, spec.id.as_str());
        assert_eq!(workspace.project_root, dir.path().display().to_string());
        assert!(workspace.created_at > 0);

        // A rename is the daemon's to own, so adoption sees the new name.
        backend
            .rename_workspace(&workspace.id, "vector DB", None)
            .unwrap();
        assert_eq!(backend.list_workspaces().unwrap()[0].name, "vector DB");

        // Killed: gone from the listing, so there is nothing to adopt back.
        backend.kill_workspace(spec.id.as_str(), None).unwrap();
        assert!(backend.list_workspaces().unwrap().is_empty());
    }

    /// The layout's own attach: an argv for a session the daemon already has,
    /// which never creates one.
    #[test]
    fn attaching_by_session_id_names_the_client_without_creating_anything() {
        let (dir, _server, backend) = backend();
        let spec = spec("ade-layout-000013", &dir);
        backend.create(&spec, None).unwrap();
        let session = backend.live_session(&spec.id, None).unwrap().unwrap();

        let argv = backend.attach_session(session.id.as_str(), None).unwrap();
        assert_eq!(argv[0], "/opt/ade/ade-daemon");
        assert_eq!(argv[1], "attach");
        assert_eq!(argv[2], session.id.to_string());
        assert_eq!(argv[3], "--socket");

        // A session id nobody owns still produces an argv — the client is what
        // discovers that, and nothing is created here either way.
        assert_eq!(backend.list().unwrap().len(), 1);
        backend.attach_session("not-a-session", None).unwrap();
        assert_eq!(backend.list().unwrap().len(), 1);

        // And killing by the daemon's own id takes that one session.
        backend.kill_session(session.id.as_str(), None).unwrap();
        assert!(backend.list().unwrap().is_empty());
    }

    #[test]
    fn a_session_whose_process_is_gone_is_not_a_live_session() {
        let (dir, server, backend) = backend();
        let spec = spec("ade-main-000004", &dir);
        backend.create(&spec, None).unwrap();
        let session = backend.live_session(&spec.id, None).unwrap().unwrap();

        // The shell exits: the daemon keeps the row, and this seam stops
        // reporting it, so the workspace reads as disconnected upstairs.
        smol::block_on(server.sessions().write(&session.id, b"exit\n")).unwrap();
        eventually("the session to be reported dead", || {
            !backend.exists(&spec.id, None).unwrap()
        });
        assert!(backend.list().unwrap().is_empty());

        // Recreating replaces the tombstone instead of piling up beside it.
        backend.create(&spec, None).unwrap();
        assert!(backend.exists(&spec.id, None).unwrap());
        assert_eq!(server.sessions().list().len(), 1);
    }

    #[test]
    fn status_is_pushed_and_named_by_the_seams_ids() {
        let (dir, _server, backend) = backend();
        assert_eq!(backend.status_delivery(), StatusDelivery::Push);

        // A session that already exists when the stream opens: subscribing
        // pushes a snapshot of it, and receiving that is also what proves the
        // subscription is live before anything below depends on it.
        let existing = spec("ade-existing-000005", &dir);
        backend.create(&existing, None).unwrap();
        let events = backend.subscribe_events().unwrap();
        let snapshot = smol::block_on(next_session(&events));
        assert_eq!(snapshot.id, existing.id);
        assert_eq!(
            snapshot.change,
            SessionChange::Status(WorkspaceStatus::Running)
        );

        // And one that appears afterwards, which the daemon announces itself.
        let fresh = spec("ade-fresh-000006", &dir);
        backend.create(&fresh, None).unwrap();
        let created = smol::block_on(next_for(&events, &fresh.id));
        assert_eq!(
            created.change,
            SessionChange::Created(WorkspaceStatus::Running)
        );
        assert_eq!(created.change.status(), Some(WorkspaceStatus::Running));

        // Killing takes the row out, and that is pushed too.
        backend.kill(&fresh.id, None).unwrap();
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
            address: LocalEndpoint::Socket(PathBuf::from("/run/ade/daemon.sock")),
            state_dir: PathBuf::from("/var/lib/ade"),
            transport: Transport::Proxy,
            identity: Arc::new(Mutex::new(None)),
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
        let LocalEndpoint::Socket(socket_path) = &endpoint.address else {
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

        let LocalEndpoint::Loopback(port) = backend.endpoint.address else {
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
        assert_eq!(link.local, backend.endpoint.address);
    }

    #[test]
    fn a_socket_endpoint_is_named_to_the_client_as_a_socket() {
        assert_eq!(
            client_argv(&LocalEndpoint::Socket(PathBuf::from(
                "/run/ade/daemon.sock"
            ))),
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
            address: LocalEndpoint::Socket(PathBuf::from("/run/ade/daemon.sock")),
            state_dir: PathBuf::from("/var/lib/ade"),
            transport: Transport::Proxy,
            identity: Arc::new(Mutex::new(None)),
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
