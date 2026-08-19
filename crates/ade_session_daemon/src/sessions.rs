//! The session table: workspaces, and the sessions that live inside them.
//!
//! Invariants encoded here:
//!
//! - **Every session belongs to a workspace.** There are no free-floating
//!   sessions: [`SessionTable::create`] creates the workspace record if the id
//!   it is handed names none, and a session persisted without one is wrapped in
//!   a workspace of its own on load.
//! - **The daemon owns the layout.** [`WorkspaceInfo::layout`] is stored,
//!   versioned and persisted here; the client renders it and asks for changes
//!   with [`SessionTable::update_layout`], which is guarded by `layout_rev` so
//!   two clients cannot silently overwrite one another.
//! - **A session dies only on [`SessionTable::kill`].** Nothing else removes a
//!   row. When the child process exits the row *stays* and its status becomes
//!   [`SessionStatus::Exited`], which is what makes a crashed agent visible in
//!   the sidebar instead of vanishing from it.
//! - **A kill is not a request.** The row goes at once, and the child's whole
//!   process group is hung up and then, if it is still there, killed; see
//!   [`terminate_group`]. Removing a row the daemon could not reach again is
//!   what leaves an agent holding its locks forever.
//! - **The child process is the agent process.** Commands are run as
//!   `sh -lc 'exec <command>'`, so the pid the daemon waits on is the agent
//!   itself and not an intermediate shell. An *empty* command means "the
//!   user's login shell", resolved here — on the host that runs it, never by
//!   the client — and spawned directly as `<shell> -l`.
//! - **Sessions from a previous daemon are reported, not hidden.** They are
//!   loaded as `Exited` rows labelled [`LOST_SUFFIX`] — a PTY cannot be
//!   resurrected, and pretending they never existed is the dishonest option.
//! - **Detaching never kills.** [`SessionTable::detach`] and
//!   [`SessionTable::detach_all`] only unsubscribe a connection; the pty, the
//!   child and the scrollback ring all outlive it.
//! - **Status is derived here, and pushed.** The daemon owns the pty, so it is
//!   the only process that can see whether an agent is working, waiting or
//!   gone; see [`SessionTable::sweep`]. Clients never poll for it — the ~500ms
//!   internal sweep is an implementation detail that never reaches the wire.
//!
//! **Executor-free on purpose:** the only concurrency here is std threads and
//! channels. [`smol::channel::Sender`] appears as the fan-out endpoint because
//! it is a plain mpmc channel with a non-async `try_send` — no runtime is
//! entered from these threads; the async half lives entirely in
//! [`crate::server`].

use std::collections::{HashMap, VecDeque};
use std::io::Read as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ade_session::proto::{
    Frame, LayoutDoc, SessionId, SessionInfo, SessionStatus, WorkspaceInfo, error_code,
};
use anyhow::{Context as _, Result};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use smol::channel::Sender;

use crate::grid::SessionGrid;
use crate::state::{PersistedSession, StateStore};

/// Appended to the label of a session inherited from a dead daemon.
pub const LOST_SUFFIX: &str = " (lost)";

/// Raw pty bytes kept per session when the client does not ask for a size.
///
/// 2 MiB is a few thousand lines of agent chatter — enough that reattaching to
/// a long-running agent shows its recent work, small enough that a hundred
/// sessions cost less than a browser tab.
pub const DEFAULT_SCROLLBACK_BYTES: usize = 2 * 1024 * 1024;

/// Bytes read from the pty in one go, and therefore the largest raw payload of
/// a single [`Frame::Output`].
const DRAIN_CHUNK_BYTES: usize = 8192;

/// How long a killed session's process group has to leave on its own before
/// [`terminate_group`] stops asking and sends `SIGKILL`.
///
/// A second is a shell's or an agent's whole `SIGHUP` cleanup window, and the
/// user never waits on it: `Kill` answers `Removed` immediately and the
/// escalation runs on a thread of its own.
#[cfg(unix)]
const KILL_GRACE: Duration = Duration::from_secs(1);

/// The byte an agent sends when it wants a human: `BEL`, 0x07.
const BELL: u8 = 0x07;

/// `ESC`, 0x1b — the byte that opens every control string [`BellScan`] tracks.
const ESC: u8 = 0x1b;

/// `CAN`, 0x18 and `SUB`, 0x1a: the two bytes that abort a control string
/// mid-flight.
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;

/// Process names treated as "sitting at a prompt" by the idle rule.
///
/// Gated the same way its only caller is: [`foreground_is_shell`] can answer
/// only where `/proc` exists, so on a Windows build this and [`is_shell_name`]
/// would be dead code — and the rule is still worth testing everywhere, hence
/// `test` in the gate.
#[cfg(any(target_os = "linux", test))]
const SHELL_NAMES: &[&str] = &["sh", "bash", "zsh", "fish", "dash", "ksh"];

/// The two numbers status derivation is tuned by.
///
/// Both are configurable so that tests can tune them down to milliseconds;
/// nothing but a test should move them. The defaults are ADE's spec values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatusConfig {
    /// Silence longer than this, from an agent that is still running, means it
    /// is waiting on the human.
    pub needs_input_after: Duration,
    /// How often [`SessionTable::sweep`] re-derives every session's status.
    /// Purely internal: no client ever polls, and this interval is invisible
    /// on the wire.
    pub sweep_interval: Duration,
}

impl Default for StatusConfig {
    fn default() -> Self {
        Self {
            needs_input_after: Duration::from_secs(5),
            sweep_interval: Duration::from_millis(500),
        }
    }
}

/// Enough of a VT parser to tell an attention bell from a string terminator.
///
/// A raw [`BELL`] byte means two different things depending on where it lands.
/// In ground state it is the agent asking for a human. Inside a *control
/// string* — OSC (`ESC ]`), DCS (`ESC P`), APC (`ESC _`), PM (`ESC ^`) or SOS
/// (`ESC X`) — it is that string's terminator, the other spelling of ST
/// (`ESC \`). Debian's default bash `PS1` sets the window title on every
/// prompt (`ESC ] 0 ; user@host: ~/dir BEL`), so a scan that counts raw bytes
/// pins an idle shell at [`SessionStatus::NeedsInput`] forever.
///
/// Stored per session rather than per chunk because the pty read loop hands
/// over arbitrary slices: a title sequence is routinely split across two
/// [`DRAIN_CHUNK_BYTES`] reads, and a scanner that reset between them would
/// see the BEL with no `ESC ]` in front of it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum BellScan {
    /// Ordinary output. A BEL here is a bell.
    #[default]
    Ground,
    /// `ESC` seen; the next byte decides whether a control string opens.
    Escape,
    /// Inside a control string. A BEL here ends it.
    InString,
    /// `ESC` seen inside a control string: `\` completes ST and ends it,
    /// anything else is part of the string.
    StringEscape,
}

impl BellScan {
    /// Feed one chunk; `true` if it contained a bell in ground state.
    fn feed(&mut self, chunk: &[u8]) -> bool {
        let mut rang = false;
        for &byte in chunk {
            *self = match (*self, byte) {
                (Self::Ground, BELL) => {
                    rang = true;
                    Self::Ground
                }
                (Self::Ground, ESC) | (Self::Escape, ESC) => Self::Escape,
                (Self::Escape, b']' | b'P' | b'_' | b'^' | b'X') => Self::InString,
                (Self::InString, ESC) | (Self::StringEscape, ESC) => Self::StringEscape,
                (Self::StringEscape, b'\\') => Self::Ground,
                // CAN and SUB abort a string. Handled so that a stream which
                // never terminates one — binary noise, a truncated sequence —
                // cannot swallow every later bell.
                (Self::InString | Self::StringEscape, CAN | SUB) => Self::Ground,
                (Self::InString, BELL) => Self::Ground,
                (Self::InString | Self::StringEscape, _) => Self::InString,
                (Self::Ground | Self::Escape, _) => Self::Ground,
            };
        }
        rang
    }
}

/// The cheap facts status derivation is computed from.
///
/// Written by the drain thread (output, bell) and the reaper thread (death),
/// read by the sweeper. Deliberately *facts only*: the threads that observe
/// the pty never decide what a status is, so the rules live in exactly one
/// place ([`derive_status`]).
struct Activity {
    state: Mutex<ActivityState>,
}

struct ActivityState {
    /// Seeded with the creation instant, so a session that has never printed
    /// anything still has a meaningful "silent since".
    last_output: Instant,
    /// Set by a chunk containing a [`BELL`] in ground state and cleared by the
    /// next chunk that does not — "sticky until the next output after the
    /// bell". A BEL that merely terminates a control string is not one: see
    /// [`BellScan`].
    bell: bool,
    /// Where the bell scan stands between chunks, since a control string can
    /// straddle the boundary.
    bell_scan: BellScan,
    /// The child has been waited on. The row stays; only its status changes.
    dead: bool,
}

impl Activity {
    fn new() -> Self {
        Self {
            state: Mutex::new(ActivityState {
                last_output: Instant::now(),
                bell: false,
                bell_scan: BellScan::default(),
                dead: false,
            }),
        }
    }

    fn record_output(&self, chunk: &[u8]) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.last_output = Instant::now();
        state.bell = state.bell_scan.feed(chunk);
    }

    fn mark_dead(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).dead = true;
    }

    /// The child has been waited on, so its pid may already belong to someone
    /// else — the kill paths' reason to stand down.
    fn is_dead(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).dead
    }

    /// `(last output, bell pending, child dead)`.
    fn snapshot(&self) -> (Instant, bool, bool) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (state.last_output, state.bell, state.dead)
    }
}

/// The connections that asked for the event stream.
///
/// Same shape as [`OutputHub`]'s subscriber list and for the same reason: a
/// send can only fail on a closed channel, which means that connection is
/// gone, so it is dropped here. Event subscription and output attachment are
/// independent — a connection may be either, both, or neither.
#[derive(Default)]
struct EventHub {
    subscribers: Vec<(SubscriberId, Sender<Frame>)>,
}

impl EventHub {
    fn publish(&mut self, frame: &Frame) {
        self.subscribers
            .retain(|(_, sender)| sender.try_send(frame.clone()).is_ok());
    }

    /// Subscribing twice is idempotent: the second call replaces the first
    /// registration rather than doubling the fan-out.
    fn subscribe(&mut self, subscriber: SubscriberId, sender: &Sender<Frame>) {
        self.subscribers.retain(|(id, _)| *id != subscriber);
        self.subscribers.push((subscriber, sender.clone()));
    }

    /// Publish to everyone but `except` — the client whose own request caused
    /// the event and which is answered directly instead.
    fn publish_except(&mut self, except: SubscriberId, frame: &Frame) {
        self.subscribers
            .retain(|(id, sender)| *id == except || sender.try_send(frame.clone()).is_ok());
    }

    fn unsubscribe(&mut self, subscriber: SubscriberId) {
        self.subscribers.retain(|(id, _)| *id != subscriber);
    }
}

/// Identifies one attached connection, so that [`SessionTable::detach`] can
/// unsubscribe exactly that connection and no other.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SubscriberId(u64);

impl SubscriberId {
    /// A fresh id. Process-wide and monotonic; ids are never reused.
    pub fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

impl std::fmt::Display for SubscriberId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A refusal from one of this table's request entry points, with the wire code
/// already decided.
///
/// The code has to be chosen **where the reason is known** — only
/// [`SessionTable::update_layout`] can tell a stale revision from a layout
/// naming a session that does not exist, and by the time an `anyhow` chain
/// reaches [`crate::server`] both are just prose. So the public entry points
/// return this and [`crate::server`] copies `code` onto the
/// [`Frame::Error`](ade_session::Frame::Error) it sends
/// (`docs/ade/protocol-compatibility.md` §2.1).
///
/// Internal helpers keep using `anyhow`: [`From<anyhow::Error>`] wraps whatever
/// they failed at as [`error_code::INTERNAL`], which is the honest answer for a
/// spawn, an ioctl or a poisoned lock — the client did nothing wrong.
#[derive(Debug)]
pub struct TableError {
    /// One of [`error_code`]'s constants. `&'static str` and not an enum for
    /// the same reason the wire field is an open string: new codes are added
    /// without breaking a reader.
    pub code: &'static str,
    pub message: String,
}

impl TableError {
    /// No session or workspace by that id.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            code: error_code::NOT_FOUND,
            message: message.into(),
        }
    }

    /// The request named something the daemon holds, and asked for something
    /// unusable with it: an empty name, a layout naming a session that is gone,
    /// a write to a pty that no longer exists.
    pub fn invalid_argument(message: impl Into<String>) -> Self {
        Self {
            code: error_code::INVALID_ARGUMENT,
            message: message.into(),
        }
    }

    /// A revision-guarded write that lost the race.
    pub fn stale_rev(message: impl Into<String>) -> Self {
        Self {
            code: error_code::STALE_REV,
            message: message.into(),
        }
    }

    /// The daemon failed at something of its own.
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: error_code::INTERNAL,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for TableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for TableError {}

impl From<anyhow::Error> for TableError {
    /// `{err:#}` and not `{err}`: the whole context chain is the message the
    /// client used to get, and the tests that assert on it are asserting on the
    /// chain's outermost context.
    fn from(error: anyhow::Error) -> Self {
        Self::internal(format!("{error:#}"))
    }
}

/// What every request entry point on [`SessionTable`] answers with.
pub type TableResult<T> = std::result::Result<T, TableError>;

/// Everything [`SessionTable::create`] needs, mirroring
/// [`Frame::CreateSession`](ade_session::Frame::CreateSession).
#[derive(Clone, Debug)]
pub struct CreateRequest {
    pub workspace_id: String,
    pub cwd: String,
    pub command: String,
    pub env: Vec<(String, String)>,
    pub cols: u16,
    pub rows: u16,
    pub agent_kind: String,
    pub instance_label: String,
    /// `None` means [`DEFAULT_SCROLLBACK_BYTES`].
    pub scrollback_bytes: Option<u64>,
}

/// Everything [`SessionTable::create_workspace`] needs, mirroring
/// [`Frame::CreateWorkspace`](ade_session::Frame::CreateWorkspace).
#[derive(Clone, Debug)]
pub struct WorkspaceRequest {
    /// Project root, and the cwd of the workspace's first session.
    pub root: String,
    /// `None` means the last component of `root`.
    pub name: Option<String>,
    pub env: Vec<(String, String)>,
    pub cols: u16,
    pub rows: u16,
}

/// The pty size a client that named none gets.
pub const DEFAULT_COLS: u16 = 80;
pub const DEFAULT_ROWS: u16 = 24;

/// What a workspace is called when the client did not say: the last component
/// of the project root, or the root itself if it has none.
fn default_workspace_name(root: &str) -> String {
    let trimmed = root.trim_end_matches(['/', '\\']);
    trimmed
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(root)
        .to_owned()
}

/// A bounded window of the raw bytes a pty has produced.
///
/// Raw, not screen state: the daemon never interprets terminal output, so a
/// replay is just "the last N bytes" handed back to the client's emulator.
struct Ring {
    bytes: VecDeque<u8>,
    capacity: usize,
    /// Set once the ring has dropped anything, and never cleared — a replay
    /// that starts mid-escape-sequence has to say so.
    truncated: bool,
}

impl Ring {
    fn new(capacity: usize) -> Self {
        Self {
            bytes: VecDeque::new(),
            capacity: capacity.max(1),
            truncated: false,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend(chunk.iter().copied());
        if self.bytes.len() > self.capacity {
            let overflow = self.bytes.len() - self.capacity;
            self.bytes.drain(..overflow);
            self.truncated = true;
        }
    }

    /// The whole window, oldest byte first.
    ///
    /// No longer what an attach replays — that is a repaint synthesized from
    /// the screen (see [`OutputHub::attach`]) — but the ring is still the
    /// honest record of what a session actually printed, which is what a
    /// history or log view would want and what `truncated` is derived from.
    #[allow(dead_code, reason = "the record the ring exists to be; see above")]
    fn snapshot(&self) -> Vec<u8> {
        let (head, tail) = self.bytes.as_slices();
        let mut out = Vec::with_capacity(self.bytes.len());
        out.extend_from_slice(head);
        out.extend_from_slice(tail);
        out
    }
}

/// One session's scrollback and screen, plus the connections streaming from it.
///
/// Ring, grid and subscriber list share a single mutex deliberately:
/// [`Self::attach`] synthesizes the repaint, queues the [`Frame::Replay`] and
/// registers the subscriber in one critical section, while [`Self::publish`]
/// appends, advances the screen and fans out in another. So no byte can slip
/// between the repaint and the live stream, and none can be delivered twice —
/// the property that used to hold for the ring snapshot alone now has to hold
/// for the screen the snapshot was taken of.
struct OutputHub {
    ring: Ring,
    /// The screen these bytes have painted. `None` for a lost session, which
    /// has no pty and never will: painting a blank screen for it would assert
    /// something untrue, so it replays empty instead.
    grid: Option<SessionGrid>,
    ending: Option<Frame>,
    subscribers: Vec<(SubscriberId, Sender<Frame>)>,
}

impl OutputHub {
    fn new(capacity: usize, grid: Option<SessionGrid>, ending: Option<Frame>) -> Self {
        Self {
            ring: Ring::new(capacity),
            grid,
            ending,
            subscribers: Vec::new(),
        }
    }

    /// Resize the screen. Separate from the pty resize, and done first: see
    /// [`SessionTable::resize`].
    fn resize_grid(&mut self, cols: u16, rows: u16) {
        if let Some(grid) = self.grid.as_mut() {
            grid.resize(cols, rows);
        }
    }

    /// Record `chunk` and hand it to every attached connection.
    ///
    /// Sending can only fail on a closed channel — the outbound queues are
    /// unbounded, so a slow client never stalls the pty — and a closed channel
    /// means that connection is gone, so it is dropped here.
    fn publish(&mut self, session_id: &SessionId, chunk: &[u8]) {
        self.ring.push(chunk);
        // The daemon's own copy of what the client is about to draw. Only this
        // copy is interpreted; the bytes forwarded below are untouched.
        if let Some(grid) = self.grid.as_mut() {
            grid.feed(chunk);
        }
        self.subscribers.retain(|(_, sender)| {
            sender
                .try_send(Frame::Output {
                    session_id: session_id.clone(),
                    bytes: chunk.to_vec(),
                })
                .is_ok()
        });
    }

    /// Queue the replay and subscribe. Re-attaching replaces the previous
    /// subscription rather than doubling it.
    ///
    /// The replay is a **repaint synthesized from the screen**, not the ring's
    /// bytes: raw scrollback only renders correctly at the width it was
    /// produced at, and a client that re-mounts a terminal view is exactly the
    /// case where that width is wrong. A lost session has no screen and
    /// replays empty. The ring stays as the honest record of what the session
    /// printed, and still supplies half of `truncated`.
    fn attach(&mut self, session_id: &SessionId, subscriber: SubscriberId, sender: &Sender<Frame>) {
        self.subscribers.retain(|(id, _)| *id != subscriber);
        let (bytes, scrolled) = match self.grid.as_ref() {
            Some(grid) => (grid.repaint(), grid.scrolled()),
            None => (Vec::new(), false),
        };
        let replay = Frame::Replay {
            session_id: session_id.clone(),
            bytes,
            // Either the ring dropped bytes or the screen scrolled: both mean
            // "this is not everything the session printed".
            truncated: self.ring.truncated || scrolled,
        };
        if sender.try_send(replay).is_err() {
            return;
        }
        if let Some(ending) = self.ending.as_ref() {
            if sender.try_send(ending.clone()).is_err() {
                return;
            }
        } else {
            self.subscribers.push((subscriber, sender.clone()));
        }
    }

    fn detach(&mut self, subscriber: SubscriberId) {
        self.subscribers.retain(|(id, _)| *id != subscriber);
    }

    /// Hand a session-level event to every *attached* connection.
    ///
    /// Attaching is a byte stream, not a subscription to the event hub, so a
    /// client that only attached — the `ade-daemon attach` terminal client is
    /// exactly that — would otherwise never learn that its session ended: the
    /// pty simply stops producing and the connection sits silent forever.
    /// [`Frame::Exited`] and [`Frame::Removed`] are the two frames that end the
    /// stream, so they go to the stream's readers as well as to the event hub.
    fn publish_event(&mut self, frame: &Frame) {
        self.ending = Some(frame.clone());
        self.subscribers
            .retain(|(_, sender)| sender.try_send(frame.clone()).is_ok());
    }
}

/// The live half of a session. Absent for a `lost` row.
///
/// Holding `master` is what keeps the pty open; dropping it sends EOF to the
/// drain thread. `writer` is taken at spawn time because `take_writer` may only
/// be called once, and sits behind its own mutex so that a `Write` never holds
/// the session-table lock across a pty write.
struct Live {
    master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn std::io::Write + Send>>>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    /// The child's pid, which on unix is also its process-group id — see
    /// [`signal_group`]. `None` only if the platform would not report one.
    pid: Option<u32>,
}

struct Session {
    info: SessionInfo,
    /// Unix seconds at which `info.status` last *changed*, reported as
    /// [`Frame::Status::since`].
    since: u64,
    /// Inherited from a previous daemon: no pty, never re-persisted.
    lost: bool,
    live: Option<Live>,
    /// Shared with the drain thread, which is the only producer.
    hub: Arc<Mutex<OutputHub>>,
    /// Shared with the drain and reaper threads, which only write facts to it.
    activity: Arc<Activity>,
    /// Last size applied by [`SessionTable::resize`]; reporting it to clients
    /// is a later step.
    #[allow(dead_code, reason = "reported to clients in a later step")]
    cols: u16,
    #[allow(dead_code, reason = "reported to clients in a later step")]
    rows: u16,
}

fn workspace_sessions_in(
    sessions: &HashMap<SessionId, Session>,
    workspace_id: &str,
) -> Vec<SessionInfo> {
    let mut infos: Vec<SessionInfo> = sessions
        .values()
        .filter(|session| session.info.workspace_id == workspace_id)
        .map(|session| session.info.clone())
        .collect();
    infos.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    infos
}

/// Every workspace this daemon owns, every session inside them, and the
/// sessions it knows it lost.
///
/// **Lock order is `sessions` then `workspaces` then `events`, never the other
/// way round.** Every status change publishes while still holding the session
/// lock, so a connection subscribing concurrently either sees the old status in
/// its snapshot and then the change, or the new status and no change — it can
/// never miss a transition.
pub struct SessionTable {
    sessions: Mutex<HashMap<SessionId, Session>>,
    workspaces: Mutex<HashMap<String, WorkspaceInfo>>,
    events: Mutex<EventHub>,
    status: StatusConfig,
    state: StateStore,
    /// Serializes [`Self::persist`] end to end. Outside the three locks above:
    /// it is taken *before* `sessions` and never while any of them is held.
    ///
    /// Snapshotting under the table locks is not enough on its own — two
    /// persists that only serialize their snapshots can still reach
    /// [`StateStore::save`] in the opposite order, and the older set is then
    /// the one left on disk: a killed session back as a lost row, with its tab.
    persisting: Mutex<()>,
    /// Open client connections, counted by the server. Part of the idle-exit
    /// decision: a daemon someone is talking to is not idle, whatever its
    /// table holds.
    connections: std::sync::atomic::AtomicUsize,
    /// The subset of `connections` that ever touched a session or workspace.
    ///
    /// Part of the shutdown decision, where the plain count is too blunt: a
    /// client mid-create must never have the daemon vanish under it, but a
    /// connection that only ever subscribed — a status stream, or one leaked
    /// by a dead client — loses nothing in a swap and reconnects to the
    /// replacement on its own.
    active_connections: std::sync::atomic::AtomicUsize,
}

impl SessionTable {
    /// Build the table from what a previous daemon left in `sessions.json`:
    /// its workspaces restored, its sessions adopted as lost rows.
    ///
    /// Three things happen here, in order:
    ///
    /// 1. **Workspaces are restored** — name, root, layout and `layout_rev` all
    ///    survive, because none of them needs a live process.
    /// 2. **Flat sessions are migrated.** A session persisted with no
    ///    workspace, or with one whose record is gone, is wrapped in a
    ///    single-terminal workspace of its own. Automatic and lossless: nothing
    ///    the old file recorded is dropped.
    /// 3. **Layouts are pruned** of terminal tabs naming sessions that are not
    ///    in the table or belong to another workspace, which keeps the same
    ///    ownership invariant enforced on writes. Splits that lose a child
    ///    collapse; editor tabs are untouched.
    ///
    /// The state file is rewritten immediately, dropping the lost rows: they
    /// have now been reported once, and re-persisting them would make every
    /// future restart replay the same tombstones forever. The workspaces stay.
    pub fn load(state: StateStore, status: StatusConfig) -> Arc<Self> {
        let mut sessions = HashMap::new();
        let previous = state.load();
        if !previous.sessions.is_empty() {
            log::warn!(
                "{} session(s) from a previous daemon cannot be resurrected; reporting them as lost",
                previous.sessions.len()
            );
        }
        let had_previous = !previous.sessions.is_empty();
        let mut dirty = had_previous;
        let mut workspaces: HashMap<String, WorkspaceInfo> = previous
            .workspaces
            .into_iter()
            .map(|workspace| (workspace.id.clone(), workspace))
            .collect();
        for persisted in previous.sessions {
            let id = persisted.id.clone();
            let workspace_id = if workspaces.contains_key(&persisted.workspace_id) {
                persisted.workspace_id.clone()
            } else {
                // Migration: a session with no workspace — or one whose record
                // did not survive — becomes a workspace of its own. Its own id
                // is kept when it had one, so anything already referring to it
                // keeps working.
                let workspace_id = if persisted.workspace_id.is_empty() {
                    new_id()
                } else {
                    persisted.workspace_id.clone()
                };
                let name = if persisted.instance_label.is_empty() {
                    default_workspace_name(&persisted.cwd)
                } else {
                    persisted.instance_label.clone()
                };
                workspaces.insert(
                    workspace_id.clone(),
                    WorkspaceInfo {
                        id: workspace_id.clone(),
                        name,
                        project_root: persisted.cwd.clone(),
                        created_at: persisted.created_at,
                        layout_rev: 1,
                        layout: LayoutDoc::single_terminal(id.clone()),
                    },
                );
                dirty = true;
                workspace_id
            };
            sessions.insert(
                id.clone(),
                Session {
                    info: SessionInfo {
                        id: id.clone(),
                        workspace_id,
                        agent_kind: persisted.agent_kind,
                        instance_label: format!("{}{LOST_SUFFIX}", persisted.instance_label),
                        cwd: persisted.cwd,
                        created_at: persisted.created_at,
                        status: SessionStatus::Exited,
                    },
                    since: now_unix(),
                    lost: true,
                    live: None,
                    // A lost session has no pty and will never produce a byte;
                    // it still gets a hub so that attaching to it replays
                    // empty instead of erroring — and no screen, because it has
                    // nothing to show and a blank one would claim otherwise.
                    hub: Arc::new(Mutex::new(OutputHub::new(
                        DEFAULT_SCROLLBACK_BYTES,
                        None,
                        Some(Frame::Exited {
                            session_id: id.clone(),
                            exit_code: None,
                        }),
                    ))),
                    activity: Arc::new(Activity::new()),
                    cols: 0,
                    rows: 0,
                },
            );
        }
        for workspace in workspaces.values_mut() {
            let workspace_id = workspace.id.clone();
            if workspace.layout.retain_sessions(|id| {
                sessions
                    .get(id)
                    .is_some_and(|session| session.info.workspace_id == workspace_id)
            }) {
                // Said out loud: a ledger written before `kill` learned to
                // scrub self-heals here, and a workspace quietly losing a tab
                // on startup is exactly the kind of thing that should not be
                // quiet. The rev is deliberately *not* bumped — no client has
                // seen this document yet.
                log::info!(
                    "pruned terminal tabs naming unknown or foreign sessions from workspace {} layout",
                    workspace.id
                );
                dirty = true;
            }
        }
        let table = Arc::new(Self {
            sessions: Mutex::new(sessions),
            workspaces: Mutex::new(workspaces),
            events: Mutex::new(EventHub::default()),
            status,
            state,
            persisting: Mutex::new(()),
            connections: std::sync::atomic::AtomicUsize::new(0),
            active_connections: std::sync::atomic::AtomicUsize::new(0),
        });
        if dirty && let Err(err) = table.persist() {
            log::warn!("could not rewrite session state: {err:#}");
        }
        spawn_sweeper(Arc::downgrade(&table), status.sweep_interval);
        table
    }

    /// A client connection opened. Balanced by [`Self::connection_closed`].
    pub fn connection_opened(&self) {
        self.connections
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn connection_closed(&self) {
        self.connections
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn connection_count(&self) -> usize {
        self.connections.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// A connection sent its first session- or workspace-touching frame.
    /// Balanced by [`Self::active_connection_closed`]; at most once per
    /// connection.
    pub fn connection_went_active(&self) {
        self.active_connections
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn active_connection_closed(&self) {
        self.active_connections
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn active_connection_count(&self) -> usize {
        self.active_connections
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Does this daemon hold nothing an exit would lose?
    ///
    /// True only when every row is a **lost** one — inherited from a previous
    /// daemon, no pty, no scrollback in this process, already reported once.
    /// A live session obviously blocks; so does an exited-but-not-lost row,
    /// because its screen and scrollback exist only in this process and are
    /// the evidence a crashed agent left behind. Workspaces never block: they
    /// are persisted and survive a restart whole.
    ///
    /// This is the whole data half of the shutdown/idle-exit decision; the
    /// server adds "and nobody is connected" where it matters.
    pub fn only_tombstones(&self) -> bool {
        self.sessions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
            .all(|session| session.lost)
    }

    /// Does this daemon hold nothing an **upgrade** may not sacrifice?
    ///
    /// Looser than [`Self::only_tombstones`], and deliberately so: a lost row
    /// is expendable because it is already lost, and an `Idle` one is
    /// expendable because [`derive_status`] only says `Idle` about a session
    /// whose child is alive, has not rung a bell, has been quiet past
    /// [`StatusConfig::needs_input_after`], and whose pty's foreground process
    /// is a shell. That is an empty terminal sitting at a prompt — nothing is
    /// running in it and nothing is waiting on the human, so ending it costs
    /// the user a shell they can have back in a keystroke. Refusing to upgrade
    /// over one is how a single forgotten terminal pins a stale daemon binary
    /// on a host indefinitely.
    ///
    /// Everything else still blocks. `Working` and `NeedsInput` are sessions
    /// with something in them. An **exited-but-not-lost** row blocks too, for
    /// the same reason it blocks a shutdown: its screen and scrollback exist
    /// only in this process, and they are the evidence of how the agent died.
    ///
    /// Note that `derive_status` counts silence from *creation*, so a session
    /// born a moment ago reads `Working` until the threshold passes. That is a
    /// guard worth having here — a client that just created a session and has
    /// not written to it yet must not have the daemon restart under it.
    ///
    /// The empty table is expendable, which is what `all` on nothing means.
    pub fn expendable(&self) -> bool {
        let now = Instant::now();
        self.sessions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
            .all(|session| {
                session.lost || derive_status(session, self.status, now) == SessionStatus::Idle
            })
    }

    /// Spawn `request.command` on a fresh PTY and record the session.
    ///
    /// The session always ends up in a workspace: an id naming no record
    /// creates one rooted at the session's cwd, and an empty id gets a fresh
    /// one. That is what "no free-floating sessions" means in practice — the
    /// daemon never refuses a session for want of a workspace, it makes the
    /// workspace.
    ///
    /// Every failure in here is the daemon's own — an `openpty` that failed, a
    /// command that would not spawn — so they all arrive as
    /// [`error_code::INTERNAL`] through [`TableError`]'s `anyhow` conversion.
    pub fn create(self: &Arc<Self>, request: CreateRequest) -> TableResult<SessionInfo> {
        let workspace_id = if request.workspace_id.is_empty() {
            new_id()
        } else {
            request.workspace_id.clone()
        };
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: request.rows.max(1),
                cols: request.cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("opening pty")?;

        // An empty command means "the user's login shell" — resolved *here*,
        // on the host that will run it, because the client may be a different
        // OS entirely. Spawned directly rather than through `sh -lc`, so the
        // waited-on child is the shell itself and `foreground_is_shell` sees
        // it. For anything else, `sh -lc 'exec ...'` makes the waited-on child
        // the agent rather than an intermediate shell.
        let mut launched = request.command.clone();
        let mut command = if request.command.is_empty() {
            let login_shell = resolve_login_shell();
            launched = format!("{login_shell} -l");
            let mut command = CommandBuilder::new(login_shell);
            command.arg("-l");
            command
        } else {
            let mut command = CommandBuilder::new(shell());
            command.arg("-lc");
            command.arg(format!("exec {}", request.command));
            command
        };
        command.cwd(&request.cwd);
        for (key, value) in terminal_env(&request.env) {
            command.env(key, value);
        }

        let child = pty
            .slave
            .spawn_command(command)
            .with_context(|| format!("spawning {launched:?}"))?;
        // The slave fd must not outlive the spawn, or the master never sees EOF.
        drop(pty.slave);

        let killer = child.clone_killer();
        let pid = child.process_id();
        let handles = pty
            .master
            .take_writer()
            .context("taking pty writer")
            .and_then(|writer| {
                let reader = pty
                    .master
                    .try_clone_reader()
                    .context("cloning pty reader")?;
                Ok((writer, reader))
            });
        let (writer, reader) = match handles {
            Ok(handles) => handles,
            Err(err) => {
                // Nothing holds this child yet — no row, no reaper — so
                // returning here would leave a process the daemon can never
                // name again. Signal and reap it before the error goes out.
                abandon(child);
                return Err(err.into());
            }
        };

        let scrollback = request
            .scrollback_bytes
            .map(|bytes| usize::try_from(bytes).unwrap_or(usize::MAX))
            .unwrap_or(DEFAULT_SCROLLBACK_BYTES);
        // The screen starts at the size the pty was opened at, so a client that
        // attaches before ever resizing gets a repaint at the size it asked for.
        let hub = Arc::new(Mutex::new(OutputHub::new(
            scrollback,
            Some(SessionGrid::new(request.cols, request.rows)),
            None,
        )));

        let id = SessionId::new(new_id());
        let created_at = now_unix();
        let info = SessionInfo {
            id: id.clone(),
            workspace_id: workspace_id.clone(),
            agent_kind: request.agent_kind,
            instance_label: request.instance_label,
            cwd: request.cwd,
            created_at,
            // What the first sweep would derive anyway: a just-launched agent
            // is booting, not idle. Setting it here rather than letting the
            // sweeper correct it keeps `Created` honest and saves every new
            // session a spurious first transition.
            status: SessionStatus::Working,
        };
        let activity = Arc::new(Activity::new());

        {
            let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            sessions.insert(
                id.clone(),
                Session {
                    info: info.clone(),
                    since: created_at,
                    lost: false,
                    live: Some(Live {
                        master: pty.master,
                        writer: Arc::new(Mutex::new(writer)),
                        killer,
                        pid,
                    }),
                    hub: hub.clone(),
                    activity: activity.clone(),
                    cols: request.cols,
                    rows: request.rows,
                },
            );
            // The workspace event makes the session event meaningful. Keep
            // both under the session lock so kill/open cannot observe the row
            // between them, and announce a newly minted workspace first.
            self.ensure_workspace(&workspace_id, &info);
            self.events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .publish(&Frame::Created {
                    session: info.clone(),
                    request_id: None,
                });
        }
        if let Err(err) = self.persist() {
            log::warn!("could not persist session state: {err:#}");
        }

        let (exit_sender, exit_receiver) = std::sync::mpsc::sync_channel(1);
        spawn_drain(
            Arc::downgrade(self),
            reader,
            id.clone(),
            hub,
            activity.clone(),
            exit_receiver,
        );
        spawn_reaper(id, child, activity, exit_sender);
        Ok(info)
    }

    /// Create a workspace, its first login-shell session, and a one-leaf layout
    /// holding that session's terminal tab.
    ///
    /// One call, because that is what the gesture is: "Add workspace" is a
    /// single click and a window that opens with a terminal in it.
    pub fn create_workspace(
        self: &Arc<Self>,
        request: WorkspaceRequest,
    ) -> TableResult<(WorkspaceInfo, SessionInfo)> {
        let id = new_id();
        let name = request
            .name
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| default_workspace_name(&request.root));
        let project_root = request.root;
        // Empty command: the login shell, resolved by this daemon on this host.
        let session = self.create(CreateRequest {
            workspace_id: id.clone(),
            cwd: project_root.clone(),
            command: String::new(),
            env: request.env,
            cols: request.cols,
            rows: request.rows,
            agent_kind: "shell".to_owned(),
            instance_label: name.clone(),
            scrollback_bytes: None,
        })?;
        let workspace = WorkspaceInfo {
            id,
            name,
            project_root,
            created_at: session.created_at,
            layout_rev: 1,
            layout: LayoutDoc::single_terminal(session.id.clone()),
        };
        Ok((workspace, session))
    }

    /// Record a workspace for a session that named one the daemon does not
    /// have. Called with the session lock held; see [`Self::create`].
    fn ensure_workspace(&self, id: &str, session: &SessionInfo) {
        let created = {
            let mut workspaces = self.workspaces.lock().unwrap_or_else(|e| e.into_inner());
            if workspaces.contains_key(id) {
                return;
            }
            let name = if session.instance_label.is_empty() {
                default_workspace_name(&session.cwd)
            } else {
                session.instance_label.clone()
            };
            let workspace = WorkspaceInfo {
                id: id.to_owned(),
                name,
                project_root: session.cwd.clone(),
                created_at: session.created_at,
                layout_rev: 1,
                layout: LayoutDoc::single_terminal(session.id.clone()),
            };
            workspaces.insert(id.to_owned(), workspace.clone());
            workspace
        };
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .publish(&Frame::Workspace {
                workspace: created,
                sessions: vec![session.clone()],
                request_id: None,
            });
    }

    /// A workspace and every session in it. An id this daemon does not hold is
    /// [`error_code::NOT_FOUND`] — never an empty workspace, which would claim
    /// something exists.
    pub fn open_workspace(&self, id: &str) -> TableResult<(WorkspaceInfo, Vec<SessionInfo>)> {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let workspaces = self.workspaces.lock().unwrap_or_else(|e| e.into_inner());
        let workspace = workspaces
            .get(id)
            .ok_or_else(|| TableError::not_found(format!("no such workspace {id}")))?
            .clone();
        Ok((workspace, workspace_sessions_in(&sessions, id)))
    }

    /// The sessions belonging to a workspace, oldest first.
    pub fn workspace_sessions(&self, id: &str) -> Vec<SessionInfo> {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        workspace_sessions_in(&sessions, id)
    }

    /// Every workspace, oldest first.
    pub fn list_workspaces(&self) -> Vec<WorkspaceInfo> {
        let _sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let workspaces = self.workspaces.lock().unwrap_or_else(|e| e.into_inner());
        let mut infos: Vec<WorkspaceInfo> = workspaces.values().cloned().collect();
        infos.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        infos
    }

    /// Store `layout` as the workspace's layout at revision `rev`.
    ///
    /// Last writer wins, guarded by the revision: `rev` at or below the stored
    /// `layout_rev` is a client writing from a stale view and is rejected, so a
    /// client sends `layout_rev + 1` and learns from the error that it lost the
    /// race. The only thing validated inside the document is that every
    /// terminal tab names a session this daemon owns **in this workspace** —
    /// what an editor tab holds is the client's business.
    ///
    /// On success the accepted layout is pushed to every *other* subscriber as
    /// [`Frame::LayoutChanged`]; the writer gets its own reply from
    /// [`crate::server`]. Contrast [`Self::scrub_layout`], which broadcasts to
    /// everyone *including* the client that caused it — there the daemon
    /// decided and nobody is already holding the document.
    pub fn update_layout(
        &self,
        id: &str,
        layout: LayoutDoc,
        rev: u64,
        writer: SubscriberId,
    ) -> TableResult<()> {
        {
            let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let mut workspaces = self.workspaces.lock().unwrap_or_else(|e| e.into_inner());
            let workspace = workspaces
                .get_mut(id)
                .ok_or_else(|| TableError::not_found(format!("no such workspace {id}")))?;
            // The three refusals below are three different answers, and the
            // client acts differently on each: a stale rev is "re-read and
            // retry", an unknown session is "your document is wrong", a missing
            // workspace is "it is gone". That is the whole argument for codes.
            if rev <= workspace.layout_rev {
                return Err(TableError::stale_rev(format!(
                    "stale layout rev {rev} for workspace {id}, which is at {}",
                    workspace.layout_rev
                )));
            }
            for session_id in layout.terminal_sessions() {
                match sessions.get(&session_id) {
                    None => {
                        return Err(TableError::invalid_argument(format!(
                            "layout names unknown session {session_id}"
                        )));
                    }
                    Some(session) if session.info.workspace_id != id => {
                        return Err(TableError::invalid_argument(format!(
                            "layout names session {session_id} from another workspace"
                        )));
                    }
                    Some(_) => {}
                }
            }
            workspace.layout = layout.clone();
            workspace.layout_rev = rev;
            self.events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .publish_except(
                    writer,
                    &Frame::LayoutChanged {
                        workspace_id: id.to_owned(),
                        layout,
                        rev,
                        request_id: None,
                    },
                );
        }
        if let Err(err) = self.persist() {
            log::warn!("could not persist workspace state: {err:#}");
        }
        Ok(())
    }

    /// Give a workspace a new display name, keeping its id.
    ///
    /// **A name is metadata; the id is identity.** Every session carries the
    /// workspace id, the layout is stored under it, and the client's own rows
    /// are keyed by it — so renaming is the one field that can move without a
    /// single link having to be redrawn, and nothing here touches anything but
    /// the string.
    ///
    /// The name is trimmed, and an empty one is refused rather than stored: a
    /// workspace with no name is a row the user cannot tell from another. An
    /// unknown id is an error too — this never creates a workspace, unlike
    /// [`Self::create`], which does so on purpose.
    ///
    /// The renamed workspace goes out to every subscriber as
    /// [`Frame::Workspace`], the same event a new one announces itself with, so
    /// a client watching does not have to ask what changed.
    pub fn rename_workspace(
        &self,
        id: &str,
        name: &str,
    ) -> TableResult<(WorkspaceInfo, Vec<SessionInfo>)> {
        let name = name.trim();
        if name.is_empty() {
            return Err(TableError::invalid_argument(
                "a workspace name cannot be empty",
            ));
        }
        let (workspace, workspace_sessions) = {
            let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let mut workspaces = self.workspaces.lock().unwrap_or_else(|e| e.into_inner());
            let workspace = workspaces
                .get_mut(id)
                .ok_or_else(|| TableError::not_found(format!("no such workspace {id}")))?;
            workspace.name = name.to_owned();
            let workspace = workspace.clone();
            let workspace_sessions = workspace_sessions_in(&sessions, id);
            self.events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .publish(&Frame::Workspace {
                    workspace: workspace.clone(),
                    sessions: workspace_sessions.clone(),
                    request_id: None,
                });
            (workspace, workspace_sessions)
        };
        if let Err(err) = self.persist() {
            log::warn!("could not persist workspace state: {err:#}");
        }
        Ok((workspace, workspace_sessions))
    }

    /// Kill every session in the workspace and forget the workspace itself.
    ///
    /// The only workspace-level kill there is. Closing a single terminal tab is
    /// [`Self::kill`], and neither detaching nor closing a window reaches
    /// either of them.
    ///
    /// **Naming the doomed sessions and dropping the record are one critical
    /// section**, `sessions` then `workspaces` as the note on the type
    /// requires. Between two separate acquisitions a `CreateSession` naming
    /// this workspace can land its row *and* have [`Self::ensure_workspace`]
    /// put the record back, so the removal would take out a workspace that had
    /// just acquired a live session — announced as gone while it exists.
    ///
    /// Session rows and all removal events are committed under that same
    /// section. Their ptys are signalled only after the locks are released.
    ///
    /// [`error_code::NOT_FOUND`] if there was no such workspace and no session
    /// claiming it — killing the same workspace twice is an error, not a second
    /// removal.
    pub fn kill_workspace(&self, id: &str) -> TableResult<()> {
        let mut removed = {
            let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let mut workspaces = self.workspaces.lock().unwrap_or_else(|e| e.into_inner());
            let doomed: Vec<SessionId> = sessions
                .values()
                .filter(|session| session.info.workspace_id == id)
                .map(|session| session.info.id.clone())
                .collect();
            let existed = workspaces.remove(id).is_some();
            if !existed && doomed.is_empty() {
                return Err(TableError::not_found(format!("no such workspace {id}")));
            }
            let removed: Vec<Session> = doomed
                .iter()
                .filter_map(|session_id| sessions.remove(session_id))
                .collect();
            let mut events = self.events.lock().unwrap_or_else(|e| e.into_inner());
            for session in &removed {
                let event = Frame::Removed {
                    session_id: session.info.id.clone(),
                };
                events.publish(&event);
                session
                    .hub
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .publish_event(&event);
            }
            events.publish(&Frame::WorkspaceRemoved {
                workspace_id: id.to_owned(),
                request_id: None,
            });
            removed
        };
        for session in &mut removed {
            kill_session_process(session);
        }
        drop(removed);
        if let Err(err) = self.persist() {
            log::warn!("could not persist workspace state: {err:#}");
        }
        Ok(())
    }

    /// Register `sender` for the event stream and push the initial snapshot:
    /// one [`Frame::Status`] per session, including exited and lost ones.
    ///
    /// The snapshot is built and sent while the registration is in place and
    /// the session lock is held, so nothing can change status in the gap.
    /// Subscribing twice just resends the snapshot.
    pub fn subscribe(&self, subscriber: SubscriberId, sender: &Sender<Frame>) {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let mut events = self.events.lock().unwrap_or_else(|e| e.into_inner());
        events.subscribe(subscriber, sender);
        let mut snapshot: Vec<&Session> = sessions.values().collect();
        snapshot.sort_by(|a, b| {
            a.info
                .created_at
                .cmp(&b.info.created_at)
                .then(a.info.id.cmp(&b.info.id))
        });
        for session in snapshot {
            if sender
                .try_send(Frame::Status {
                    session_id: session.info.id.clone(),
                    status: session.info.status,
                    since: session.since,
                })
                .is_err()
            {
                events.unsubscribe(subscriber);
                break;
            }
        }
    }

    /// Queue a [`Frame::Replay`] of the whole ring on `sender`, then stream
    /// live [`Frame::Output`] there until detach or disconnect — followed by
    /// the [`Frame::Exited`] or [`Frame::Removed`] that ends the stream, so an
    /// attached client never has to subscribe just to learn its session is
    /// over (see [`OutputHub::publish_event`]).
    ///
    /// [`error_code::NOT_FOUND`] for an unknown session. Attaching to an exited
    /// or lost session succeeds and replays whatever the ring holds — that is
    /// how a crashed agent's last words stay readable.
    pub fn attach(
        &self,
        id: &SessionId,
        subscriber: SubscriberId,
        sender: &Sender<Frame>,
    ) -> TableResult<()> {
        let hub = {
            let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            match sessions.get(id) {
                Some(session) => session.hub.clone(),
                None => return Err(TableError::not_found(format!("no such session {id}"))),
            }
        };
        hub.lock()
            .unwrap_or_else(|e| e.into_inner())
            .attach(id, subscriber, sender);
        Ok(())
    }

    /// Stop streaming this session to this connection. **Never touches the
    /// session**, and detaching something that was not attached is a no-op.
    pub fn detach(&self, id: &SessionId, subscriber: SubscriberId) {
        let hub = {
            let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            match sessions.get(id) {
                Some(session) => session.hub.clone(),
                None => return,
            }
        };
        hub.lock()
            .unwrap_or_else(|e| e.into_inner())
            .detach(subscriber);
    }

    /// Detach `subscriber` from every session and from the event stream. This
    /// is what a dropped connection does — and all it does.
    pub fn detach_all(&self, subscriber: SubscriberId) {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .unsubscribe(subscriber);
        let hubs: Vec<Arc<Mutex<OutputHub>>> = {
            let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            sessions
                .values()
                .map(|session| session.hub.clone())
                .collect()
        };
        for hub in hubs {
            hub.lock()
                .unwrap_or_else(|e| e.into_inner())
                .detach(subscriber);
        }
    }

    /// Write client keystrokes to the pty master.
    ///
    /// The writer mutex is cloned out from under the table lock so that a pty
    /// whose child has stopped reading cannot wedge `list` or `create`.
    ///
    /// A session that exists but cannot take bytes — lost with a previous
    /// daemon, or exited — is [`error_code::INVALID_ARGUMENT`] and not
    /// `not_found`: the id names a row the client can still see, so telling it
    /// the session is gone would send it looking for the wrong bug.
    pub fn write(&self, id: &SessionId, bytes: &[u8]) -> TableResult<()> {
        let writer = {
            let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let session = sessions
                .get(id)
                .ok_or_else(|| TableError::not_found(format!("no such session {id}")))?;
            let Some(live) = session.live.as_ref() else {
                return Err(TableError::invalid_argument(format!(
                    "session {id} was lost when the daemon restarted"
                )));
            };
            if session.info.status == SessionStatus::Exited {
                return Err(TableError::invalid_argument(format!(
                    "session {id} has exited"
                )));
            }
            live.writer.clone()
        };
        let mut writer = writer.lock().unwrap_or_else(|e| e.into_inner());
        writer
            .write_all(bytes)
            .with_context(|| format!("writing to session {id}"))?;
        writer
            .flush()
            .with_context(|| format!("flushing session {id}"))?;
        Ok(())
    }

    /// Resize the screen, then the pty, and remember the new size.
    ///
    /// **The screen is resized first, and unconditionally.** First, because the
    /// child learns its new size from the `SIGWINCH` the pty resize sends and
    /// will start drawing at it immediately — a screen still at the old size
    /// would take those bytes as if the terminal had not moved. Unconditionally,
    /// because a session whose pty is gone still has a size a client can ask
    /// for: a lost row that a client mounts into a small split has no pty to
    /// resize and no screen either, and erroring before recording the size
    /// would leave `cols`/`rows` lying about what the client asked for.
    pub fn resize(&self, id: &SessionId, cols: u16, rows: u16) -> TableResult<()> {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let session = sessions
            .get_mut(id)
            .ok_or_else(|| TableError::not_found(format!("no such session {id}")))?;
        session.cols = cols.max(1);
        session.rows = rows.max(1);
        // Lock order is sessions then hub, the same way `kill` takes them.
        session
            .hub
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .resize_grid(cols.max(1), rows.max(1));
        let Some(live) = session.live.as_ref() else {
            return Err(TableError::invalid_argument(format!(
                "session {id} was lost when the daemon restarted"
            )));
        };
        // A TIOCSWINSZ ioctl: no blocking, so holding the table lock is fine.
        live.master
            .resize(PtySize {
                rows: rows.max(1),
                cols: cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .with_context(|| format!("resizing session {id}"))?;
        Ok(())
    }

    /// Every session, live and lost, oldest first.
    pub fn list(&self) -> Vec<SessionInfo> {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let mut infos: Vec<SessionInfo> = sessions.values().map(|s| s.info.clone()).collect();
        infos.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        infos
    }

    /// The only path that removes a session. Signals the child, drops the pty
    /// (which EOFs the drain thread), scrubs the session out of its workspace's
    /// layout and forgets the row.
    ///
    /// **Forgetting the row and scrubbing its tab are one critical section**,
    /// `sessions` then `workspaces` as the note on the type requires. Taking
    /// the two locks in turn instead would leave a window in which
    /// [`Self::open_workspace`] or [`Self::list_workspaces`] hands out a layout
    /// naming a session the table no longer has — the exact wedged document the
    /// scrub exists to prevent.
    ///
    /// [`error_code::NOT_FOUND`] if there is no such session.
    pub fn kill(&self, id: &SessionId) -> TableResult<()> {
        if self.remove_session(id) {
            Ok(())
        } else {
            Err(TableError::not_found(format!("no such session {id}")))
        }
    }

    /// [`Self::kill`]'s body, minus the classification.
    ///
    /// Returns `false` if there is no such session.
    fn remove_session(&self, id: &SessionId) -> bool {
        let mut session = {
            let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let Some(session) = sessions.remove(id) else {
                return false;
            };
            let event = Frame::Removed {
                session_id: id.clone(),
            };
            self.events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .publish(&event);
            // Whoever was streaming this session has to be told too, or an
            // attached client would wait forever on a pty that is gone.
            session
                .hub
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .publish_event(&event);
            self.scrub_layout(&session.info.workspace_id, id);
            session
        };
        kill_session_process(&mut session);
        drop(session);
        if let Err(err) = self.persist() {
            log::warn!("could not persist session state: {err:#}");
        }
        true
    }

    /// Take a killed session's terminal tab out of its workspace's layout.
    ///
    /// **Part of the kill, not a follow-up the client is trusted to send** —
    /// tmux's `kill-pane`, where the pane and the cell holding it go together.
    /// Without it a workspace wedges permanently: the client emits `Kill` for a
    /// terminal tab that was only *dragged* into a split, then pushes an
    /// [`Frame::UpdateLayout`] still naming it, which is refused for naming an
    /// unknown session — so the stored layout keeps the dead reference and
    /// every reopen renders a tab that cannot attach.
    ///
    /// Collapsing is [`LayoutDoc::retain_sessions`]' own rule, unchanged: the
    /// tab is removed, an emptied leaf disappears, a split that loses a child
    /// *becomes* its sibling, and `active` is clamped. Editor tabs are never
    /// touched.
    ///
    /// Unlike [`Self::update_layout`] the frame this publishes goes to
    /// **every** subscriber, the killer included. `update_layout` excludes the
    /// writer because that client already holds the document it just sent; here
    /// the *daemon* decided, so nobody holds it — and the killer needs it most,
    /// since its own next `UpdateLayout` would otherwise be built on a layout
    /// naming the session it just killed.
    ///
    /// Called with the session lock held, so the state change and its event use
    /// the table's `sessions`, `workspaces`, `events` lock order. A later
    /// revision or workspace removal cannot be announced ahead of this one.
    fn scrub_layout(&self, workspace_id: &str, session_id: &SessionId) {
        let mut workspaces = self.workspaces.lock().unwrap_or_else(|e| e.into_inner());
        // `kill_workspace` pops the record before killing its sessions, so
        // there is no layout left to scrub and `WorkspaceRemoved` says more
        // than a `LayoutChanged` about a workspace that is gone would.
        let Some(workspace) = workspaces.get_mut(workspace_id) else {
            return;
        };
        if !workspace.layout.retain_sessions(|id| id != session_id) {
            // The session held no terminal tab — nothing changed, so nothing is
            // announced and the rev does not move.
            return;
        }
        workspace.layout_rev += 1;
        log::info!(
            "scrubbed killed session {session_id} from workspace {workspace_id} layout, now at rev {}",
            workspace.layout_rev
        );
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .publish(&Frame::LayoutChanged {
                workspace_id: workspace_id.to_owned(),
                layout: workspace.layout.clone(),
                rev: workspace.layout_rev,
                request_id: None,
            });
    }

    /// Called by the reaper thread once the child has been waited on. The row
    /// stays — the [`Frame::Exited`] event is the only thing that fires, and
    /// it fires exactly once because the reaper waits exactly once.
    ///
    /// The status itself is left to the next [`Self::sweep`], so that every
    /// status in the table comes from [`derive_status`] and from nowhere else.
    fn report_exit(&self, id: &SessionId, exit_code: Option<i32>) {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let Some(session) = sessions.get(id) else {
            // Killed while we were waiting: `Removed` has already been sent
            // and an `Exited` for a session the client has forgotten is noise.
            return;
        };
        let event = Frame::Exited {
            session_id: id.clone(),
            exit_code,
        };
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .publish(&event);
        // Same reason as in `kill`: an attached client learns the process is
        // gone from the stream it is attached to, not from the event hub it
        // never subscribed to.
        session
            .hub
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .publish_event(&event);
    }

    /// Re-derive every session's status and push the ones that changed.
    ///
    /// Called by the sweeper thread on [`StatusConfig::sweep_interval`]. This
    /// is the *only* writer of `info.status` after creation, which is what
    /// makes [`derive_status`] the single definition of ADE's status dots.
    pub fn sweep(&self) {
        let now = Instant::now();
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let mut changed = Vec::new();
        for session in sessions.values_mut() {
            let derived = derive_status(session, self.status, now);
            if derived != session.info.status {
                session.info.status = derived;
                session.since = now_unix();
                changed.push(Frame::Status {
                    session_id: session.info.id.clone(),
                    status: derived,
                    since: session.since,
                });
            }
        }
        if changed.is_empty() {
            return;
        }
        // Still under the session lock: see the lock-order note on the type.
        let mut events = self.events.lock().unwrap_or_else(|e| e.into_inner());
        for frame in &changed {
            events.publish(frame);
        }
    }

    pub fn state(&self) -> &StateStore {
        &self.state
    }

    /// Write every workspace, and the metadata of every *live* session. Lost
    /// session rows are excluded on purpose (see [`SessionTable::load`]);
    /// workspaces are not, because a workspace outlives the sessions in it.
    ///
    /// Serialized end to end by `persisting`, and the two tables are snapshotted
    /// under **one** acquisition — sessions then workspaces, the order the note
    /// on the type gives. Both matter: taking them separately can pair a
    /// session set with a workspace set that never coexisted, and writing
    /// outside the guard lets a slower persist land its older pair last.
    fn persist(&self) -> Result<()> {
        let _persisting = self.persisting.lock().unwrap_or_else(|e| e.into_inner());
        let (records, workspaces) = {
            let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let workspaces = self.workspaces.lock().unwrap_or_else(|e| e.into_inner());
            let mut records: Vec<PersistedSession> = sessions
                .values()
                .filter(|session| !session.lost)
                .map(|session| PersistedSession::from_info(&session.info))
                .collect();
            records.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
            let mut infos: Vec<WorkspaceInfo> = workspaces.values().cloned().collect();
            infos.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
            (records, infos)
        };
        self.state.save(&records, &workspaces)
    }
}

/// ADE's status-dot rules, in the order they are checked.
///
/// The order is the whole content of this function:
///
/// 1. **Dead or lost → `Exited`.** A row outlives its process; that is the
///    point of the daemon.
/// 2. **Bell → `NeedsInput`.** An explicit "look at me" outranks everything a
///    heuristic could infer — a real one, in VT ground state, not the BEL that
///    ends a title sequence ([`BellScan`]).
/// 3. **Output within [`StatusConfig::needs_input_after`] → `Working`.**
///    Deliberately *above* the shell check, unlike the order the rules are
///    usually recited in: a session running `sh -c 'while …'` has a shell as
///    its foreground process while it is plainly working, so the idle rule is
///    only meaningful once the output has gone quiet.
/// 4. **Quiet, and the pty's foreground process is a shell → `Idle`.** This
///    rule is skipped, not guessed at, wherever the probe cannot answer (see
///    [`foreground_is_shell`]).
/// 5. **Otherwise `NeedsInput`** — a running agent that has said nothing for
///    longer than the threshold is waiting on the human.
///
/// A session that has never produced a byte counts its silence from creation,
/// so a freshly launched agent shows `Working` while it boots.
fn derive_status(session: &Session, config: StatusConfig, now: Instant) -> SessionStatus {
    if session.lost {
        return SessionStatus::Exited;
    }
    let (last_output, bell, dead) = session.activity.snapshot();
    if dead {
        return SessionStatus::Exited;
    }
    if bell {
        return SessionStatus::NeedsInput;
    }
    if now.saturating_duration_since(last_output) < config.needs_input_after {
        return SessionStatus::Working;
    }
    let is_shell = session
        .live
        .as_ref()
        .and_then(|live| foreground_is_shell(live.master.as_ref()));
    if is_shell == Some(true) {
        return SessionStatus::Idle;
    }
    SessionStatus::NeedsInput
}

/// Is the pty's foreground process group leader a shell?
///
/// `None` means "do not know" — a platform without `/proc`, a process that has
/// already gone, or a pty with no foreground group. The caller must skip the
/// idle rule in that case rather than guess: reporting a working agent as idle
/// is worse than reporting nothing.
fn foreground_is_shell(master: &(dyn MasterPty + Send)) -> Option<bool> {
    #[cfg(target_os = "linux")]
    {
        // `process_group_leader` is portable-pty's `tcgetpgrp` on the master.
        let pgid = master.process_group_leader()?;
        if pgid <= 0 {
            return None;
        }
        let comm = std::fs::read_to_string(format!("/proc/{pgid}/comm")).ok()?;
        Some(is_shell_name(&comm))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = master;
        None
    }
}

/// Classify a `/proc/<pid>/comm` value. Split out from the probe so the rule
/// is testable without a pty. Gated like [`SHELL_NAMES`].
#[cfg(any(target_os = "linux", test))]
fn is_shell_name(comm: &str) -> bool {
    // A login shell is conventionally `-bash`; `comm` normally strips the
    // dash, but stripping it here costs nothing and covers the shells that
    // rewrite their own name.
    let name = comm.trim().trim_start_matches('-');
    let name = name.rsplit('/').next().unwrap_or(name);
    SHELL_NAMES.contains(&name)
}

/// Re-derive every session's status on a fixed cadence.
///
/// One thread for the whole table, not one per session: the work is a handful
/// of instant comparisons and at most one `tcgetpgrp` per session. It stops on
/// its own once the table is dropped, which is what ends it in tests.
fn spawn_sweeper(table: Weak<SessionTable>, interval: Duration) {
    std::thread::Builder::new()
        .name("ade-status-sweep".to_owned())
        .spawn(move || {
            loop {
                std::thread::sleep(interval);
                match table.upgrade() {
                    Some(table) => table.sweep(),
                    None => break,
                }
            }
        })
        .expect("spawning status sweeper thread");
}

/// The whole kill sequence for one removed row, shared by `Kill` and
/// `KillWorkspace`.
///
/// The group first: its reaped-gate must read the state from *before* this
/// kill — the direct kill below can end the leader and have it reaped fast
/// enough to read as a long-dead row. The direct kill sits behind the same
/// gate, because the killer signals a raw pid the kernel may have recycled.
/// The killer reaches the direct child only, and dropping the pty reaches
/// whatever is in the foreground; neither is enough on its own — see
/// [`terminate_group`].
fn kill_session_process(session: &mut Session) {
    let id = session.info.id.clone();
    let pid = session.live.as_ref().and_then(|live| live.pid);
    let activity = session.activity.clone();
    terminate_group(&id, pid, activity.clone());
    if activity.is_dead() {
        // Reaped before this kill began.
    } else if let Some(live) = session.live.as_mut()
        && let Err(err) = live.killer.kill()
    {
        // Already dead is the common case here, not a failure.
        log::debug!("killing {id}: {err}");
    }
}

/// End a killed session's whole process group, and make sure it ended.
///
/// Three things push at a killed session, and only the last is unconditional.
/// The killer's `SIGHUP` reaches the direct child; closing the pty makes the
/// kernel `SIGHUP` whatever is in the *foreground* of it. An agent that traps
/// `SIGHUP`, or a descendant sitting in the background, survives both — and
/// once the row is gone nothing can name that process again, so it keeps its
/// files and its locks for good. That is the shape of the reported failure: a
/// killed Codex kept its per-thread writer lock, and the next `codex resume`
/// was refused because the thread "already has an active writer".
///
/// So the group gets a `SIGHUP` of its own, and after [`KILL_GRACE`] a
/// `SIGKILL`, which nothing can trap. The wait runs on a detached thread —
/// `Kill` still answers `Removed` at once. A row whose child was already
/// reaped gets neither signal: its pid may have been recycled, and the group
/// signals would land on a stranger.
///
/// The child is a session leader (`portable-pty` calls `setsid` before `exec`),
/// so its pid *is* its process-group id, that group holds every descendant that
/// did not deliberately leave it, and the daemon — in another session entirely
/// — can never be caught by this. A descendant that called `setsid` itself is
/// beyond any signal we could send; only a cgroup would follow it there.
fn terminate_group(label: &dyn std::fmt::Display, pid: Option<u32>, activity: Arc<Activity>) {
    #[cfg(unix)]
    {
        let Some(pid) = pid
            .and_then(|pid| libc::pid_t::try_from(pid).ok())
            .filter(|pid| *pid > 0)
        else {
            return;
        };
        // Once the reaper has waited on the child, the kernel may hand its pid
        // to anyone — signalling `-pid` could kill an unrelated group. So a
        // row whose child was already reaped (killed long after it exited)
        // gets no group signal at all; a descendant that survived its leader
        // this long was deliberate. An unreaped leader keeps the pid reserved,
        // which makes the signals below safe.
        if activity.is_dead() {
            return;
        }
        signal_group(pid, libc::SIGHUP);
        let label = label.to_string();
        let escalate = std::thread::Builder::new()
            .name(format!("ade-kill-{pid}"))
            .spawn(move || {
                std::thread::sleep(KILL_GRACE);
                // No re-check of `is_dead` here: reaping the *leader* inside
                // the grace is the trap-SIGHUP case this SIGKILL exists for,
                // and surviving members keep the pgid reserved. The pid is
                // recyclable only once the whole group is gone — and then a
                // wrap of pid_max inside one second is not a real window.
                if signal_group(pid, libc::SIGKILL) {
                    log::warn!("{label} outlived SIGHUP; killed its process group {pid}");
                }
            });
        if let Err(err) = escalate {
            log::warn!("could not spawn the kill escalation for {pid}: {err}");
        }
    }
    #[cfg(not(unix))]
    {
        // Windows has no process groups to signal and no daemon to signal them
        // from; the killer's `TerminateProcess` is the whole story there.
        let _ = (label, pid, activity);
    }
}

/// Send `signal` to the process group led by `pid`; `true` if anything was
/// there to receive it.
///
/// `pid` must be positive. `kill(0, ...)` would signal the daemon's own group
/// and `kill(-1, ...)` every process it can reach.
#[cfg(unix)]
fn signal_group(pid: libc::pid_t, signal: libc::c_int) -> bool {
    debug_assert!(pid > 0, "a process group id is a positive pid");
    // SAFETY: `kill(2)` against a group this daemon created, never 0 or -1.
    unsafe { libc::kill(-pid, signal) == 0 }
}

/// End a child that never reached the table.
///
/// Between the spawn and the insert there is no row and no reaper, so returning
/// an error from in there would drop the [`portable_pty::Child`] — which on
/// unix neither signals nor waits — and leave a live process nothing in the
/// daemon knows about. Reaping happens on its own thread because the
/// `SIGKILL` that guarantees the child goes is on a timer, and `create` must
/// not block on it.
fn abandon(mut child: Box<dyn portable_pty::Child + Send + Sync>) {
    let pid = child.process_id();
    if let Err(err) = child.kill() {
        // Already dead is fine here too.
        log::debug!("killing an abandoned child: {err}");
    }
    // A fresh activity: the child is not reaped until the thread below runs,
    // which is exactly what makes the group signals safe to send.
    terminate_group(&"an abandoned session", pid, Arc::new(Activity::new()));
    if let Err(err) = std::thread::Builder::new()
        .name("ade-reap-abandoned".to_owned())
        .spawn(move || {
            if let Err(err) = child.wait() {
                log::warn!("waiting on an abandoned child: {err}");
            }
        })
    {
        log::warn!("could not spawn a reaper for an abandoned child: {err}");
    }
}

/// Wait for the child and hand its exit status to the PTY drain. One blocking
/// thread per session, which is the boring way to do this and costs nothing at
/// ADE's session counts.
fn spawn_reaper(
    id: SessionId,
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    activity: Arc<Activity>,
    exit_sender: std::sync::mpsc::SyncSender<Option<i32>>,
) {
    std::thread::Builder::new()
        .name(format!("ade-reap-{id}"))
        .spawn(move || {
            let exit_code = match child.wait() {
                Ok(status) => {
                    log::debug!("session {id} exited with code {}", status.exit_code());
                    i32::try_from(status.exit_code()).ok()
                }
                Err(err) => {
                    log::warn!("waiting on session {id}: {err}");
                    None
                }
            };
            // The fact first, so that a sweep racing the event can only ever
            // be late, never contradictory.
            activity.mark_dead();
            if exit_sender.send(exit_code).is_err() {
                log::debug!("session {id} output drain stopped before its reaper");
            }
        })
        .expect("spawning reaper thread");
}

/// Drain pty output into the session's ring and out to attached connections.
///
/// The only producer for [`OutputHub`], and the reason a chatty child never
/// blocks on a full pty buffer whether anyone is watching or not. The daemon
/// does not interpret these bytes — they are stored and forwarded raw, for the
/// client's terminal emulator to make sense of.
fn spawn_drain(
    table: Weak<SessionTable>,
    mut reader: Box<dyn std::io::Read + Send>,
    id: SessionId,
    hub: Arc<Mutex<OutputHub>>,
    activity: Arc<Activity>,
    exit_receiver: std::sync::mpsc::Receiver<Option<i32>>,
) {
    std::thread::Builder::new()
        .name(format!("ade-drain-{id}"))
        .spawn(move || {
            let mut buffer = [0u8; DRAIN_CHUNK_BYTES];
            // A read error here is the pty hanging up (EIO on Linux once the
            // child is gone), which is the same end of stream as `Ok(0)`.
            while let Ok(read) = reader.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                // Facts only: what this output *means* is the sweeper's call.
                activity.record_output(&buffer[..read]);
                hub.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .publish(&id, &buffer[..read]);
            }
            log::debug!("session {id} pty reached end of output");
            match exit_receiver.recv() {
                Ok(exit_code) => {
                    if let Some(table) = table.upgrade() {
                        table.report_exit(&id, exit_code);
                    }
                }
                Err(_) => log::warn!("session {id} reaper stopped before its output drain"),
            }
        })
        .expect("spawning drain thread");
}

fn shell() -> String {
    if cfg!(windows) {
        "sh".into()
    } else {
        "/bin/sh".into()
    }
}

/// The environment a session's pty is spawned with: the terminal type the
/// daemon vouches for, then the request's own variables, which win.
///
/// Every client that can attach is a full emulator, but the daemon itself
/// rarely was started from one — a spawned background process inherits no
/// `TERM`, and a shell without one prints for a teletype: no colors, no
/// prompt styling. Like tmux, the daemon states the terminal type of the
/// ptys it serves rather than passing on the accident of its own launch.
fn terminal_env(request_env: &[(String, String)]) -> Vec<(String, String)> {
    [("TERM", "xterm-256color"), ("COLORTERM", "truecolor")]
        .into_iter()
        .filter(|(key, _)| !request_env.iter().any(|(name, _)| name == key))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .chain(request_env.iter().cloned())
        .collect()
}

/// The login shell of the user this daemon runs as, on **this** host.
///
/// `$SHELL` first — it is what the user's own login set — then the passwd
/// entry, which is the authoritative answer when the daemon was started
/// without a login environment (an ssh `exec`, a systemd unit). [`shell`] is
/// the last resort, and is all Windows ever gets.
fn resolve_login_shell() -> String {
    login_shell_from(std::env::var("SHELL").ok(), passwd_login_shell)
}

/// [`resolve_login_shell`] with both lookups injected, so the precedence is
/// testable without mutating the process environment.
fn login_shell_from(
    env_shell: Option<String>,
    passwd_lookup: impl FnOnce() -> Option<String>,
) -> String {
    env_shell
        .filter(|shell| !shell.is_empty())
        .or_else(passwd_lookup)
        .unwrap_or_else(shell)
}

/// This user's shell according to `/etc/passwd`. `None` everywhere but unix.
fn passwd_login_shell() -> Option<String> {
    #[cfg(unix)]
    {
        let user = std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .ok()
            .filter(|user| !user.is_empty())?;
        let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
        passwd_shell(&passwd, &user)
    }
    #[cfg(not(unix))]
    None
}

/// The shell field (7th, colon-separated) of `user`'s line in `passwd`.
///
/// Split out from [`resolve_login_shell`] so the parsing is testable without a
/// real `/etc/passwd`. Lines with fewer than seven fields, or an empty shell
/// field, are treated as no answer.
#[cfg(unix)]
fn passwd_shell(passwd: &str, user: &str) -> Option<String> {
    passwd.lines().find_map(|line| {
        if line.split(':').next()? != user {
            return None;
        }
        line.split(':')
            .nth(6)
            .filter(|shell| !shell.is_empty())
            .map(str::to_owned)
    })
}

/// A fresh opaque id, for a session or a workspace.
fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;
    #[cfg(unix)]
    use std::time::Instant;

    use ade_session::proto::{LayoutDoc, SessionId, SessionInfo, SessionStatus, WorkspaceInfo};

    use super::{
        Activity, SessionTable, StatusConfig, SubscriberId, is_shell_name, login_shell_from, shell,
        terminal_env,
    };
    #[cfg(unix)]
    use super::{KILL_GRACE, terminate_group};
    use crate::state::{PersistedSession, StateStore};

    /// Feed `chunks` to one session in order: is a bell pending after the last
    /// of them? Goes through [`Activity`] rather than [`super::BellScan`]
    /// alone because the scan state and the sticky-until-the-next-chunk rule
    /// have to hold together.
    fn bell_after(chunks: &[&[u8]]) -> bool {
        let activity = Activity::new();
        for chunk in chunks {
            activity.record_output(chunk);
        }
        activity.snapshot().1
    }

    /// A table holding one session and the workspace whose layout names it,
    /// built without a pty: the session is adopted as a *lost* row, which
    /// [`SessionTable::kill`] removes exactly like a live one.
    ///
    /// The sweep interval is set past any test's lifetime on purpose — the
    /// lock-scope tests below assert on who holds the session lock, and a
    /// sweeper waking up mid-assertion would be a second answer to that.
    fn seeded_table(dir: &std::path::Path) -> (Arc<SessionTable>, SessionId) {
        let session = SessionInfo {
            id: SessionId::new("session-1"),
            workspace_id: "workspace-1".to_owned(),
            agent_kind: "shell".to_owned(),
            instance_label: "one".to_owned(),
            cwd: "/tmp".to_owned(),
            created_at: 1,
            status: SessionStatus::Exited,
        };
        let workspace = WorkspaceInfo {
            id: session.workspace_id.clone(),
            name: "one".to_owned(),
            project_root: session.cwd.clone(),
            created_at: 1,
            layout_rev: 1,
            layout: LayoutDoc::single_terminal(session.id.clone()),
        };
        StateStore::new(dir)
            .save(&[PersistedSession::from_info(&session)], &[workspace])
            .expect("seeding the state file");
        let table = SessionTable::load(
            StateStore::new(dir),
            StatusConfig {
                needs_input_after: Duration::from_secs(600),
                sweep_interval: Duration::from_secs(600),
            },
        );
        (table, session.id)
    }

    #[test]
    fn load_prunes_sessions_borrowed_from_another_workspace() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let session = SessionInfo {
            id: SessionId::new("session-b"),
            workspace_id: "workspace-b".to_owned(),
            agent_kind: "shell".to_owned(),
            instance_label: "b".to_owned(),
            cwd: "/tmp/b".to_owned(),
            created_at: 1,
            status: SessionStatus::Exited,
        };
        let owner = WorkspaceInfo {
            id: "workspace-b".to_owned(),
            name: "b".to_owned(),
            project_root: "/tmp/b".to_owned(),
            created_at: 1,
            layout_rev: 1,
            layout: LayoutDoc::single_terminal(session.id.clone()),
        };
        let borrower = WorkspaceInfo {
            id: "workspace-a".to_owned(),
            name: "a".to_owned(),
            project_root: "/tmp/a".to_owned(),
            created_at: 2,
            layout_rev: 4,
            layout: LayoutDoc::single_terminal(session.id.clone()),
        };
        StateStore::new(dir.path())
            .save(&[PersistedSession::from_info(&session)], &[owner, borrower])
            .expect("seeding the legacy state file");

        let table = SessionTable::load(
            StateStore::new(dir.path()),
            StatusConfig {
                needs_input_after: Duration::from_secs(600),
                sweep_interval: Duration::from_secs(600),
            },
        );

        let (borrower, sessions) = table
            .open_workspace("workspace-a")
            .expect("borrower workspace");
        assert!(sessions.is_empty());
        assert!(borrower.layout.terminal_sessions().is_empty());
        assert_eq!(
            borrower.layout_rev, 4,
            "startup repair is not a client edit"
        );

        let rewritten = StateStore::new(dir.path()).load();
        let borrower = rewritten
            .workspaces
            .into_iter()
            .find(|workspace| workspace.id == "workspace-a")
            .expect("rewritten borrower workspace");
        assert!(borrower.layout.terminal_sessions().is_empty());
    }

    /// Long enough that a thread parked on a lock has certainly reached it.
    const REACHED_THE_LOCK: Duration = Duration::from_millis(250);

    /// Removing the row and pruning its tab are **one** critical section.
    ///
    /// Made observable by holding the workspace lock from here: a `kill` that
    /// took the two in turn would have let go of the session lock before
    /// blocking, and that gap is exactly when a reader can be handed a layout
    /// naming a session the table no longer has.
    #[test]
    fn kill_holds_the_session_lock_across_the_layout_scrub() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let (table, session) = seeded_table(dir.path());
        let workspaces = table.workspaces.lock().expect("the workspace lock");

        let killer = {
            let table = Arc::clone(&table);
            std::thread::spawn(move || table.kill(&session))
        };
        std::thread::sleep(REACHED_THE_LOCK);
        assert!(
            table.sessions.try_lock().is_err(),
            "kill let go of the session lock before taking the workspace lock"
        );

        drop(workspaces);
        killer.join().expect("the killer thread").expect("the kill");
        assert!(
            table
                .open_workspace("workspace-1")
                .expect("the workspace")
                .0
                .layout
                .terminal_sessions()
                .is_empty(),
            "the tab went with the session"
        );
    }

    /// Naming the doomed sessions and dropping the record are one critical
    /// section too, so a `CreateSession` cannot put the record back between
    /// them and have it removed out from under a live session.
    #[test]
    fn kill_workspace_holds_the_session_lock_across_dropping_the_record() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let (table, _) = seeded_table(dir.path());
        let workspaces = table.workspaces.lock().expect("the workspace lock");

        let killer = {
            let table = Arc::clone(&table);
            std::thread::spawn(move || table.kill_workspace("workspace-1"))
        };
        std::thread::sleep(REACHED_THE_LOCK);
        assert!(
            table.sessions.try_lock().is_err(),
            "kill_workspace let go of the session lock before dropping the record"
        );

        drop(workspaces);
        killer
            .join()
            .expect("the killer thread")
            .expect("the workspace kill");
        assert!(table.open_workspace("workspace-1").is_err());
        assert!(table.list().is_empty());
    }

    /// A workspace and its sessions are one snapshot. Taking the workspace
    /// lock first lets a concurrent kill remove the sessions after the layout
    /// was cloned, returning a document whose terminal tabs have no rows.
    #[test]
    fn open_workspace_holds_the_session_lock_until_its_snapshot_is_complete() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let (table, _) = seeded_table(dir.path());
        let workspaces = table.workspaces.lock().expect("the workspace lock");

        let opener = {
            let table = Arc::clone(&table);
            std::thread::spawn(move || table.open_workspace("workspace-1"))
        };
        std::thread::sleep(REACHED_THE_LOCK);
        assert!(
            table.sessions.try_lock().is_err(),
            "open_workspace did not reserve the session half of its snapshot"
        );

        drop(workspaces);
        let (workspace, sessions) = opener
            .join()
            .expect("the opener thread")
            .expect("the workspace");
        assert_eq!(
            workspace.layout.terminal_sessions(),
            vec![sessions[0].id.clone()]
        );
    }

    /// Listing layouts participates in the same snapshot even though it does
    /// not return session rows. Otherwise it can slip between removing a
    /// session and scrubbing that session's terminal tab.
    #[test]
    fn list_workspaces_holds_the_session_lock_until_layouts_are_cloned() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let (table, _) = seeded_table(dir.path());
        let workspaces = table.workspaces.lock().expect("the workspace lock");

        let lister = {
            let table = Arc::clone(&table);
            std::thread::spawn(move || table.list_workspaces())
        };
        std::thread::sleep(REACHED_THE_LOCK);
        assert!(
            table.sessions.try_lock().is_err(),
            "list_workspaces can clone a layout between session removal and scrub"
        );

        drop(workspaces);
        assert_eq!(lister.join().expect("the lister thread").len(), 1);
    }

    /// Rename publishes the workspace together with its sessions. It must use
    /// the same transaction as kill, or a delayed rename can announce a stale
    /// workspace after `WorkspaceRemoved`.
    #[test]
    fn rename_workspace_holds_the_session_lock_through_its_snapshot() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let (table, _) = seeded_table(dir.path());
        let workspaces = table.workspaces.lock().expect("the workspace lock");

        let renamer = {
            let table = Arc::clone(&table);
            std::thread::spawn(move || table.rename_workspace("workspace-1", "renamed"))
        };
        std::thread::sleep(REACHED_THE_LOCK);
        assert!(
            table.sessions.try_lock().is_err(),
            "rename_workspace can publish a workspace paired with a later session set"
        );

        drop(workspaces);
        let (workspace, sessions) = renamer
            .join()
            .expect("the renamer thread")
            .expect("the rename");
        assert_eq!(workspace.name, "renamed");
        assert_eq!(sessions.len(), 1);
    }

    /// The revision and its broadcast are one ordered operation. Releasing the
    /// workspace lock first lets a later revision publish before this one.
    #[test]
    fn update_layout_holds_the_workspace_lock_until_its_event_is_published() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let (table, _) = seeded_table(dir.path());
        let events = table.events.lock().expect("the event lock");

        let writer = {
            let table = Arc::clone(&table);
            std::thread::spawn(move || {
                table.update_layout("workspace-1", LayoutDoc::empty(), 2, SubscriberId::next())
            })
        };
        std::thread::sleep(REACHED_THE_LOCK);
        assert!(
            table.workspaces.try_lock().is_err(),
            "a later revision can land before this revision is broadcast"
        );

        drop(events);
        writer
            .join()
            .expect("the writer thread")
            .expect("the layout update");
    }

    /// Workspace removal is not announced after the transaction releases the
    /// workspace lock: a create in that gap would recreate the workspace and
    /// then be hidden by the stale removal event.
    #[test]
    fn kill_workspace_holds_the_workspace_lock_until_removal_is_published() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let (table, _) = seeded_table(dir.path());
        let events = table.events.lock().expect("the event lock");

        let killer = {
            let table = Arc::clone(&table);
            std::thread::spawn(move || table.kill_workspace("workspace-1"))
        };
        std::thread::sleep(REACHED_THE_LOCK);
        assert!(
            table.workspaces.try_lock().is_err(),
            "a workspace can be recreated before its stale removal is published"
        );

        drop(events);
        killer
            .join()
            .expect("the killer thread")
            .expect("the workspace kill");
    }

    /// `persist` is serialized *end to end*, not just over its snapshot: the
    /// write happens under the same guard. Two persists that only serialized
    /// their snapshots could still reach `save` in the opposite order and
    /// leave the older pair on disk.
    #[test]
    fn persist_writes_under_the_guard_it_snapshots_under() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let (table, _) = seeded_table(dir.path());
        let guard = table.persisting.lock().expect("the persist guard");

        let (done, finished) = std::sync::mpsc::channel();
        let persister = {
            let table = Arc::clone(&table);
            std::thread::spawn(move || {
                table.persist().expect("persisting");
                let _ = done.send(());
            })
        };
        assert!(
            finished.recv_timeout(REACHED_THE_LOCK).is_err(),
            "persist reached the write without holding the guard"
        );

        drop(guard);
        finished
            .recv_timeout(Duration::from_secs(5))
            .expect("persist runs once the guard is free");
        persister.join().expect("the persist thread");
    }

    /// The regression this scanner exists for: Debian's default bash `PS1`
    /// sets the window title on every prompt, and its BEL is a terminator.
    #[test]
    fn a_title_sequence_is_not_a_bell() {
        assert!(!bell_after(&[b"\x1b]0;kingii@rasp0: ~/dir\x07"]));
        assert!(!bell_after(&[b"\x1b]0;kingii@rasp0: ~/dir\x07\x1b[?2026l"]));
    }

    #[test]
    fn a_bell_in_ordinary_output_is_a_bell() {
        assert!(bell_after(&[b"hello\x07"]));
    }

    /// The pty read loop chunks at [`super::DRAIN_CHUNK_BYTES`], so the title
    /// sequence above routinely arrives in two pieces.
    #[test]
    fn a_control_string_split_across_chunks_is_still_one_string() {
        assert!(!bell_after(&[b"\x1b]0;tit"]));
        assert!(!bell_after(&[b"\x1b]0;tit", b"le\x07"]));
    }

    /// ST closes the string, so the *next* BEL is back in ground state.
    #[test]
    fn a_bell_after_a_string_terminator_rings() {
        assert!(!bell_after(&[b"\x1b]0;title\x1b\\"]));
        assert!(bell_after(&[b"\x1b]0;title\x1b\\", b"\x07"]));
    }

    #[test]
    fn a_bare_bell_after_a_title_in_the_same_chunk_rings() {
        assert!(bell_after(&[b"\x1b]0;t\x07 then \x07"]));
    }

    /// "Sticky until the next output after the bell" — unchanged by the scan.
    #[test]
    fn plain_output_after_a_bell_clears_it() {
        assert!(bell_after(&[b"\x07"]));
        assert!(!bell_after(&[b"\x07", b"more output\n"]));
    }

    #[test]
    fn shell_names_are_classified_by_comm() {
        for shell in ["sh", "bash", "zsh", "fish", "dash", "ksh"] {
            assert!(is_shell_name(shell), "{shell}");
            // `/proc/<pid>/comm` comes back with its trailing newline.
            assert!(is_shell_name(&format!("{shell}\n")), "{shell}");
            assert!(is_shell_name(&format!("-{shell}")), "login {shell}");
        }
    }

    #[test]
    fn agent_processes_are_not_shells() {
        for name in ["claude", "cat", "sleep", "node", "python3", "shell", ""] {
            assert!(!is_shell_name(name), "{name}");
        }
    }

    #[test]
    fn a_session_is_told_it_runs_in_a_color_terminal() {
        // The daemon is rarely started from a terminal itself, so a session
        // that inherited its environment unamended would print for a teletype.
        let env = terminal_env(&[("PAGER".into(), "less".into())]);
        assert!(env.contains(&("TERM".into(), "xterm-256color".into())));
        assert!(env.contains(&("COLORTERM".into(), "truecolor".into())));
        assert!(env.contains(&("PAGER".into(), "less".into())));
    }

    #[test]
    fn a_request_that_names_its_own_terminal_type_wins() {
        let env = terminal_env(&[("TERM".into(), "screen".into())]);
        assert_eq!(
            env.iter().filter(|(name, _)| name == "TERM").count(),
            1,
            "one TERM, the request's: {env:?}"
        );
        assert!(env.contains(&("TERM".into(), "screen".into())));
    }

    #[test]
    fn login_shell_prefers_the_environment_then_passwd_then_sh() {
        assert_eq!(
            login_shell_from(Some("/usr/bin/fish".into()), || Some("/bin/zsh".into())),
            "/usr/bin/fish"
        );
        // An empty $SHELL is no answer at all — daemons started without a
        // login environment get one.
        assert_eq!(
            login_shell_from(Some(String::new()), || Some("/bin/zsh".into())),
            "/bin/zsh"
        );
        assert_eq!(
            login_shell_from(None, || Some("/bin/zsh".into())),
            "/bin/zsh"
        );
        assert_eq!(login_shell_from(None, || None), shell());
    }

    #[cfg(unix)]
    #[test]
    fn passwd_shell_reads_the_seventh_field_of_the_matching_user() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\n\
                      kingii:x:1000:1000:Kingii,,,:/home/kingii:/usr/bin/zsh\n\
                      noshell:x:1001:1001::/home/noshell:\n\
                      truncated:x:1002:1002::/home/truncated\n";
        assert_eq!(
            super::passwd_shell(passwd, "kingii").as_deref(),
            Some("/usr/bin/zsh")
        );
        assert_eq!(
            super::passwd_shell(passwd, "root").as_deref(),
            Some("/bin/bash")
        );
        // Empty, short and absent entries are all "no answer".
        assert_eq!(super::passwd_shell(passwd, "noshell"), None);
        assert_eq!(super::passwd_shell(passwd, "truncated"), None);
        assert_eq!(super::passwd_shell(passwd, "nobody"), None);
    }

    /// A decoy process in its own group, standing in for a stranger that
    /// inherited a recycled pid.
    #[cfg(unix)]
    #[allow(clippy::disallowed_methods, reason = "a synchronous test thread")]
    fn decoy_group() -> std::process::Child {
        use std::os::unix::process::CommandExt as _;
        std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .expect("spawning the decoy")
    }

    /// A row whose child was already reaped gets no group signal at all: its
    /// pid may belong to someone else by now.
    #[cfg(unix)]
    #[test]
    fn terminate_group_stands_down_for_a_reaped_child() {
        let mut decoy = decoy_group();
        let activity = Arc::new(Activity::new());
        activity.mark_dead();
        terminate_group(&"a reaped session", Some(decoy.id()), activity);

        // Past the escalation deadline; a SIGKILL would have landed by now.
        std::thread::sleep(KILL_GRACE + Duration::from_millis(500));
        assert!(
            decoy.try_wait().expect("polling the decoy").is_none(),
            "a reaped row's signals reached a live group"
        );
        decoy.kill().expect("ending the decoy");
        decoy.wait().expect("reaping the decoy");
    }

    /// The counterpart that gives the stand-down test its teeth: with the
    /// child unreaped, the same call does signal the group.
    #[cfg(unix)]
    #[test]
    fn terminate_group_signals_an_unreaped_group() {
        let mut decoy = decoy_group();
        terminate_group(
            &"a live session",
            Some(decoy.id()),
            Arc::new(Activity::new()),
        );

        // `sleep`'s default SIGHUP disposition is to die; no grace needed.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if decoy.try_wait().expect("polling the decoy").is_some() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the group signal never reached an unreaped group"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
