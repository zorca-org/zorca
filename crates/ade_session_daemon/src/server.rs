//! The Unix-socket server: accept, handshake, request loop.
//!
//! One connection per client; many connections may be open at once and each is
//! served by its own task. A connection is a *view* of the session table, never
//! an owner of it — **when a client goes away the sessions it created keep
//! running**. That is the whole reason the daemon exists.
//!
//! Each connection is two tasks over one transport: a request loop that reads
//! frames, and a *writer* task that owns the only writing half. Everything the
//! daemon sends — replies, replays, live output and status events — is queued
//! on one [bounded](OUTBOUND_QUEUE_BYTES) channel per connection and written
//! by that task alone. Nothing else touches the transport, so frames can never
//! interleave, and the pty drain threads (which are plain std threads, see
//! [`crate::sessions`]) push output without ever entering the executor.
//!
//! v1 transport is a Unix socket, so this module is unix-only; Windows gets a
//! named pipe when it gets a daemon at all.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ade_session::client::Connection;
use ade_session::framing::{ReadFrameError, bounded, bounded_debug, rejection_frame};
use ade_session::proto::{
    Frame, HelloAck, LayoutDoc, MAX_GENERATION, MIN_GENERATION, SessionId, error_code,
    select_generation, validate_capabilities,
};
#[cfg(unix)]
use anyhow::bail;
use anyhow::{Context as _, Result};
use smol::io::{AsyncRead, AsyncWrite};
#[cfg(unix)]
use smol::net::unix::UnixListener;

use crate::DAEMON_VERSION;
use crate::sessions::{
    CreateRequest, Outbound, SessionTable, StatusConfig, SubscriberId, TableError, WorkspaceRequest,
};
use crate::state::StateStore;
use crate::state::create_private_dir;

/// How long a daemon that serves nobody and holds only tombstones waits
/// before exiting on its own.
///
/// Long enough that a client restart, a rebuild, or a lunch break never
/// observes a daemon vanishing under it; short enough that an abandoned host
/// does not carry an idle daemon for weeks — and, with binary identity in the
/// handshake, that an idle daemon stops pinning a stale binary.
const IDLE_EXIT_AFTER: Duration = Duration::from_secs(15 * 60);

/// How many *bytes* one connection may have queued but unwritten before the
/// daemon calls it a stalled consumer and drops it.
///
/// Bounded because the pty drain thread filling it must never block
/// ([`crate::sessions`]); unbounded, a stalled client would turn its
/// session's output into unlimited daemon memory. 2 MiB, matching
/// [`DEFAULT_SCROLLBACK_BYTES`](crate::sessions::DEFAULT_SCROLLBACK_BYTES) —
/// past that a reader is caught up only by a re-attach's repaint, not the
/// stream. **Bytes, not frames**: see [`Outbound`] / [`Outbound::push`] for
/// why, and for the drop-on-full rule.
pub const OUTBOUND_QUEUE_BYTES: u64 = 2 * 1024 * 1024;

/// The previous generation in the two-generation window
/// ([`MIN_GENERATION`]), whose dialect this daemon still serves on connections
/// that negotiated it.
pub(crate) const LEGACY_GENERATION: u32 = 2;

/// The pty size generation 2 gave a `create_workspace` that named none.
const LEGACY_DEFAULT_COLS: u16 = 80;
const LEGACY_DEFAULT_ROWS: u16 = 24;

/// Where the daemon listens, where it keeps its session list, and how it times
/// status derivation.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub socket_path: PathBuf,
    pub state_dir: PathBuf,
    /// Defaults to ADE's spec values; only tests should move it.
    pub status: StatusConfig,
    /// Exit after this long with no connections and only tombstone rows.
    /// `None` disables idle exit entirely.
    pub idle_exit_after: Option<Duration>,
}

impl ServerConfig {
    pub fn new(socket_path: impl Into<PathBuf>, state_dir: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
            state_dir: state_dir.into(),
            status: StatusConfig::default(),
            idle_exit_after: Some(IDLE_EXIT_AFTER),
        }
    }

    /// Tune status derivation. Tests use this to make a five-second rule
    /// observable in milliseconds.
    pub fn with_status(mut self, status: StatusConfig) -> Self {
        self.status = status;
        self
    }

    /// Tune or disable idle exit. Tests use this to make a fifteen-minute
    /// rule observable in milliseconds.
    pub fn with_idle_exit(mut self, after: Option<Duration>) -> Self {
        self.idle_exit_after = after;
        self
    }

    /// `$XDG_RUNTIME_DIR/ade/daemon.sock`, falling back to
    /// `~/.ade/daemon.sock` on hosts without a runtime dir (macOS, most ssh
    /// sessions).
    #[cfg(unix)]
    pub fn default_socket_path() -> PathBuf {
        if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR")
            && !runtime_dir.is_empty()
        {
            return PathBuf::from(runtime_dir).join("ade").join("daemon.sock");
        }
        ade_home().join("daemon.sock")
    }

    /// `~/.ade/daemon/`.
    pub fn default_state_dir() -> PathBuf {
        ade_home().join("daemon")
    }
}

#[cfg(unix)]
impl Default for ServerConfig {
    fn default() -> Self {
        Self::new(Self::default_socket_path(), Self::default_state_dir())
    }
}

/// `~/.ade`, where `~` is `$HOME`.
pub(crate) fn ade_home() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) {
        return PathBuf::from(home).join(".ade");
    }
    std::env::temp_dir().join("ade")
}

/// What a connection can say and do on behalf of the daemon as a whole: the
/// identity it reports in the handshake, and the one orderly way out.
///
/// [`DaemonControl::exit`] runs at most once — unlink the socket so nothing
/// new can connect or forward to it, drop the pid file, then whatever "exit"
/// means for this server: `process::exit(0)` for the real daemon, a flag for
/// an in-process one.
///
/// What "nothing is lost" means differs between its two callers. The idle
/// watcher only ever fires over an empty table
/// ([`SessionTable::only_tombstones`]) — an idle shell is not a pointless
/// daemon. An accepted [`Frame::Shutdown`] uses the looser
/// [`SessionTable::expendable`], so a manual upgrade may take idle shells with
/// it: the workspace records are persisted and come back whole, their shells as
/// lost rows. A [`Frame::Shutdown`] with `force` set checks nothing at all — a
/// human clicked, and whatever went with the process comes back the same way.
struct DaemonControl {
    /// Hex sha256 of the executable this daemon started from, reported in
    /// [`HelloAck::binary_hash`]. `None` if the executable could not be read
    /// back, which a client must treat as "unknown, leave it alone".
    binary_hash: Option<String>,
    /// Which daemon this is, across restarts — see
    /// [`StateStore::instance_id`] and [`HelloAck::instance_id`]. Read once at
    /// startup: the file behind it may be deleted while the daemon runs, and a
    /// client that has already been told an identity must not be told another.
    instance_id: String,
    socket_path: PathBuf,
    state_dir: PathBuf,
    fired: AtomicBool,
    shutting_down: AtomicBool,
    // ponytail: one daemon-wide admission counter is enough; shard by
    // workspace only if admission contention becomes measurable.
    active_state_requests: Mutex<usize>,
    on_exit: Box<dyn Fn() + Send + Sync>,
}

impl DaemonControl {
    fn admit_state_request(&self) -> Option<StateRequestGuard<'_>> {
        let mut active = self
            .active_state_requests
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.shutting_down.load(Ordering::SeqCst) {
            return None;
        }
        *active += 1;
        Some(StateRequestGuard { control: self })
    }

    fn admit_shutdown(&self, force: bool, expendable: impl FnOnce() -> bool) -> bool {
        let active = self
            .active_state_requests
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if force || (*active == 0 && expendable()) {
            self.shutting_down.store(true, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    fn exit(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        if self.fired.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Err(err) = unlink_endpoint(&self.socket_path) {
            log::warn!(
                "could not unlink {} on exit: {err}",
                self.socket_path.display()
            );
        }
        StateStore::new(&self.state_dir).remove_pid();
        (self.on_exit)();
    }
}

/// Take the endpoint out of the namespace so nothing new can connect to or
/// forward to it.
fn unlink_endpoint(path: &Path) -> std::io::Result<()> {
    std::fs::remove_file(path)
}

struct StateRequestGuard<'a> {
    control: &'a DaemonControl,
}

impl Drop for StateRequestGuard<'_> {
    fn drop(&mut self) {
        let mut active = self
            .control
            .active_state_requests
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *active -= 1;
    }
}

/// Hex sha256 of the running executable, hashed once at startup.
///
/// Read from the file rather than from memory so it matches what a client
/// hashes: the bytes it would upload. On Linux `current_exe` stays correct
/// even if the file is later replaced — the hash is taken now, while the path
/// still names the image actually running.
fn hash_current_exe() -> Option<String> {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            log::warn!("cannot locate this executable to hash it: {err}");
            return None;
        }
    };
    match std::fs::read(&exe) {
        Ok(bytes) => Some(ade_session::sha256_hex(&bytes)),
        Err(err) => {
            log::warn!("cannot read {} to hash it: {err}", exe.display());
            None
        }
    }
}

/// The bound listener.
type Listener = UnixListener;

/// Bind the endpoint, refusing to start beside a daemon already serving it.
///
/// A leftover unix socket is removed only after a connect proves nobody
/// answers — a successful connect means a second daemon would strand the first.
fn bind_listener(socket_path: &Path) -> Result<Listener> {
    if let Some(parent) = socket_path.parent() {
        create_private_dir(parent)?;
    }
    if socket_path.exists() {
        match std::os::unix::net::UnixStream::connect(socket_path) {
            Ok(_) => bail!(
                "daemon already running on {} — refusing to start a second one",
                socket_path.display()
            ),
            Err(err) => {
                log::info!("removing stale socket {} ({err})", socket_path.display());
                std::fs::remove_file(socket_path)
                    .with_context(|| format!("removing {}", socket_path.display()))?;
            }
        }
    }
    UnixListener::bind(socket_path).with_context(|| format!("binding {}", socket_path.display()))
}

/// A bound listener plus the session table it serves.
pub struct Server {
    listener: Listener,
    socket_path: PathBuf,
    state_dir: PathBuf,
    sessions: Arc<SessionTable>,
    binary_hash: Option<String>,
    instance_id: String,
    idle_exit_after: Option<Duration>,
}

impl Server {
    /// Bind the endpoint (see [`bind_listener`]) and load previous session
    /// metadata.
    pub fn bind(config: ServerConfig) -> Result<Self> {
        let socket_path = config.socket_path.clone();
        let listener = bind_listener(&socket_path)?;
        // A daemon started by the proxy's start-if-absent has no terminal and
        // no parent that outlives it, so the pid file is the only way anything
        // finds it afterwards. Failing to write it is not a reason to refuse to
        // serve; nothing in the daemon reads it back.
        let state_dir = config.state_dir.clone();
        let state = StateStore::new(&state_dir);
        if let Err(err) = state.write_pid() {
            log::warn!("could not record the daemon pid: {err:#}");
        }
        let sessions = SessionTable::load(StateStore::new(config.state_dir), config.status);
        Ok(Self {
            listener,
            socket_path,
            state_dir,
            sessions,
            binary_hash: hash_current_exe(),
            instance_id: state.instance_id(),
            idle_exit_after: config.idle_exit_after,
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn sessions(&self) -> &Arc<SessionTable> {
        &self.sessions
    }

    /// Accept forever — or until the daemon exits itself through
    /// [`DaemonControl`]. A failed accept is logged, not fatal.
    pub async fn run(self) -> Result<()> {
        log::info!("listening on {}", self.socket_path.display());
        let control = Arc::new(DaemonControl {
            binary_hash: self.binary_hash.clone(),
            instance_id: self.instance_id.clone(),
            socket_path: self.socket_path.clone(),
            state_dir: self.state_dir.clone(),
            fired: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            active_state_requests: Mutex::new(0),
            // The real daemon's exit is a process exit: the accept loop, the
            // sweeper and every drain thread go with it, and there is nothing
            // they hold that outlives them — that is what the callers proved.
            on_exit: Box::new(|| std::process::exit(0)),
        });
        spawn_idle_watcher(self.sessions.clone(), control.clone(), self.idle_exit_after);
        accept_loop(self.listener, self.sessions, control).await
    }

    /// Bind and run the accept loop on the global executor. Used by tests and
    /// by anything that wants a daemon inside its own process.
    ///
    /// "Exit" cannot mean `process::exit` here — this daemon shares a process
    /// with its owner — so an accepted shutdown or an idle exit only raises a
    /// flag ([`RunningServer::was_shutdown`]) and unlinks the socket.
    pub fn spawn(config: ServerConfig) -> Result<RunningServer> {
        let server = Self::bind(config)?;
        let socket_path = server.socket_path.clone();
        let state_dir = server.state_dir.clone();
        let sessions = server.sessions.clone();
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let control = Arc::new(DaemonControl {
            binary_hash: server.binary_hash.clone(),
            instance_id: server.instance_id.clone(),
            socket_path: socket_path.clone(),
            state_dir: state_dir.clone(),
            fired: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            active_state_requests: Mutex::new(0),
            on_exit: Box::new({
                let flag = shutdown_flag.clone();
                move || flag.store(true, Ordering::SeqCst)
            }),
        });
        spawn_idle_watcher(sessions.clone(), control.clone(), server.idle_exit_after);
        let task = smol::spawn(async move {
            if let Err(err) = accept_loop(server.listener, server.sessions, control).await {
                log::debug!("server stopped: {err:#}");
            }
        });
        Ok(RunningServer {
            socket_path,
            state_dir,
            sessions,
            shutdown_flag,
            _task: task,
        })
    }
}

/// Accept forever. A failed accept is logged, not fatal.
///
/// The two halves handed to [`serve_connection`] are two clones of the one
/// socket — the reader half and the single writer, same as always.
async fn accept_loop(
    listener: Listener,
    sessions: Arc<SessionTable>,
    control: Arc<DaemonControl>,
) -> Result<()> {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let sessions = sessions.clone();
                let control = control.clone();
                smol::spawn(async move {
                    if let Err(err) =
                        serve_connection(stream.clone(), stream, sessions, control).await
                    {
                        log::debug!("connection ended: {err:#}");
                    }
                })
                .detach();
            }
            Err(err) => log::warn!("accept failed: {err}"),
        }
    }
}

/// Watch for the daemon becoming pointless, and end it when it stays that way.
///
/// "Pointless" is nobody connected and nothing held but tombstones — the same
/// condition an accepted [`Frame::Shutdown`] requires, observed continuously
/// for `after` instead of at a client's request. The point is twofold: an
/// abandoned host does not carry an idle daemon indefinitely, and an idle
/// daemon stops pinning a stale binary — the next connect finds no socket and
/// deploys fresh bytes.
fn spawn_idle_watcher(
    sessions: Arc<SessionTable>,
    control: Arc<DaemonControl>,
    after: Option<Duration>,
) {
    let Some(after) = after else { return };
    smol::spawn(async move {
        // A table that has not rehydrated yet is all tombstones — the rows of
        // the sessions it is about to take back. Exiting over that would kill
        // the daemon for holding nothing, seconds before it holds everything.
        sessions.ready().await;
        let tick = (after / 4).clamp(Duration::from_millis(10), Duration::from_secs(30));
        let mut idle_since: Option<Instant> = None;
        loop {
            sleep(tick).await;
            if sessions.connection_count() > 0 || !sessions.only_tombstones() {
                idle_since = None;
                continue;
            }
            let since = *idle_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= after {
                let active_state_requests = control
                    .active_state_requests
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                if *active_state_requests == 0
                    && sessions.connection_count() == 0
                    && sessions.only_tombstones()
                {
                    control.shutting_down.store(true, Ordering::SeqCst);
                    drop(active_state_requests);
                    log::info!(
                        "nobody connected and nothing but tombstones for {after:?}; exiting"
                    );
                    control.exit();
                    return;
                }
                idle_since = None;
            }
        }
    })
    .detach();
}

/// Park a blocking thread. `smol::Timer` is disallowed by the workspace lints.
pub(crate) async fn sleep(duration: Duration) {
    smol::unblock(move || std::thread::sleep(duration)).await;
}

/// Handle to a server running on the global executor. Dropping it stops the
/// accept loop and unlinks the socket; it does **not** kill sessions, because
/// nothing except an explicit `Kill` ever does.
pub struct RunningServer {
    socket_path: PathBuf,
    state_dir: PathBuf,
    sessions: Arc<SessionTable>,
    shutdown_flag: Arc<AtomicBool>,
    _task: smol::Task<()>,
}

impl RunningServer {
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    pub fn sessions(&self) -> &Arc<SessionTable> {
        &self.sessions
    }

    /// Whether this in-process daemon "exited" — an accepted
    /// [`Frame::Shutdown`] or an idle exit. The real daemon is a dead process
    /// at this point; in-process it is this flag plus an unlinked socket.
    pub fn was_shutdown(&self) -> bool {
        self.shutdown_flag.load(Ordering::SeqCst)
    }
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        let _ = unlink_endpoint(&self.socket_path);
        StateStore::new(&self.state_dir).remove_pid();
    }
}

/// Balances [`SessionTable::connection_opened`] however the connection ends —
/// return, error, or the whole task being dropped mid-await.
///
/// Also remembers whether this connection ever went *active* — sent a frame
/// that touches a session or workspace — because the shutdown gate cares about
/// busy clients, not merely connected ones.
struct ConnectionGuard {
    sessions: Arc<SessionTable>,
    active: bool,
}

impl ConnectionGuard {
    fn new(sessions: Arc<SessionTable>) -> Self {
        Self {
            sessions,
            active: false,
        }
    }

    /// Sticky: once a connection has touched something it stays active until
    /// it closes, because an attach client that fell quiet is still a viewer.
    fn mark_active(&mut self) {
        if !self.active {
            self.active = true;
            self.sessions.connection_went_active();
        }
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        if self.active {
            self.sessions.active_connection_closed();
        }
        self.sessions.connection_closed();
    }
}

/// Does this frame touch a session or workspace, making its sender an active
/// client rather than a bystander? List-shaped requests, `Subscribe` and the
/// handshake do not: a connection that only ever asked questions survives a
/// daemon swap by reconnecting, and must not veto one.
///
/// It takes a decoded frame, so a request that never decoded — an unknown op, a
/// broken body — cannot mark a connection active. That is the right answer as
/// well as the only possible one: a frame the daemon could not read touched
/// nothing.
fn touches_state(frame: &Frame) -> bool {
    matches!(
        frame,
        Frame::CreateSession { .. }
            | Frame::Attach { .. }
            | Frame::Detach { .. }
            | Frame::Write { .. }
            | Frame::Resize { .. }
            | Frame::FocusSession { .. }
            | Frame::Kill { .. }
            | Frame::CreateWorkspace { .. }
            | Frame::UpdateLayout { .. }
            | Frame::RenameWorkspace { .. }
            | Frame::UpdateWorkspaceProject { .. }
            | Frame::KillWorkspace { .. }
    )
}

/// Handshake, then serve requests until the peer goes away — or asks the
/// daemon to.
///
/// `reader` and `writer` are the two halves of one connection, owned
/// separately because the writer is handed to a task of its own — see the
/// module doc for why that, not a cloneable duplex stream, is the seam. Only
/// one `Connection` is ever written to at a time: it writes the handshake,
/// then moves into the writer task, so no two writers can interleave a length
/// prefix with somebody else's payload.
async fn serve_connection<R, W>(
    reader: R,
    writer: W,
    sessions: Arc<SessionTable>,
    control: Arc<DaemonControl>,
) -> Result<()>
where
    R: AsyncRead + AsyncWrite + Unpin,
    W: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    sessions.connection_opened();
    let mut guard = ConnectionGuard::new(sessions.clone());
    if control.shutting_down.load(Ordering::SeqCst) {
        return Ok(());
    }

    let mut reader = Connection::new(reader);
    let mut writer = Connection::new(writer);
    // Held here for the connection's life and passed to every handler: control,
    // subscription and attach connections negotiate independently and may sit
    // at different generations at the same moment, so this can never be daemon
    // state (`docs/ade/protocol-compatibility.md` §3.3).
    let Some(generation) = handshake(&mut reader, &mut writer, &sessions, &control).await? else {
        return Ok(());
    };

    // Bounded: see [`OUTBOUND_QUEUE_BYTES`]. A producer that finds it full
    // closes it and drops its subscription rather than waiting — the drain
    // threads must never block on a client — and a closed queue ends this
    // writer task and then this connection, because the peer has stopped
    // reading the stream it asked for.
    let (outbound, queued) = Outbound::new(OUTBOUND_QUEUE_BYTES);
    let writer_task = smol::spawn(async move {
        while let Some(frame) = queued.recv().await {
            if let Err(err) = writer.send(&frame).await {
                log::debug!("connection writer stopped: {err:#}");
                break;
            }
        }
    });

    let subscriber = SubscriberId::next();
    let mut exiting = false;
    // Whether the loop ended having queued something the peer is owed. A
    // dropped writer task is *cancelled*, so anything still in the queue is
    // discarded — fine when the client is the one that vanished, wrong when the
    // daemon closed the connection and the reason is the last frame in it.
    let mut owes_a_frame = false;
    loop {
        // **The failure boundary is the request, not the connection**
        // (`docs/ade/protocol-compatibility.md` §1). Before the envelope, any
        // decode failure broke this loop and took every attach on the
        // connection with it; now only the transport can.
        let frame = match reader.recv().await {
            Ok(frame) => frame,
            // EOF or a broken pipe: the client is gone. Sessions are not.
            Err(ReadFrameError::Transport(err)) => {
                log::debug!("client disconnected: {err:#}");
                break;
            }
            Err(err) => {
                // `rejection_frame` decides what is answerable, and both ends
                // of the protocol call it, so the daemon cannot drift from the
                // client on which failures get a reply. `None` is the rid-less
                // request-scoped case: nothing to correlate a reply to and no
                // session named, so it is logged and dropped (§2).
                match rejection_frame(&err) {
                    Some(reply) => {
                        if !outbound.push(reply) {
                            break;
                        }
                        owes_a_frame = true;
                    }
                    None => log::debug!("dropping an unanswerable frame: {err}"),
                }
                if err.is_request_scoped() {
                    // The whole point: one frame this build cannot read costs
                    // one request, and the connection keeps serving.
                    continue;
                }
                // A peer that cannot frame an envelope is pre-cut or broken —
                // the spec permits closing here, and closing is what makes the
                // client-side diagnosis in §6.1 possible.
                log::warn!("closing the connection after a malformed frame: {err}");
                break;
            }
        };
        // Past the handshake, every frame is about the table — a request, a
        // subscription, or the shutdown decision, which is itself a question
        // about what the table holds. So this is the one place the rehydration
        // gate is waited on, and the handshake above deliberately is not: a
        // client connects and is answered at once, and only then waits for
        // whatever the table is still rehydrating. Free here; see
        // [`SessionTable::ready`].
        sessions.ready().await;
        if let Frame::Shutdown { force, request_id } = frame {
            // One condition, and it is about the *table*, not about who is
            // connected: nothing held that an upgrade may not sacrifice
            // ([`SessionTable::expendable`], which forgives lost rows and
            // idle shells). Looser than the idle watcher's rule on purpose —
            // an accepted shutdown may take idle shells with it. The process
            // exit closes their pty masters, the children die, and the rows
            // this daemon persisted come back under the next one as lost,
            // inside the workspaces the ledger restores whole. A shell at a
            // prompt is worth that; anything with work in it, or an exited
            // row's last screen, is not.
            //
            // `force` skips even that: it is set only when a human clicked
            // "upgrade host daemon", and the click is the consent. The
            // sessions die with the process the same way — the operator traded
            // them for the upgrade knowingly, which is more than the table can
            // know.
            //
            // Connections are deliberately *not* consulted either way.
            // Shutdown is reached by a human asking for it from an app that
            // is itself connected and busy with this daemon: gating on other
            // clients would decline the one request the operator asked for.
            // Admission and this decision share one short-held gate. A request
            // already in flight makes a polite shutdown decline immediately;
            // one behind an accepted shutdown is refused. The request itself
            // never holds the gate, because a PTY write may block indefinitely.
            if !control.admit_shutdown(force, || sessions.expendable()) {
                let reply = Frame::Error {
                    session_id: None,
                    workspace_id: None,
                    // Understood and not honoured, which is a different answer
                    // from "failed" and the client shows it differently: wait
                    // for the terminals or the request in flight to finish.
                    code: error_code::DECLINED.to_owned(),
                    message: "shutdown declined: another state request is in flight, this daemon \
                              holds a session with something running in it, or an exited session's \
                              last screen"
                        .to_owned(),
                    request_id,
                };
                if !outbound.push(reply) {
                    break;
                }
            } else {
                if force {
                    log::info!(
                        "shutdown forced: exiting over {} live session(s), because a human asked \
                         for the upgrade",
                        sessions.list().len()
                    );
                } else {
                    log::info!("shutdown accepted: nothing worth keeping is held; exiting");
                }
                outbound.push(Frame::ShutdownAck { request_id });
                exiting = true;
                break;
            }
            continue;
        }
        if touches_state(&frame) {
            guard.mark_active();
        }
        let reply = if touches_state(&frame) {
            match control.admit_state_request() {
                Some(_request) => {
                    handle_frame(frame, &sessions, subscriber, &outbound, generation).await
                }
                None => Some(Frame::Error {
                    session_id: frame.session_id().cloned(),
                    workspace_id: frame.workspace_id().map(str::to_owned),
                    code: error_code::DECLINED.to_owned(),
                    message: "daemon is shutting down".to_owned(),
                    request_id: frame.request_id(),
                }),
            }
        } else {
            handle_frame(frame, &sessions, subscriber, &outbound, generation).await
        };
        if let Some(reply) = reply
            && !outbound.push(reply)
        {
            break;
        }
    }

    // Losing the connection detaches everything it was watching. It kills
    // nothing: the ptys, the children and the scrollback all outlive it.
    sessions.detach_all(subscriber).await;
    // Closing the queue ends the writer task; anything still queued is for a
    // client that is no longer there — unless the daemon is exiting, in which
    // case the queue holds the ShutdownAck, or the daemon closed the connection
    // itself, in which case it holds the `malformed_frame` error that says why.
    // Both are awaited onto the wire; a dropped task would swallow them.
    drop(outbound);
    if exiting || owes_a_frame {
        writer_task.await;
    } else {
        drop(writer_task);
    }
    if exiting {
        control.exit();
    }
    Ok(())
}

/// The negotiation (`docs/ade/protocol-compatibility.md` §3): the daemon
/// **selects** the generation, the client verifies it.
///
/// `Ok(None)` means the handshake was rejected and the connection is done.
/// Everything that ends in `Ok(None)` has already said why on the wire, except
/// the undecodable cases that have nobody to answer. `Ok(Some(g))` is the
/// generation this connection — and only this connection — will be served at.
///
/// Writing directly on the writing half is deliberate and stays: the writer
/// task does not exist yet, and the handshake is strictly one frame in, one
/// frame out.
async fn handshake<R, W>(
    connection: &mut Connection<R>,
    writer: &mut Connection<W>,
    sessions: &SessionTable,
    control: &DaemonControl,
) -> Result<Option<u32>>
where
    R: AsyncRead + AsyncWrite + Unpin,
    W: AsyncRead + AsyncWrite + Unpin,
{
    let hello = match connection.recv().await {
        Ok(Frame::Hello(hello)) => hello,
        Ok(other) => {
            writer
                .send(&Frame::Error {
                    session_id: None,
                    workspace_id: None,
                    code: error_code::INVALID_ARGUMENT.to_owned(),
                    // Bounded, like every other quotation of something the peer
                    // sent: a frame's whole `Debug` rendering can be 16 MB, and
                    // an error frame that large cannot be written at all.
                    message: format!(
                        "expected hello as the first frame, got {}",
                        bounded_debug(&other)
                    ),
                    request_id: other.request_id(),
                })
                .await?;
            return Ok(None);
        }
        // Nothing can be read after a transport failure; the connection is
        // over and there is nothing to write an answer to.
        Err(ReadFrameError::Transport(err)) => return Err(err).context("reading hello"),
        // A decode failure *before* a generation has been agreed. The reply
        // rules are the same as mid-stream (§2) but the outcome is not: there
        // is no negotiated connection here worth keeping, so the daemon says
        // what it can and closes.
        Err(err) => {
            log::debug!("rejecting a connection whose hello did not decode: {err}");
            if let Some(reply) = rejection_frame(&err) {
                writer.send(&reply).await?;
            }
            return Ok(None);
        }
    };

    // §3.1: `G = min(maxes)`, valid only when it clears both minima. Disjoint
    // ranges are the one negotiation outcome that is fatal by design — there is
    // no frame shape both ends could agree to speak next.
    let Some(generation) = select_generation(
        hello.min_generation,
        hello.max_generation,
        MIN_GENERATION,
        MAX_GENERATION,
    ) else {
        writer
            .send(&Frame::Error {
                session_id: None,
                workspace_id: None,
                code: error_code::UNSUPPORTED_GENERATION.to_owned(),
                // Both ranges, because whoever reads this has to decide which
                // end to upgrade and cannot do that from one of them.
                message: format!(
                    "no protocol generation is common to this client's {}..={} and this daemon's \
                     {MIN_GENERATION}..={MAX_GENERATION}",
                    hello.min_generation, hello.max_generation,
                ),
                request_id: hello.request_id,
            })
            .await?;
        return Ok(None);
    };

    // §3.2: the *bounds* are fatal to the handshake; unknown identifiers and
    // duplicates are not errors at all and are handled by the intersection.
    if let Err(reason) = validate_capabilities(&hello.capabilities) {
        writer
            .send(&Frame::Error {
                session_id: None,
                workspace_id: None,
                code: error_code::INVALID_ARGUMENT.to_owned(),
                message: format!("unusable capability list: {reason}"),
                request_id: hello.request_id,
            })
            .await?;
        return Ok(None);
    }

    // The selected generation goes back to `serve_connection`, which holds it
    // for this connection alone (§3.3). The effective capability set is still
    // not kept: the intersection with an empty daemon list is always empty.
    writer
        .send(&Frame::HelloAck(HelloAck {
            daemon_version: DAEMON_VERSION.to_owned(),
            // Legacy informational, equal to the selected generation, and never
            // used for a compatibility decision again (§3.1).
            protocol_version: generation,
            host_os: std::env::consts::OS.to_owned(),
            min_generation: MIN_GENERATION,
            max_generation: MAX_GENERATION,
            generation,
            // The daemon advertises nothing at either generation. Capability
            // identifiers land with the feature that needs them, in the same
            // release that defines them (§5) — an empty list here is not a
            // placeholder, it is the honest answer.
            capabilities: Vec::new(),
            // The ledger is read-only for this daemon: it found one written by
            // a newer schema, or one it could not read at all. Mutations still
            // apply and still publish; they are simply not recorded (§8.5).
            // Only the flag is carried here — the per-ack `persisted` field is
            // a separate change.
            degraded: sessions.state().read_only(),
            binary_hash: control.binary_hash.clone(),
            // A snapshot, not a promise — the client re-proves it by sending
            // Shutdown, which re-checks under the same table.
            upgrade_ready: Some(sessions.expendable()),
            instance_id: Some(control.instance_id.clone()),
            request_id: hello.request_id,
        }))
        .await?;
    Ok(Some(generation))
}

/// The first session a generation-2 `create_workspace` spawns: the host's
/// login shell (the empty command), in the workspace root, at the size the
/// request named or 80x24.
fn combined_create_request(
    workspace_id: String,
    root: String,
    label: String,
    env: Vec<(String, String)>,
    cols: Option<u16>,
    rows: Option<u16>,
) -> CreateRequest {
    CreateRequest {
        workspace_id,
        cwd: root,
        command: String::new(),
        env,
        cols: cols.unwrap_or(LEGACY_DEFAULT_COLS),
        rows: rows.unwrap_or(LEGACY_DEFAULT_ROWS),
        agent_kind: "shell".to_owned(),
        instance_label: label,
        scrollback_bytes: None,
    }
}

/// Generation 2's `create_workspace`: the record, its first login shell and the
/// one-leaf layout holding that shell's tab, in one round trip.
///
/// Composed from the entry points generation 3 uses rather than reinstated
/// beside them, so every hardening since the cut — persist-before-ack, layout
/// validation, refusals — still applies to an old client. A create that
/// cannot be completed takes the record with it: a workspace whose first
/// session never started is not a workspace, which is what the old daemon
/// meant too.
#[allow(clippy::too_many_arguments, reason = "the gen-2 request's own fields")]
async fn combined_create(
    sessions: &Arc<SessionTable>,
    subscriber: SubscriberId,
    root: String,
    name: Option<String>,
    project_id: Option<String>,
    project_identity: Option<String>,
    env: Vec<(String, String)>,
    cols: Option<u16>,
    rows: Option<u16>,
    persisted: bool,
    request_id: Option<u64>,
) -> Frame {
    let refusal = |error: TableError, workspace_id: Option<String>| Frame::Error {
        session_id: None,
        workspace_id,
        code: error.code.to_owned(),
        message: error.message,
        request_id,
    };
    let workspace = match sessions.create_workspace(WorkspaceRequest {
        root: root.clone(),
        name,
        project_id,
        project_identity,
        id: None,
    }) {
        Ok(workspace) => workspace,
        // No `workspace_id`: a creation that failed has no id to name.
        Err(err) => return refusal(err, None),
    };
    let session = match sessions
        .create(combined_create_request(
            workspace.id.clone(),
            root,
            workspace.name.clone(),
            env,
            cols,
            rows,
        ))
        .await
    {
        Ok(session) => session,
        Err(err) => {
            let _ = sessions.kill_workspace(&workspace.id).await;
            return refusal(err, Some(workspace.id));
        }
    };
    if let Err(err) = sessions.update_layout(
        &workspace.id,
        LayoutDoc::single_terminal(session.id.clone()),
        workspace.layout_rev + 1,
        Some(subscriber),
    ) {
        let _ = sessions.kill_workspace(&workspace.id).await;
        return refusal(err, Some(workspace.id));
    }
    match sessions.open_workspace(&workspace.id) {
        Ok((workspace, sessions)) => Frame::Workspace {
            workspace,
            sessions,
            persisted,
            request_id,
        },
        Err(err) => refusal(err, Some(workspace.id)),
    }
}

/// One request in, at most one reply out.
///
/// `None` means the frame is answered by something other than a return value,
/// or not answered at all:
///
/// - `Attach` — its reply is the [`Frame::Replay`] that
///   [`SessionTable::attach`] queues on `outbound`, ahead of the live output,
///   which is what makes replay-then-live ordering a property of the lock
///   rather than of this function.
/// - `Detach` — the protocol has no ack for it, and detaching something that
///   was never attached is a no-op rather than an error.
/// - `Write` / `Resize` — fire-and-forget; only failure is reported.
/// - `Subscribe` — its answer is the [`Frame::Status`] snapshot that
///   [`SessionTable::subscribe`] queues on `outbound`, one per session. A
///   daemon with no sessions therefore answers with nothing at all, which is
///   the honest snapshot of an empty table; there is no `SubscribeAck` to
///   wait for.
/// `generation` is this connection's alone (see [`serve_connection`]) and picks
/// the dialect: at 2 `create_workspace` is the combined create, `create_session`
/// auto-creates its workspace, `resize` is last-request-wins, `attach` has no
/// view, and `focus_session` does not exist. Everything else — and every
/// hardening since the cut — is the same at both.
async fn handle_frame(
    frame: Frame,
    sessions: &Arc<SessionTable>,
    subscriber: SubscriberId,
    outbound: &Outbound,
    generation: u32,
) -> Option<Frame> {
    /// A refusal from the table, as the frame that carries it back.
    ///
    /// The code is chosen where the reason was known — inside
    /// [`crate::sessions`] — and only copied here; the subject is added at this
    /// end, because the table answers about ids and the wire wants to know
    /// whether the id was a session's or a workspace's.
    fn refusal(
        error: TableError,
        session_id: Option<SessionId>,
        workspace_id: Option<String>,
        request_id: Option<u64>,
    ) -> Frame {
        Frame::Error {
            session_id,
            workspace_id,
            code: error.code.to_owned(),
            message: error.message,
            request_id,
        }
    }

    let request_id = frame.request_id();
    // §8.5: a degraded daemon serves normally and says so on every ack it
    // sends, because the durability §8.1 promises is the one thing it cannot
    // provide. Read once, and it cannot change: nothing clears degraded mode.
    let persisted = sessions.persisted();
    match frame {
        Frame::CreateSession {
            workspace_id,
            cwd,
            project_id,
            project_identity,
            command,
            env,
            cols,
            rows,
            agent_kind,
            instance_label,
            scrollback_bytes,
            request_id,
        } => {
            let named = workspace_id.clone();
            // Generation 2 never refused a session for want of a workspace: it
            // made one, under the id the client named or a fresh one when it
            // named none. Both forms, because both were reachable.
            //
            // Record, session and one-leaf layout are **one semantic outcome**:
            // what cannot be completed is compensated and refused, never acked
            // as a bare `Created` over a workspace that has no layout — see
            // `compensate` below.
            let auto_created = if generation == LEGACY_GENERATION
                && sessions.open_workspace(&workspace_id).is_err()
            {
                match sessions.ensure_workspace(WorkspaceRequest {
                    root: cwd.clone(),
                    name: (!instance_label.is_empty()).then(|| instance_label.clone()),
                    project_id,
                    project_identity,
                    id: (!workspace_id.is_empty()).then(|| workspace_id.clone()),
                }) {
                    Ok((workspace, created_here)) => Some((workspace.id, created_here)),
                    Err(err) => return Some(refusal(err, None, Some(named), request_id)),
                }
            } else {
                None
            };
            let workspace_id = auto_created
                .as_ref()
                .map_or(workspace_id, |(id, _)| id.clone());
            /// Undo what this request made and nothing else: the workspace only
            /// if this request created it *and* it is still empty — a
            /// concurrent create may have adopted it, and its sessions are not
            /// this refusal's to take. A cleanup that fails is logged; the
            /// answer stays the original refusal, which is what went wrong.
            async fn compensate(
                sessions: &Arc<SessionTable>,
                created: Option<&(String, bool)>,
                session: Option<&SessionId>,
            ) {
                if let Some(session) = session
                    && let Err(err) = sessions.kill(session).await
                {
                    log::warn!("could not take back session {session}: {}", err.message);
                }
                if let Some((workspace_id, true)) = created
                    && let Err(err) = sessions.remove_empty_workspace(workspace_id).await
                {
                    log::warn!(
                        "could not take back auto-created workspace {workspace_id}: {}",
                        err.message
                    );
                }
            }
            let request = CreateRequest {
                workspace_id: workspace_id.clone(),
                cwd,
                command,
                env,
                cols,
                rows,
                agent_kind,
                instance_label,
                scrollback_bytes,
            };
            let session = match sessions.create(request).await {
                Ok(session) => session,
                // A create is refused over its workspace as often as not, so
                // the error names it.
                Err(err) => {
                    compensate(sessions, auto_created.as_ref(), None).await;
                    return Some(refusal(err, None, Some(named), request_id));
                }
            };
            if auto_created.is_some()
                && let Err(err) = sessions.install_legacy_layout(&workspace_id, &session.id)
            {
                compensate(sessions, auto_created.as_ref(), Some(&session.id)).await;
                return Some(refusal(err, None, Some(named), request_id));
            }
            Some(Frame::Created {
                session,
                persisted,
                request_id,
            })
        }
        Frame::ListSessions { request_id } => Some(Frame::SessionList {
            sessions: sessions.list(),
            request_id,
        }),
        Frame::CreateWorkspace {
            root,
            name,
            project_id,
            project_identity,
            env,
            cols,
            rows,
            request_id,
        } => {
            if generation == LEGACY_GENERATION {
                return Some(
                    combined_create(
                        sessions,
                        subscriber,
                        root,
                        name,
                        project_id,
                        project_identity,
                        env,
                        cols,
                        rows,
                        persisted,
                        request_id,
                    )
                    .await,
                );
            }
            // Generation 3 creates the record alone and the legacy request
            // fields mean nothing here — a gen-3 client never sends them.
            let request = WorkspaceRequest {
                root,
                name,
                project_id,
                project_identity,
                id: None,
            };
            Some(match sessions.create_workspace(request) {
                Ok(workspace) => Frame::Workspace {
                    workspace,
                    sessions: Vec::new(),
                    persisted,
                    request_id,
                },
                // No `workspace_id`: a creation that failed has no id to name.
                Err(err) => refusal(err, None, None, request_id),
            })
        }
        Frame::OpenWorkspace {
            workspace_id,
            request_id,
        } => Some(match sessions.open_workspace(&workspace_id) {
            Ok((workspace, sessions)) => Frame::Workspace {
                workspace,
                sessions,
                persisted,
                request_id,
            },
            Err(err) => refusal(err, None, Some(workspace_id), request_id),
        }),
        Frame::ListWorkspaces { request_id } => Some(Frame::WorkspaceList {
            workspaces: sessions.list_workspaces(),
            request_id,
        }),
        Frame::UpdateLayout {
            workspace_id,
            layout,
            rev,
            request_id,
        } => Some(
            match sessions.update_layout(&workspace_id, layout.clone(), rev, Some(subscriber)) {
                Ok(()) => Frame::LayoutChanged {
                    workspace_id,
                    layout,
                    rev,
                    persisted,
                    request_id,
                },
                Err(err) => refusal(err, None, Some(workspace_id), request_id),
            },
        ),
        Frame::RenameWorkspace {
            workspace_id,
            name,
            request_id,
        } => Some(match sessions.rename_workspace(&workspace_id, &name) {
            Ok((workspace, sessions)) => Frame::Workspace {
                workspace,
                sessions,
                persisted,
                request_id,
            },
            Err(err) => refusal(err, None, Some(workspace_id), request_id),
        }),
        Frame::UpdateWorkspaceProject {
            workspace_id,
            project_id,
            project_identity,
            project_root,
            minimum_scope_rev,
            request_id,
        } => Some(
            match sessions.update_workspace_project(
                &workspace_id,
                &project_id,
                &project_identity,
                project_root.as_deref(),
                minimum_scope_rev,
            ) {
                Ok((workspace, sessions)) => Frame::Workspace {
                    workspace,
                    sessions,
                    persisted,
                    request_id,
                },
                Err(err) => refusal(err, None, Some(workspace_id), request_id),
            },
        ),
        Frame::KillWorkspace {
            workspace_id,
            request_id,
        } => Some(match sessions.kill_workspace(&workspace_id).await {
            Ok(()) => Frame::WorkspaceRemoved {
                workspace_id,
                persisted,
                request_id,
            },
            Err(err) => refusal(err, None, Some(workspace_id), request_id),
        }),
        Frame::Kill {
            session_id,
            request_id,
        } => Some(match sessions.kill(&session_id).await {
            Ok(()) => Frame::Removed { session_id },
            Err(err) => refusal(err, Some(session_id), None, request_id),
        }),
        Frame::Attach {
            session_id,
            view_id,
            request_id,
        } => match sessions
            .attach(
                &session_id,
                subscriber,
                outbound,
                // Generation 2 has no views: a `view_id` from a peer at that
                // generation is read as absent however it got here, or a buggy
                // sender would become a focus owner the dialect has no focus
                // for.
                view_id
                    .as_deref()
                    .filter(|_| generation > LEGACY_GENERATION),
            )
            .await
        {
            Ok(()) => None,
            Err(err) => Some(refusal(err, Some(session_id), None, request_id)),
        },
        Frame::Detach { session_id, .. } => {
            sessions.detach(&session_id, subscriber).await;
            None
        }
        // All three are fire-and-forget and carry no `request_id` by construction,
        // so their failures go out as the legal *unsolicited* error frame: no
        // rid to echo, but a session named, which is what makes it routable to
        // diagnostics rather than to a pending request (§2).
        Frame::Write { session_id, bytes } => match sessions.write(&session_id, &bytes).await {
            Ok(()) => None,
            Err(err) => Some(refusal(err, Some(session_id), None, None)),
        },
        Frame::Resize {
            session_id,
            cols,
            rows,
        } => {
            // Generation 2 is last-request-wins straight to the pty; its asks
            // never enter the gen-3 tables (`crate::sessions::Session::sizes`).
            let resized = if generation == LEGACY_GENERATION {
                sessions.resize_legacy(&session_id, cols, rows).await
            } else {
                sessions.resize(&session_id, subscriber, cols, rows).await
            };
            match resized {
                Ok(()) => None,
                Err(err) => Some(refusal(err, Some(session_id), None, None)),
            }
        }
        // A generation-3 operation: generation 2 has no focus notion, so it is
        // refused request-scoped and the connection keeps serving. Sender
        // gating is not a trust boundary — this build can decode the op from
        // any peer.
        Frame::FocusSession { session_id, .. } if generation == LEGACY_GENERATION => {
            Some(Frame::Error {
                session_id: Some(session_id),
                workspace_id: None,
                code: error_code::UNCAPABLE_PEER.to_owned(),
                message: "focus_session is a generation-3 operation and this connection \
                          negotiated generation 2"
                    .to_owned(),
                request_id: None,
            })
        }
        Frame::FocusSession {
            session_id,
            view_id,
            hover,
        } => match sessions.focus(&session_id, &view_id, hover).await {
            Ok(()) => None,
            Err(err) => Some(refusal(err, Some(session_id), None, None)),
        },
        Frame::Subscribe { .. } => {
            sessions.subscribe(subscriber, outbound);
            None
        }
        // Both of these are frames the daemon *understood* and will not act on,
        // which is `invalid_argument` and never `unknown_op` — the op is known,
        // it is the sender's use of it that is wrong. Neither closes the
        // connection.
        Frame::Hello(hello) => Some(Frame::Error {
            session_id: None,
            workspace_id: None,
            code: error_code::INVALID_ARGUMENT.to_owned(),
            message: "hello may only be sent once, as the first frame".to_owned(),
            request_id: hello.request_id,
        }),
        // A client-sent error is **diagnostic, not a request**, and is answered
        // with nothing at all. The attach client sends one when a frame from
        // this daemon fails to decode (`crate::attach`); if that landed in the
        // catch-all below, the `invalid_argument` reply would itself be an error
        // frame, which the client's output pump reads as terminal and exits on
        // — the survivability net undoing itself. An error about an error must
        // not echo, so this arm exists before the catch-all and only logs.
        //
        // *Both* fields go through the bound. `code` is as peer-chosen as
        // `message` — §4 defines it as an open string and nothing checks its
        // length below `MAX_FRAME_BYTES` — so quoting it whole would let one
        // frame write 16 MB into the log, repeatedly, precisely because this
        // arm neither answers nor closes.
        Frame::Error { code, message, .. } => {
            log::warn!(
                "a client reported an error: {}: {}",
                bounded(&code),
                bounded(&message)
            );
            None
        }
        other => Some(Frame::Error {
            session_id: other.session_id().cloned(),
            workspace_id: other.workspace_id().map(str::to_owned),
            code: error_code::INVALID_ARGUMENT.to_owned(),
            // Bounded for the same reason as the handshake's refusal above: a
            // legal 16 MB `output` frame would otherwise become a ~20 MB error
            // message that `write_frame` refuses, killing the writer task.
            message: format!("unexpected frame from a client: {}", bounded_debug(&other)),
            request_id,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control() -> DaemonControl {
        DaemonControl {
            binary_hash: None,
            instance_id: "test-daemon".to_owned(),
            socket_path: PathBuf::new(),
            state_dir: PathBuf::new(),
            fired: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            active_state_requests: Mutex::new(0),
            on_exit: Box::new(|| {}),
        }
    }

    #[test]
    fn state_requests_do_not_hold_the_shutdown_admission_gate() {
        let control = control();
        let first = control.admit_state_request().expect("first request");
        let second = control.admit_state_request().expect("second request");

        assert_eq!(
            *control
                .active_state_requests
                .lock()
                .expect("admission gate"),
            2
        );
        assert!(
            !control.admit_shutdown(false, || {
                panic!("an active request must decline before inspecting the table")
            }),
            "a polite shutdown declines instead of waiting behind active work"
        );

        drop((first, second));
        assert!(control.admit_shutdown(false, || true));
        assert!(control.admit_state_request().is_none());
    }

    /// The generation-2 request's own fields reach the shell it spawns: the
    /// environment verbatim, the size it named, 80x24 when it named none. The
    /// wire test proves the size reaches the pty; this pins the rest of the
    /// request, which travels to the pty in the same struct.
    #[test]
    fn the_combined_create_carries_the_legacy_requests_fields() {
        let env = vec![("ADE_COMBINED".to_owned(), "1".to_owned())];
        let request = combined_create_request(
            "w-1".to_owned(),
            "/home/u/proj".to_owned(),
            "proj".to_owned(),
            env.clone(),
            None,
            None,
        );
        assert_eq!(request.command, "", "an empty command is the login shell");
        assert_eq!(request.env, env);
        assert_eq!(request.cwd, "/home/u/proj");
        assert_eq!(request.instance_label, "proj");
        assert_eq!(request.agent_kind, "shell");
        assert_eq!(
            (request.cols, request.rows),
            (LEGACY_DEFAULT_COLS, LEGACY_DEFAULT_ROWS)
        );

        let sized = combined_create_request(
            "w-1".to_owned(),
            "/home/u/proj".to_owned(),
            "proj".to_owned(),
            env,
            Some(120),
            Some(40),
        );
        assert_eq!((sized.cols, sized.rows), (120, 40));
    }

    #[test]
    fn forced_shutdown_never_waits_for_an_active_request() {
        let control = control();
        let _request = control.admit_state_request().expect("active request");

        assert!(control.admit_shutdown(true, || {
            panic!("forced shutdown must not inspect the table")
        }));
        assert!(control.admit_state_request().is_none());
    }
}
