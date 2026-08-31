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
//!   [`terminate_groups`]. Removing a row the daemon could not reach again is
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
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ade_session::proto::{
    Frame, LayoutDoc, LayoutNode, SessionId, SessionInfo, SessionStatus, Tab, WorkspaceInfo,
    error_code,
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
/// [`terminate_groups`] stops asking and sends `SIGKILL`.
///
/// A second is a shell's or an agent's whole `SIGHUP` cleanup window. `Kill`
/// waits for the escalation so a replacement cannot race a process that still
/// holds the old session's files or locks.
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

    /// The child has been waited on (or, on Windows, its exit was observed),
    /// so its pid may already belong to someone else — the kill paths' reason
    /// to stand down, and a lost row's only evidence that its terminal really
    /// is gone. See `keeps_a_live_terminal`.
    fn is_dead(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).dead
    }

    /// `(last output, bell pending, child dead)`.
    fn snapshot(&self) -> (Instant, bool, bool) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (state.last_output, state.bell, state.dead)
    }
}

/// What one queued frame costs the bound: its payload, plus a flat charge for
/// being a frame at all. The flat part keeps a flood of *empty* frames — status
/// events, one per session per sweep — bounded too, and is deliberately generous
/// next to the JSON an envelope really costs. Only the two frames that carry a
/// session's bytes have a payload worth counting.
fn frame_bytes(frame: &Frame) -> u64 {
    /// Enough that the byte bound also caps the frame count at a few thousand.
    const OVERHEAD: u64 = 256;

    let payload = match frame {
        Frame::Output { bytes, .. } | Frame::Replay { bytes, .. } => bytes.len() as u64,
        _ => 0,
    };
    payload + OVERHEAD
}

/// Headroom the attach replay leaves in the outbound bound, so the ending
/// frame and the first live output still fit behind a full replay — without
/// it, serving the replay would close the connection it just caught up. It
/// also absorbs the replay frame's own flat overhead, which the budget math
/// does not count.
const ATTACH_RESERVE_BYTES: u64 = 64 * 1024;

/// One connection's outbound queue: the frames waiting for its writer task, and
/// the byte count that bounds them.
///
/// **Bounded in bytes, not in frames.** A frame count only bounds memory if
/// every frame is the same size, and how much output one frame carries is the
/// transport's business, not this queue's. 2 MiB is the point past which a
/// reader can only be caught up by a repaint anyway.
///
/// Which is also why the channel underneath is unbounded in messages: the
/// accounting here is the bound, and a push must never block and never await,
/// because publishers hold session locks while they push and a pty that stops
/// being drained is a child that stops running.
#[derive(Clone)]
pub struct Outbound {
    frames: Sender<Frame>,
    /// Bytes queued and not yet written, shared with [`OutboundQueue`], which
    /// is what gives them back.
    queued: Arc<AtomicU64>,
    max_bytes: u64,
}

/// The receiving half of an [`Outbound`], for one connection's writer task.
pub struct OutboundQueue {
    frames: smol::channel::Receiver<Frame>,
    queued: Arc<AtomicU64>,
}

impl Outbound {
    pub fn new(max_bytes: u64) -> (Self, OutboundQueue) {
        let (frames, receiver) = smol::channel::unbounded();
        let queued = Arc::new(AtomicU64::new(0));
        (
            Self {
                frames,
                queued: queued.clone(),
                max_bytes,
            },
            OutboundQueue {
                frames: receiver,
                queued,
            },
        )
    }

    /// Queue one frame, and say whether that connection is still worth keeping.
    ///
    /// **The one delivery rule** for every fan-out in this module, because each
    /// runs on a thread that must not block: a pty drain thread, the sweeper, or
    /// a request holding the session lock. Queued → the connection stays; closed
    /// → it was already gone; **over the bound** → the peer really has stopped
    /// reading, a whole default scrollback behind, so the queue is *closed* and
    /// the subscription dropped. Closing ends the whole connection and not just
    /// this subscription: the writer task stops on a closed queue, and the
    /// request loop's next send fails and breaks.
    pub fn push(&self, frame: Frame) -> bool {
        // Non-request-scoped errors are unsolicited — not the answer to one
        // frame the peer sent — and per `error_code::is_request_scoped` that
        // is exactly the kind that ends whatever this connection was doing
        // (an attach included). Request-scoped ones are routine per-request
        // replies and stay quiet.
        if let Frame::Error {
            code,
            message,
            session_id,
            workspace_id,
            ..
        } = &frame
            && !error_code::is_request_scoped(code)
        {
            let subject = session_id
                .as_ref()
                .map(|id| format!("session {id}"))
                .or_else(|| workspace_id.as_ref().map(|id| format!("workspace {id}")))
                .unwrap_or_else(|| "no subject".to_owned());
            log::warn!("sending {code} to a client ({subject}): {message}");
        }
        let cost = frame_bytes(&frame);
        // Reserve, then send. A concurrent push may overshoot the bound by one
        // frame between the two, which is why this is `>` on the total rather
        // than an exact fill: the point is a ceiling on memory, not a quota.
        let before = self.queued.fetch_add(cost, Ordering::SeqCst);
        if before + cost > self.max_bytes {
            self.queued.fetch_sub(cost, Ordering::SeqCst);
            log::warn!(
                "a client is {before} byte(s) behind on its outbound queue; dropping its \
                 connection"
            );
            self.frames.close();
            return false;
        }
        if self.frames.try_send(frame).is_err() {
            // Unbounded, so the only failure is a queue somebody already closed.
            self.queued.fetch_sub(cost, Ordering::SeqCst);
            return false;
        }
        true
    }

    /// Queue one frame only if `reserve` bytes are still free behind it, and
    /// say whether it went. A refusal costs the connection nothing — it is for
    /// the frames this daemon synthesized, which a client can live without.
    ///
    /// Reserve and frame are accounted in one compare-and-swap on purpose, and
    /// **a reservation is never visible unless it commits**: reading
    /// [`Self::free_bytes`] and then pushing lets a concurrent push land
    /// between the two, and adding the cost only to take it back on refusal is
    /// worse still — a concurrent [`Self::push`] that reads the inflated count
    /// overruns the bound and *closes* the connection over bytes this call was
    /// never going to queue.
    fn try_push(&self, frame: Frame, reserve: u64) -> bool {
        let cost = frame_bytes(&frame);
        self.queued
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |queued| {
                (queued + cost + reserve <= self.max_bytes).then_some(queued + cost)
            })
            .is_ok()
            && (self.frames.try_send(frame).is_ok() || {
                // Unbounded, so the only failure is a queue somebody closed.
                self.queued.fetch_sub(cost, Ordering::SeqCst);
                false
            })
    }

    /// The room left under the bound right now — what an attach budgets its
    /// replay against. Reading it beats assuming an empty queue: a multiplexed
    /// connection may already hold another session's replay.
    fn free_bytes(&self) -> u64 {
        self.max_bytes
            .saturating_sub(self.queued.load(Ordering::SeqCst))
    }

    /// Whether this connection's queue has already been closed on. [`Self::push`]
    /// answers it as a side effect; the synthetic paths on [`Self::try_push`]
    /// have to ask, since a refusal there says nothing about the connection.
    fn is_closed(&self) -> bool {
        self.frames.is_closed()
    }
}

impl OutboundQueue {
    /// Everything queued so far, without waiting for more. The shape a test
    /// drains with; a writer task always wants [`Self::recv`].
    #[cfg(test)]
    pub(crate) fn try_recv(&self) -> Option<Frame> {
        let frame = self.frames.try_recv().ok()?;
        self.queued.fetch_sub(frame_bytes(&frame), Ordering::SeqCst);
        Some(frame)
    }

    /// The next frame to write, or `None` once the connection's last
    /// [`Outbound`] is gone and the queue has drained.
    pub async fn recv(&self) -> Option<Frame> {
        let frame = self.frames.recv().await.ok()?;
        self.queued.fetch_sub(frame_bytes(&frame), Ordering::SeqCst);
        Some(frame)
    }
}

/// The connections that asked for the event stream.
///
/// Same shape as [`OutputHub`]'s subscriber list and for the same reason: a
/// send that does not land means that connection is gone or is going, so it is
/// dropped here — see [`deliver`]. Event subscription and output attachment are
/// independent — a connection may be either, both, or neither.
#[derive(Default)]
struct EventHub {
    subscribers: Vec<(SubscriberId, Outbound)>,
}

impl EventHub {
    fn publish(&mut self, frame: &Frame) {
        self.subscribers
            .retain(|(_, outbound)| outbound.push(frame.clone()));
    }

    /// Subscribing twice is idempotent: the second call replaces the first
    /// registration rather than doubling the fan-out.
    fn subscribe(&mut self, subscriber: SubscriberId, outbound: &Outbound) {
        self.subscribers.retain(|(id, _)| *id != subscriber);
        self.subscribers.push((subscriber, outbound.clone()));
    }

    /// Publish to everyone but `except` — the client whose own request caused
    /// the event and which is answered directly instead.
    fn publish_except(&mut self, except: SubscriberId, frame: &Frame) {
        self.subscribers
            .retain(|(id, outbound)| *id == except || outbound.push(frame.clone()));
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

    /// The mutation applied and the ledger could not be written (§8.1). For a
    /// class-A mutation this reads "this happened and I could not record it" —
    /// the published events stand and the client must not try to undo them.
    pub fn persist_failed(message: impl Into<String>) -> Self {
        Self {
            code: error_code::PERSIST_FAILED,
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

    /// A request this build understood and cannot serve on this platform.
    ///
    /// [`error_code::DECLINED`] and not `unknown_op`: the op is known and the
    /// request well formed, so a client must not read it as "old daemon" and go
    /// deploy an upgrade. §2's "understood and not honoured", and
    /// request-scoped: the connection keeps serving every other op.
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self {
            code: error_code::DECLINED,
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
    /// Project root.
    pub root: String,
    /// `None` means the last component of `root`.
    pub name: Option<String>,
    pub project_id: Option<String>,
    pub project_identity: Option<String>,
    /// The id to record this workspace under. `None` mints one — the only
    /// thing a client ever gets. Set only by the generation-2 `create_session`
    /// auto-create, which must keep the id the old client already named.
    pub id: Option<String>,
}

/// Add a terminal tab to the first leaf in tree order — deterministic, so two
/// daemons converging on the same document put the tab in the same place.
fn append_terminal(node: &mut LayoutNode, session: &SessionId) {
    match node {
        LayoutNode::Leaf { tabs, .. } => tabs.push(Tab::Terminal {
            session_id: session.clone(),
        }),
        LayoutNode::Split { children, .. } => append_terminal(&mut children[0], session),
    }
}

/// A fresh record for `request`, unwritten and unannounced.
///
/// The name is trimmed and a blank one falls back to the root's basename at
/// **both** generations — a deliberate cross-generation hardening, docs §4.2.
fn new_workspace(request: WorkspaceRequest) -> WorkspaceInfo {
    WorkspaceInfo {
        id: request.id.unwrap_or_else(new_id),
        name: request
            .name
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| default_workspace_name(&request.root)),
        project_id: request.project_id,
        project_identity: request.project_identity,
        project_root: request.root,
        project_scope_rev: 0,
        created_at: now_unix(),
        // No document yet, and revision zero so the client's first layout
        // write — which must exceed the stored rev — is 1.
        layout_rev: 0,
        layout: LayoutDoc::empty(),
    }
}

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
struct Ring {
    bytes: VecDeque<u8>,
    capacity: usize,
    /// Set once the ring has dropped anything, and never cleared.
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

    /// The newest `limit` bytes, oldest first — the history an attach replays.
    /// Copied straight off the tail, so a large ring is never materialized
    /// whole just to be sliced.
    fn newest(&self, limit: usize) -> Vec<u8> {
        let skip = self.bytes.len().saturating_sub(limit);
        let (head, tail) = self.bytes.as_slices();
        let mut out = Vec::with_capacity(self.bytes.len() - skip);
        if skip < head.len() {
            out.extend_from_slice(&head[skip..]);
            out.extend_from_slice(tail);
        } else {
            out.extend_from_slice(&tail[skip - head.len()..]);
        }
        out
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }
}

/// One session's scrollback and screen, plus the connections streaming from it.
///
/// Ring, grid and subscriber list share a single mutex deliberately:
/// [`Self::attach`] builds the replay and registers the subscriber in one
/// critical section, while [`Self::publish`] records and fans out in another.
/// So no output can slip between the replay and the live stream.
struct OutputHub {
    ring: Ring,
    /// The screen these bytes have painted. `None` for a lost session, which
    /// has no pty and never will: painting a blank screen for it would assert
    /// something untrue, so it replays empty instead.
    grid: Option<SessionGrid>,
    /// The frame that ended this session's stream, if it has. A late attacher
    /// gets it right after the replay instead of waiting on a dead stream.
    ending: Option<Frame>,
    subscribers: Vec<(SubscriberId, Outbound)>,
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
    /// This runs on the pty drain thread, so it never waits on a client:
    /// [`Outbound::push`] queues or drops, and either way the pty keeps
    /// being read.
    fn publish(&mut self, session_id: &SessionId, mut chunk: &[u8]) {
        // History is raw pty bytes only — a synthesized repaint never enters it.
        self.ring.push(chunk);
        // At most one synthesis per call: an app toggling the alternate screen
        // dozens of times inside one read would otherwise pay a whole screen
        // per toggle, and only the last toggle repairs anything anyway. The
        // rest re-latch, so the repair rides the next splice point.
        let mut synthesized = false;
        let mut deferred = false;
        while !chunk.is_empty() {
            let boundary = self
                .grid
                .as_mut()
                .and_then(|grid| grid.feed_until_primary(chunk));
            let (prefix, suffix) = chunk.split_at(boundary.unwrap_or(chunk.len()));
            // At a splice point, repair a truncated replay's primary screen
            // without splitting a VT sequence — synthesized here, since a
            // detached session or an ordinary chunk must pay nothing for it.
            let spliced = match (boundary, self.grid.as_ref()) {
                (Some(_), Some(grid)) if !self.subscribers.is_empty() && !synthesized => {
                    synthesized = true;
                    Some([prefix, &grid.repaint()].concat())
                }
                // A later splice in the same chunk, with someone to repair for.
                (Some(_), Some(_)) => {
                    deferred |= !self.subscribers.is_empty();
                    None
                }
                _ => None,
            };
            self.subscribers.retain(|(_, outbound)| {
                // Prefix and repaint go as one frame: two would let a client
                // render the stale primary in between.
                if let Some(spliced) = spliced.as_ref()
                    && outbound.try_push(
                        Frame::Output {
                            session_id: session_id.clone(),
                            bytes: spliced.clone(),
                        },
                        ATTACH_RESERVE_BYTES,
                    )
                {
                    return true;
                }
                // No room for the repair: the prefix alone, so the client keeps
                // a stale screen but loses no pty byte. See [`Self::repaint`]
                // for why this one may not cost the connection.
                deferred |= spliced.is_some();
                outbound.push(Frame::Output {
                    session_id: session_id.clone(),
                    bytes: prefix.to_vec(),
                })
            });
            if boundary.is_none() {
                break;
            }
            chunk = suffix;
        }
        if deferred && let Some(grid) = self.grid.as_mut() {
            grid.defer_repair();
        }
    }

    /// Queue the replay and subscribe. Re-attaching replaces the previous
    /// subscription rather than doubling it.
    ///
    /// The retained ring is replayed first to restore scrollback, then a
    /// repaint synthesized from the screen repairs the visible rows at the
    /// current size — raw history renders right only at the width it was
    /// produced at, and a wrapped ring may open mid-escape; the repaint fixes
    /// what either garbles. History is capped so the replay leaves room under
    /// the outbound bound: blowing it would close the connection the replay
    /// was for.
    fn attach(&mut self, session_id: &SessionId, subscriber: SubscriberId, outbound: &Outbound) {
        self.subscribers.retain(|(id, _)| *id != subscriber);
        // An app killed mid-synchronized-update leaves the screen frozen with
        // no further chunk to end it; painting that would replay the app's
        // screen for a session that has left it.
        if let Some(grid) = self.grid.as_mut() {
            grid.flush_expired_sync();
            // Nobody else is waiting on the splice, and the replay below repaints
            // this client whole, so a latched repair has nothing left to repair.
            if self.subscribers.is_empty() {
                grid.take_pending_repair();
            }
        }
        let mut repaint = match self.grid.as_ref() {
            Some(grid) => grid.repaint(),
            None => Vec::new(),
        };
        let free = outbound.free_bytes();
        let mut dropped_repaint = false;
        if repaint.len() as u64 + ATTACH_RESERVE_BYTES > free {
            // A screen too large for the queue would close the connection this
            // replay is meant to catch up; what history fits still goes.
            repaint = Vec::new();
            dropped_repaint = true;
        }
        let budget = (outbound.max_bytes / 2)
            .min(free.saturating_sub(repaint.len() as u64 + ATTACH_RESERVE_BYTES))
            as usize;
        let mut bytes = self.ring.newest(budget);
        let capped = bytes.len() < self.ring.len();
        bytes.extend(repaint);
        let replay = Frame::Replay {
            session_id: session_id.clone(),
            bytes,
            // Only omission truncates: the ring dropped bytes, the cap cut the
            // replay, or the screen could not be painted. A screen that merely
            // scrolled is all still here.
            truncated: self.ring.truncated || capped || dropped_repaint,
        };
        if !outbound.push(replay) {
            return;
        }
        if let Some(ending) = self.ending.as_ref() {
            outbound.push(ending.clone());
        } else {
            self.subscribers.push((subscriber, outbound.clone()));
        }
    }

    /// Redraw the shared screen for every attached viewer — what a resize or a
    /// focus claim owes them; a queue that cannot take it loses its connection,
    /// unlike [`Self::publish`]'s splice, whose repaint is only a repair.
    fn repaint(&mut self, session_id: &SessionId) {
        if self.subscribers.is_empty() {
            return;
        }
        let Some(grid) = self.grid.as_mut() else {
            return;
        };
        // See [`Self::attach`]: an update nothing will end must not be painted.
        grid.flush_expired_sync();
        let bytes = grid.repaint();
        self.subscribers.retain(|(_, outbound)| {
            outbound.push(Frame::Output {
                session_id: session_id.clone(),
                bytes: bytes.clone(),
            })
        });
    }

    /// End a synchronized update whose deadline has passed, and hand out the
    /// repair it exposed. The one path that runs for a pty producing nothing:
    /// an app killed mid-update writes no further chunk for
    /// [`Self::publish`] to flush it on, so the sweep's cadence is what
    /// unfreezes the screen.
    ///
    /// The repaint is a frame of its own — between chunks there is no stream
    /// position to splice it into, and none is needed.
    fn flush_stalled(&mut self, session_id: &SessionId) {
        let Some(grid) = self.grid.as_mut() else {
            return;
        };
        grid.flush_expired_sync();
        if !grid.take_pending_repair() {
            return;
        }
        // With nobody attached the latch simply stays cleared: an attach
        // repaints the whole screen anyway, so there is nothing left to repair.
        if self.subscribers.is_empty() {
            return;
        }
        let bytes = grid.repaint();
        let mut skipped = false;
        self.subscribers.retain(|(_, outbound)| {
            // A closed queue is gone, not behind: re-latching for it would defer
            // the repair afresh on every sweep, forever.
            if outbound.is_closed() {
                return false;
            }
            skipped |= !outbound.try_push(
                Frame::Output {
                    session_id: session_id.clone(),
                    bytes: bytes.clone(),
                },
                ATTACH_RESERVE_BYTES,
            );
            true
        });
        if skipped && let Some(grid) = self.grid.as_mut() {
            grid.defer_repair();
        }
    }

    /// `true` if `subscriber` was actually attached here.
    fn detach(&mut self, subscriber: SubscriberId) -> bool {
        let before = self.subscribers.len();
        self.subscribers.retain(|(id, _)| *id != subscriber);
        self.subscribers.len() != before
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
            .retain(|(_, outbound)| outbound.push(frame.clone()));
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
    /// Last size applied by [`SessionTable::resize`].
    cols: u16,
    rows: u16,
    /// What each attached client last asked for. One pty, many clients: the pty
    /// holds the element-wise smallest ask, so no client renders a stream drawn
    /// for a grid larger than its own. Detaching gives the size back.
    ///
    /// **Generation-3 bookkeeping, and it never sees a generation-2 client.**
    /// Generation 2 has no per-subscriber ask: a resize there sets the pty
    /// directly ([`SessionTable::resize_legacy`]) and is recorded nowhere, so
    /// this table, `view_ids` and `focused_view` hold only gen-3 connections.
    /// Mixed attachments follow from that one rule — the minimum is over the
    /// gen-3 asks alone, a gen-2 resize overrides it until the next gen-3 ask,
    /// and a gen-2 detach has nothing to give back.
    sizes: Vec<(SubscriberId, (u16, u16))>,
    /// Which terminal view each attached client is the pty for, from its
    /// `Frame::Attach`. The only thing that can resolve [`Self::focused_view`]
    /// to a client, and therefore to an ask.
    view_ids: Vec<(SubscriberId, String)>,
    /// The view whose ask the pty follows verbatim, set by `Frame::FocusSession`
    /// and cleared only when that view's own client detaches. A view that has
    /// not attached — or has not asked — falls back to the minimum until it
    /// does; view ids are never reused, so a claim that never resolves is inert
    /// rather than wrong.
    focused_view: Option<String>,
}

impl Session {
    /// The size the pty should hold. `None` when nobody is asking — the current
    /// size then stands.
    ///
    /// The focused view's ask wins outright, because a user typing into one
    /// terminal wants that terminal's geometry and not the smallest sibling's.
    /// Everything else is the element-wise minimum, so no client renders a
    /// stream drawn for a grid larger than its own.
    fn effective_size(&self) -> Option<(u16, u16)> {
        if let Some(view) = self.focused_view.as_deref()
            && let Some((owner, _)) = self.view_ids.iter().find(|(_, id)| id == view)
            && let Some((_, size)) = self.sizes.iter().find(|(who, _)| who == owner)
        {
            return Some(*size);
        }
        self.sizes
            .iter()
            .map(|(_, size)| *size)
            .reduce(|(c, r), (c2, r2)| (c.min(c2), r.min(r2)))
    }

    /// Record which view a client is attaching on behalf of.
    fn remember_view(&mut self, subscriber: SubscriberId, view_id: &str) {
        self.view_ids.retain(|(who, _)| *who != subscriber);
        self.view_ids.push((subscriber, view_id.to_owned()));
    }

    /// Drop a departing client's ask and its view — including the focus, if it
    /// was the one holding it — and report the size the rest now need if it
    /// changed.
    fn forget_client(&mut self, subscriber: SubscriberId) -> Option<(u16, u16)> {
        let mut touched = false;
        if let Some(index) = self.view_ids.iter().position(|(who, _)| *who == subscriber) {
            let (_, view) = self.view_ids.swap_remove(index);
            if self.focused_view.as_deref() == Some(view.as_str()) {
                self.focused_view = None;
                touched = true;
            }
        }
        let before = self.sizes.len();
        self.sizes.retain(|(id, _)| *id != subscriber);
        touched |= self.sizes.len() != before;
        if !touched {
            return None;
        }
        let effective = self.effective_size()?;
        (effective != (self.cols, self.rows)).then_some(effective)
    }
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
    ///
    /// Also the serialization point for a class-B mutation (§8.3), which holds
    /// it across apply → persist → publish so that no second writer can
    /// supersede the record a failed write would roll back.
    persisting: Mutex<()>,
    /// Rows written to the ledger *before* their terminal was spawned, and not
    /// in `sessions` yet. Every persist merges them in, so a create running
    /// alongside another mutation cannot have its reservation swept off the
    /// disk during the window it exists to cover. See [`Self::reserve`].
    pending: Mutex<Vec<PersistedSession>>,
    /// Whether the ledger this table was built from was readable — see
    /// [`PersistedState::authoritative`]. Read by anything that would destroy
    /// a terminal for not being named in it.
    ledger_authoritative: bool,
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
    /// Test-only rendezvous: announce arrival, then wait to be let go. Armed by
    /// one test at a time, parked at [`Self::test_gate`]'s two call sites — a
    /// create spawned but not yet in the table, and a rename written but not
    /// yet published. Neither window is reachable by holding any of the locks
    /// above, since every one of them is taken and released inside it, so the
    /// only way to land a `kill_workspace` there deterministically is from
    /// inside.
    #[cfg(test)]
    test_gate: Mutex<
        Option<(
            std::sync::mpsc::SyncSender<()>,
            std::sync::mpsc::Receiver<()>,
        )>,
    >,
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
        let ledger_authoritative = previous.authoritative;
        if !previous.sessions.is_empty() {
            log::warn!(
                "{} session(s) from a previous daemon: reporting them as lost{}",
                previous.sessions.len(),
                "; a pty cannot be resurrected"
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
                        project_id: None,
                        project_identity: None,
                        project_root: persisted.cwd.clone(),
                        project_scope_rev: 0,
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
                    sizes: Vec::new(),
                    view_ids: Vec::new(),
                    focused_view: None,
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
            pending: Mutex::new(Vec::new()),
            ledger_authoritative,
            connections: std::sync::atomic::AtomicUsize::new(0),
            active_connections: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            test_gate: Mutex::new(None),
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

    /// The workspace a create names has to exist already: only
    /// [`Self::create_workspace`] makes a record, and nothing about a session
    /// makes one as a side effect.
    fn require_workspace(&self, id: &str) -> TableResult<()> {
        if id.is_empty() {
            return Err(TableError::invalid_argument(
                "a session needs the id of an existing workspace",
            ));
        }
        if !self
            .workspaces
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(id)
        {
            return Err(TableError::not_found(format!("no such workspace {id}")));
        }
        Ok(())
    }

    /// Spawn `request.command` on a fresh PTY and record the session.
    ///
    /// Refused outright unless `request.workspace_id` names a record this
    /// daemon holds — see [`Self::require_workspace`]. The check is not a
    /// guarantee: `kill_workspace` can still win the window that ends at
    /// [`Self::commit_created`], which is where the losing create is undone.
    ///
    /// Every other failure in here is the daemon's own — an `openpty` that
    /// failed, a command that would not spawn — so they all arrive as
    /// [`error_code::INTERNAL`] through [`TableError`]'s `anyhow` conversion.
    ///
    /// `async` and awaiting nothing: one signature for both platforms is what
    /// keeps this one table.
    pub async fn create(self: &Arc<Self>, request: CreateRequest) -> TableResult<SessionInfo> {
        self.require_workspace(&request.workspace_id)?;
        let workspace_id = request.workspace_id.clone();
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

        // §8.3 class C, plus the precondition silent-kill rests on: the ledger
        // row is on disk before there is a child that could outlive this
        // daemon. A reservation that cannot be written refuses the create
        // while refusing it is still free.
        let id = SessionId::new(new_id());
        self.reserve(Self::reservation(&id, &workspace_id, &request))?;
        let child = match pty.slave.spawn_command(command) {
            Ok(child) => child,
            Err(err) => {
                self.abandon_reservation(&id);
                return Err(err.context(format!("spawning {launched:?}")).into());
            }
        };
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
                self.abandon_reservation(&id);
                abandon(child);
                return Err(err.into());
            }
        };

        let (info, hub, activity) = self.record_created(
            id.clone(),
            workspace_id,
            request,
            Live {
                master: pty.master,
                writer: Arc::new(Mutex::new(writer)),
                killer,
                pid,
            },
        );
        if let Err(err) = self.commit_created(&info) {
            // Class C's compensation: a live session that is unpersisted, or
            // whose workspace was killed under it, is exactly the state a
            // restart cannot describe — and this one is empty.
            let _ = self.remove_session(&id).await;
            abandon(child);
            return Err(err);
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

    /// The ledger row a create writes before it spawns anything. Everything in
    /// it is known from the request.
    fn reservation(
        id: &SessionId,
        workspace_id: &str,
        request: &CreateRequest,
    ) -> PersistedSession {
        PersistedSession {
            id: id.clone(),
            workspace_id: workspace_id.to_owned(),
            agent_kind: request.agent_kind.clone(),
            instance_label: request.instance_label.clone(),
            cwd: request.cwd.clone(),
            created_at: now_unix(),
        }
    }

    /// The tail both [`create`](Self::create) halves share: from the platform's
    /// [`Live`] value to the published, persisted row.
    ///
    /// Nothing is announced here: §8.3 class C wants the ledger row down before
    /// any subscriber is told the session exists, so the events belong to
    /// [`Self::commit_created`].
    fn record_created(
        self: &Arc<Self>,
        id: SessionId,
        workspace_id: String,
        request: CreateRequest,
        live: Live,
    ) -> (SessionInfo, Arc<Mutex<OutputHub>>, Arc<Activity>) {
        #[cfg(test)]
        self.test_gate();
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

        let created_at = now_unix();
        let info = SessionInfo {
            id: id.clone(),
            workspace_id,
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

        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                id,
                Session {
                    info: info.clone(),
                    since: created_at,
                    lost: false,
                    live: Some(live),
                    hub: hub.clone(),
                    activity: activity.clone(),
                    cols: request.cols,
                    rows: request.rows,
                    sizes: Vec::new(),
                    view_ids: Vec::new(),
                    focused_view: None,
                },
            );
        (info, hub, activity)
    }

    /// Put a just-created session's row on disk and only then announce it.
    ///
    /// §8.3 class C: the requester's ack and every subscriber's `created` come
    /// after the write, so a failure here costs a session that is milliseconds
    /// old and empty instead of leaving a live terminal the ledger cannot
    /// describe. The caller compensates by killing it.
    ///
    /// The session lock is re-taken to publish: a kill that won the gap has
    /// already removed the row and published its `removed`, and announcing a
    /// `created` after that would describe a session that is gone.
    ///
    /// **The workspace is rechecked here, and this is the only place it can
    /// be.** [`Self::require_workspace`] runs before the spawn, but a
    /// `kill_workspace` whose critical section falls between `reserve` and the
    /// insert above sweeps a session that is not in the map yet — so the record
    /// can be gone by now, and nothing re-creates it. That create loses:
    /// `NOT_FOUND` sends the caller down class C's compensation path, which
    /// kills the shell and takes the row back off the disk. The doomed row this
    /// persist just wrote is superseded by the compensating one.
    fn commit_created(&self, info: &SessionInfo) -> TableResult<()> {
        // The real row is in `sessions` now and supersedes the reservation.
        self.release(&info.id);
        self.persist().map_err(|err| {
            TableError::persist_failed(format!("could not record session {}: {err:#}", info.id))
        })?;
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        // Before the session check, because a `kill_workspace` takes both and
        // the workspace is the one that says so whichever side of the insert it
        // landed on.
        if !self
            .workspaces
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&info.workspace_id)
        {
            return Err(TableError::not_found(format!(
                "workspace {} was killed while session {} was starting",
                info.workspace_id, info.id
            )));
        }
        if !sessions.contains_key(&info.id) {
            return Ok(());
        }
        let mut events = self.events.lock().unwrap_or_else(|e| e.into_inner());
        events.publish(&Frame::Created {
            session: info.clone(),
            persisted: self.persisted(),
            request_id: None,
        });
        Ok(())
    }

    /// Create a workspace record — that alone, with no session and no layout.
    ///
    /// What a panel row is before anything is put in it; a workspace with no
    /// sessions is a normal state. Nothing spawns, so nothing about a terminal
    /// is involved. The row's first terminal is a separate [`Self::create`]
    /// naming this id.
    pub fn create_workspace(&self, request: WorkspaceRequest) -> TableResult<WorkspaceInfo> {
        self.commit_workspace(new_workspace(request), None)
    }

    /// Generation 2's auto-create: the record under the id the client named, or
    /// the one already there. `true` means this call made it.
    ///
    /// Check, insert, persist and publish are **one section under
    /// `persisting`**, which is what a `contains_key` guard alone cannot give:
    /// a row inserted before its write completed is a row a second requester
    /// would adopt and the first requester's failing persist would then remove
    /// out from under it. A reuse is therefore only ever returned after the
    /// creating write landed — and only the creator is told it created, since
    /// compensating a row someone else owns would take their sessions with it.
    ///
    /// An id-less request mints a fresh one and cannot collide, so it stays
    /// [`Self::create_workspace`].
    pub fn ensure_workspace(
        &self,
        request: WorkspaceRequest,
    ) -> TableResult<(WorkspaceInfo, bool)> {
        let Some(id) = request.id.clone() else {
            return self.create_workspace(request).map(|w| (w, true));
        };
        let record = new_workspace(request);
        let persisting = self.persisting.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = self
            .workspaces
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
        {
            return Ok((existing.clone(), false));
        }
        self.commit_workspace(record, Some(&persisting))
            .map(|w| (w, true))
    }

    /// Take back a workspace this request created and nothing has used.
    ///
    /// Compensation for a generation-2 create that could not be completed. Only
    /// *empty*, and emptiness is decided inside the removal's own critical
    /// section: between the create and the failure a concurrent request may
    /// have put a session in it, and that workspace is no longer this request's
    /// to remove. Nothing left to compensate is `Ok`, not an error.
    pub async fn remove_empty_workspace(&self, id: &str) -> TableResult<()> {
        match self.remove_workspace(id, true).await {
            Err(err) if err.code == error_code::NOT_FOUND => Ok(()),
            other => other,
        }
    }

    /// Put a new workspace's record on disk and only then announce it.
    ///
    /// §8.3 class C: nothing was spawned, so a write the ledger refuses is
    /// undone completely — the record goes back out of memory and the requester
    /// is told, and the compensating `workspace_removed` covers the one thing
    /// that cannot be taken back, a subscriber that saw the creation. A
    /// workspace absent from the ledger cannot be described after a restart,
    /// and nothing the user put there exists yet.
    ///
    /// The map is rechecked after the write because `kill_workspace` can take
    /// the record during it, and then this create lost: publishing `workspace`
    /// after that kill's `workspace_removed` would leave every subscriber a row
    /// nothing will ever remove again. **The ledger needs no repair in that
    /// case**: [`Self::persist`] snapshots the live map rather than a captured
    /// value, and both writes serialize on `persisting`, so whichever lands
    /// last still writes a map the removal has already left.
    ///
    /// `held` is `persisting` when the caller already has it, because its check
    /// and this write must be one section ([`Self::ensure_workspace`]). A plain
    /// create takes it inside the ledger write, as it always did.
    fn commit_workspace(
        &self,
        workspace: WorkspaceInfo,
        held: Option<&MutexGuard<'_, ()>>,
    ) -> TableResult<WorkspaceInfo> {
        self.workspaces
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(workspace.id.clone(), workspace.clone());
        let recorded = match held {
            Some(_) => self.persist_serialized(),
            None => self.persist(),
        };
        if let Err(err) = recorded {
            // Only what this create put there is taken back — a kill that got
            // there first has already published its own removal.
            let removed = self
                .workspaces
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&workspace.id)
                .is_some();
            if removed {
                self.events
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .publish(&Frame::WorkspaceRemoved {
                        workspace_id: workspace.id.clone(),
                        persisted: self.persisted(),
                        request_id: None,
                    });
            }
            return Err(TableError::persist_failed(format!(
                "could not record workspace {}: {err:#}",
                workspace.id
            )));
        }
        // Held across the publish, the way `kill_workspace` holds it across its
        // own: otherwise a kill in that gap publishes its removal first.
        let workspaces = self.workspaces.lock().unwrap_or_else(|e| e.into_inner());
        if !workspaces.contains_key(&workspace.id) {
            return Err(TableError::not_found(format!(
                "workspace {} was killed while it was being created",
                workspace.id
            )));
        }
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .publish(&Frame::Workspace {
                workspace: workspace.clone(),
                sessions: Vec::new(),
                persisted: self.persisted(),
                request_id: None,
            });
        Ok(workspace)
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
    /// On success the accepted layout is pushed to every subscriber but
    /// `writer` as [`Frame::LayoutChanged`]; the writer gets its own reply from
    /// [`crate::server`]. `None` excludes nobody, for the one write whose
    /// requester has no reply to learn the layout from — see
    /// [`Self::install_legacy_layout`]. Contrast [`Self::scrub_layout`], which
    /// always broadcasts to everyone: there the daemon decided and nobody is
    /// already holding the document.
    ///
    /// **§8.3 class B**: nothing outside this process has changed, so the order
    /// is strict — apply, persist, *then* publish. A write that cannot be
    /// recorded is rolled back and never announced, so nothing ever observed
    /// it. `persisting` is held across all three, which is what makes the
    /// rollback safe without a revision test: no other writer can have taken
    /// the record in between.
    pub fn update_layout(
        &self,
        id: &str,
        layout: LayoutDoc,
        rev: u64,
        writer: Option<SubscriberId>,
    ) -> TableResult<()> {
        let _persisting = self.persisting.lock().unwrap_or_else(|e| e.into_inner());
        let previous = {
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
            let previous = (
                std::mem::replace(&mut workspace.layout, layout.clone()),
                workspace.layout_rev,
            );
            workspace.layout_rev = rev;
            previous
        };
        if let Err(err) = self.persist_serialized() {
            let mut workspaces = self.workspaces.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(workspace) = workspaces.get_mut(id) {
                (workspace.layout, workspace.layout_rev) = previous;
            }
            return Err(TableError::persist_failed(format!(
                "could not record the layout of workspace {id}: {err:#}"
            )));
        }
        let changed = Frame::LayoutChanged {
            workspace_id: id.to_owned(),
            layout,
            rev,
            persisted: self.persisted(),
            request_id: None,
        };
        let mut events = self.events.lock().unwrap_or_else(|e| e.into_inner());
        match writer {
            Some(writer) => events.publish_except(writer, &changed),
            None => events.publish(&changed),
        }
        Ok(())
    }

    /// Put `session`'s tab in the workspace's layout, converging with whoever
    /// else is writing it.
    ///
    /// Generation 2 made the record, its first shell and its one-leaf layout
    /// one outcome, so a lost revision race is retried rather than reported:
    /// re-read, stop if the tab is already there, otherwise append it to the
    /// document that won and write at *its* rev + 1. **The concurrent document
    /// is never replaced** — a fresh one-leaf write would delete tabs a client
    /// already has on screen.
    ///
    /// Published to everyone, the requester included: at generation 2 the
    /// layout arrived inside the workspace event, so an old client that
    /// subscribed on the connection it created from has no reply to learn it
    /// from. The bound on the retries is a livelock guard, not a semantic — a
    /// workspace being rewritten this hard has a live writer and the loser's
    /// error is the honest answer.
    pub fn install_legacy_layout(
        &self,
        workspace_id: &str,
        session: &SessionId,
    ) -> TableResult<()> {
        /// Racing writers this converges with before giving up.
        const ATTEMPTS: usize = 8;
        let mut last = None;
        for _ in 0..ATTEMPTS {
            let (workspace, _) = self.open_workspace(workspace_id)?;
            if workspace.layout.terminal_sessions().contains(session) {
                return Ok(());
            }
            let mut layout = workspace.layout;
            append_terminal(&mut layout.root, session);
            match self.update_layout(workspace_id, layout, workspace.layout_rev + 1, None) {
                Err(err) if err.code == error_code::STALE_REV => last = Some(err),
                other => return other,
            }
        }
        Err(last.unwrap_or_else(|| {
            TableError::stale_rev(format!(
                "workspace {workspace_id} was rewritten while its first tab was being added"
            ))
        }))
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
        // Class B, exactly as [`Self::update_layout`]: apply, persist, publish,
        // all under `persisting` so a failed write can be rolled back onto a
        // record no other writer can have taken meanwhile.
        let _persisting = self.persisting.lock().unwrap_or_else(|e| e.into_inner());
        let (workspace, workspace_sessions, previous) = {
            let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let mut workspaces = self.workspaces.lock().unwrap_or_else(|e| e.into_inner());
            let workspace = workspaces
                .get_mut(id)
                .ok_or_else(|| TableError::not_found(format!("no such workspace {id}")))?;
            let previous = std::mem::replace(&mut workspace.name, name.to_owned());
            let workspace = workspace.clone();
            let workspace_sessions = workspace_sessions_in(&sessions, id);
            (workspace, workspace_sessions, previous)
        };
        if let Err(err) = self.persist_serialized() {
            let mut workspaces = self.workspaces.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(workspace) = workspaces.get_mut(id) {
                workspace.name = previous;
            }
            return Err(TableError::persist_failed(format!(
                "could not record the new name of workspace {id}: {err:#}"
            )));
        }
        #[cfg(test)]
        self.test_gate();
        // [`Self::commit_workspace`]'s recheck, for the same reason and held
        // across the publish the same way: a kill can take the record during
        // the write, and a `workspace` frame after its `workspace_removed`
        // leaves every subscriber a row nothing will remove again.
        let workspaces = self.workspaces.lock().unwrap_or_else(|e| e.into_inner());
        if !workspaces.contains_key(id) {
            return Err(TableError::not_found(format!(
                "workspace {id} was killed while it was being renamed"
            )));
        }
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .publish(&Frame::Workspace {
                workspace: workspace.clone(),
                sessions: workspace_sessions.clone(),
                persisted: self.persisted(),
                request_id: None,
            });
        Ok((workspace, workspace_sessions))
    }

    pub fn update_workspace_project(
        &self,
        id: &str,
        project_id: &str,
        project_identity: &str,
        project_root: Option<&str>,
        minimum_scope_rev: Option<u64>,
    ) -> TableResult<(WorkspaceInfo, Vec<SessionInfo>)> {
        if project_id.trim().is_empty()
            || project_identity.trim().is_empty()
            || project_root.is_some_and(|root| root.trim().is_empty())
        {
            return Err(TableError::invalid_argument(
                "workspace project scope cannot contain empty values",
            ));
        }

        let _persisting = self.persisting.lock().unwrap_or_else(|e| e.into_inner());
        let (workspace, workspace_sessions, previous) = {
            let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let mut workspaces = self.workspaces.lock().unwrap_or_else(|e| e.into_inner());
            let workspace = workspaces
                .get_mut(id)
                .ok_or_else(|| TableError::not_found(format!("no such workspace {id}")))?;
            let minimum_scope_rev = minimum_scope_rev.unwrap_or_default();
            let fields_changed = workspace.project_id.as_deref() != Some(project_id)
                || workspace.project_identity.as_deref() != Some(project_identity)
                || project_root.is_some_and(|root| workspace.project_root != root);
            if !fields_changed && workspace.project_scope_rev >= minimum_scope_rev {
                return Ok((workspace.clone(), workspace_sessions_in(&sessions, id)));
            }
            let next_revision = workspace
                .project_scope_rev
                .max(minimum_scope_rev)
                .checked_add(1)
                .ok_or_else(|| {
                    TableError::internal(format!(
                        "project scope revision overflowed for workspace {id}"
                    ))
                })?;
            let previous = (
                workspace.project_id.replace(project_id.to_owned()),
                workspace
                    .project_identity
                    .replace(project_identity.to_owned()),
                project_root
                    .map(|root| std::mem::replace(&mut workspace.project_root, root.to_owned())),
                std::mem::replace(&mut workspace.project_scope_rev, next_revision),
            );
            let workspace = workspace.clone();
            let workspace_sessions = workspace_sessions_in(&sessions, id);
            (workspace, workspace_sessions, previous)
        };
        if let Err(err) = self.persist_serialized() {
            let mut workspaces = self.workspaces.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(workspace) = workspaces.get_mut(id) {
                workspace.project_id = previous.0;
                workspace.project_identity = previous.1;
                if let Some(project_root) = previous.2 {
                    workspace.project_root = project_root;
                }
                workspace.project_scope_rev = previous.3;
            }
            return Err(TableError::persist_failed(format!(
                "could not record the project scope of workspace {id}: {err:#}"
            )));
        }
        #[cfg(test)]
        self.test_gate();
        let workspaces = self.workspaces.lock().unwrap_or_else(|e| e.into_inner());
        if !workspaces.contains_key(id) {
            return Err(TableError::not_found(format!(
                "workspace {id} was killed while its project scope was being updated"
            )));
        }
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .publish(&Frame::Workspace {
                workspace: workspace.clone(),
                sessions: workspace_sessions.clone(),
                persisted: self.persisted(),
                request_id: None,
            });
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
    /// this workspace can land its row after the sweep and before the removal,
    /// leaving a live session whose workspace was announced gone.
    ///
    /// A create still in flight is *not* covered by this section — its row is
    /// not in the map yet — so the loser is caught at
    /// [`Self::commit_created`] instead, which rechecks the record.
    ///
    /// Session rows and all removal events are committed under that same
    /// section. Their ptys are signalled only after the locks are released.
    ///
    /// [`error_code::NOT_FOUND`] if there was no such workspace and no session
    /// claiming it — killing the same workspace twice is an error, not a second
    /// removal.
    ///
    /// **The kills run after the critical section, not before it**, which is
    /// where this differs from [`Self::kill`]: a single session has nothing to
    /// race, but a `CreateSession` naming this workspace can land a new session
    /// between a kill pass and the section that removes the rows, leaving a child
    /// the table has just forgotten and nothing ever killed. Remove first, kill
    /// what was removed.
    pub async fn kill_workspace(&self, id: &str) -> TableResult<()> {
        self.remove_workspace(id, false).await
    }

    /// [`Self::kill_workspace`], with the emptiness test of
    /// [`Self::remove_empty_workspace`] inside the same critical section that
    /// decides what dies — the only place it cannot be raced.
    async fn remove_workspace(&self, id: &str, only_if_empty: bool) -> TableResult<()> {
        let mut removed = {
            let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let mut workspaces = self.workspaces.lock().unwrap_or_else(|e| e.into_inner());
            let doomed: Vec<SessionId> = sessions
                .values()
                .filter(|session| session.info.workspace_id == id)
                .map(|session| session.info.id.clone())
                .collect();
            if only_if_empty && !doomed.is_empty() {
                return Ok(());
            }
            let existed = workspaces.remove(id).is_some();
            if !existed && doomed.is_empty() {
                return Err(TableError::not_found(format!("no such workspace {id}")));
            }
            // A reservation naming this workspace goes with it, inside the same
            // section: the create that wrote it is still spawning, and the
            // persist below would otherwise leave a row whose workspace is gone
            // — which the next daemon's load migrates back into a workspace.
            // That create's own commit recheck answers NOT_FOUND and takes its
            // terminal back down.
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|record| record.workspace_id != id);
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
                persisted: self.persisted(),
                request_id: None,
            });
            removed
        };
        let terminations = removed
            .iter_mut()
            .filter_map(kill_session_process)
            .collect::<Vec<_>>();
        for termination in terminations {
            wait_for_termination(termination);
        }
        drop(removed);
        // Class A (§8.3): the children are dead and the events describing that
        // have gone out unconditionally, because reality does not roll back.
        // Only the ack can carry a failure to record it, so it does.
        self.persist().map_err(|err| {
            TableError::persist_failed(format!(
                "workspace {id} was killed and could not be recorded: {err:#}"
            ))
        })
    }

    /// Register `outbound` for the event stream and push the initial snapshot:
    /// one [`Frame::Status`] per session, including exited and lost ones.
    ///
    /// The snapshot is built and sent while the registration is in place and
    /// the session lock is held, so nothing can change status in the gap.
    /// Subscribing twice just resends the snapshot.
    pub fn subscribe(&self, subscriber: SubscriberId, outbound: &Outbound) {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let mut events = self.events.lock().unwrap_or_else(|e| e.into_inner());
        events.subscribe(subscriber, outbound);
        let mut snapshot: Vec<&Session> = sessions.values().collect();
        snapshot.sort_by(|a, b| {
            a.info
                .created_at
                .cmp(&b.info.created_at)
                .then(a.info.id.cmp(&b.info.id))
        });
        for session in snapshot {
            if !outbound.push(Frame::Status {
                session_id: session.info.id.clone(),
                status: session.info.status,
                since: session.since,
            }) {
                events.unsubscribe(subscriber);
                break;
            }
        }
    }

    /// Queue a [`Frame::Replay`] of the whole ring on `outbound`, then stream
    /// live [`Frame::Output`] there until detach or disconnect — followed by
    /// the [`Frame::Exited`] or [`Frame::Removed`] that ends the stream, so an
    /// attached client never has to subscribe just to learn its session is
    /// over (see [`OutputHub::publish_event`]).
    ///
    /// [`error_code::NOT_FOUND`] for an unknown session. Attaching to an exited
    /// or lost session succeeds and replays whatever the ring holds — that is
    /// how a crashed agent's last words stay readable.
    ///
    /// `view_id` is which terminal view this client draws — absent from an
    /// older client, and then this connection can never be a focus owner.
    pub async fn attach(
        &self,
        id: &SessionId,
        subscriber: SubscriberId,
        outbound: &Outbound,
        view_id: Option<&str>,
    ) -> TableResult<()> {
        let (hub, resize) = {
            let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            match sessions.get_mut(id) {
                Some(session) => {
                    let mut resize = None;
                    if let Some(view_id) = view_id {
                        session.remember_view(subscriber, view_id);
                        // The view may resolve a focus claim that was waiting
                        // for it; its ask then owns the pty from the first
                        // painted byte, not from the next resize.
                        if session.focused_view.as_deref() == Some(view_id) {
                            resize = session
                                .effective_size()
                                .filter(|&size| size != (session.cols, session.rows));
                        }
                    }
                    (session.hub.clone(), resize)
                }
                None => return Err(TableError::not_found(format!("no such session {id}"))),
            }
        };
        if let Some((cols, rows)) = resize {
            self.apply_size(id, cols, rows).await?;
        }
        hub.lock()
            .unwrap_or_else(|e| e.into_inner())
            .attach(id, subscriber, outbound);
        log::info!("session {id} attached (subscriber {subscriber})");
        Ok(())
    }

    /// Stop streaming this session to this connection. **Never touches the
    /// session** beyond giving back the size it was holding down — and the
    /// focus, if this was the view holding it — and detaching something that
    /// was not attached is a no-op.
    pub async fn detach(&self, id: &SessionId, subscriber: SubscriberId) {
        let (hub, grown) = {
            let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            match sessions.get_mut(id) {
                Some(session) => (session.hub.clone(), session.forget_client(subscriber)),
                None => return,
            }
        };
        hub.lock()
            .unwrap_or_else(|e| e.into_inner())
            .detach(subscriber);
        if let Some((cols, rows)) = grown {
            self.grow_back(id, cols, rows).await;
        }
    }

    /// Detach `subscriber` from every session and from the event stream. This
    /// is what a dropped connection does — and all it does.
    pub async fn detach_all(&self, subscriber: SubscriberId) {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .unsubscribe(subscriber);
        type Detaching = (SessionId, Arc<Mutex<OutputHub>>, Option<(u16, u16)>);
        let hubs: Vec<Detaching> = {
            let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            sessions
                .values_mut()
                .map(|session| {
                    (
                        session.info.id.clone(),
                        session.hub.clone(),
                        session.forget_client(subscriber),
                    )
                })
                .collect()
        };
        for (id, hub, grown) in hubs {
            let was_attached = hub
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .detach(subscriber);
            if was_attached {
                log::info!("session {id} attach ended (subscriber {subscriber} connection closed)");
            }
            if let Some((cols, rows)) = grown {
                self.grow_back(&id, cols, rows).await;
            }
        }
    }

    /// Hand the size back to the clients still attached. Nobody asked for this
    /// resize, so a session that cannot take it is a log line, not a refusal.
    /// Callers hold `sizing`: the grow-back decision is `forget_client`, made
    /// before this, and the two are one size operation.
    async fn grow_back(&self, id: &SessionId, cols: u16, rows: u16) {
        if let Err(error) = self.apply_size(id, cols, rows).await {
            log::warn!("session {id} could not grow back to {cols}x{rows} after a detach: {error}");
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
    ///
    /// **The lookup is the whole critical section and the write happens after
    /// it**: nothing that can block is done under the table lock.
    pub async fn write(&self, id: &SessionId, bytes: &[u8]) -> TableResult<()> {
        let target = {
            let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let session = sessions
                .get_mut(id)
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
        let mut writer = target.lock().unwrap_or_else(|e| e.into_inner());
        writer
            .write_all(bytes)
            .with_context(|| format!("writing to session {id}"))?;
        writer
            .flush()
            .with_context(|| format!("flushing session {id}"))?;
        Ok(())
    }

    /// Record what this client wants and hold the pty at the smallest ask.
    ///
    /// A size that changes nothing is not applied at all, so a second client
    /// mounting a larger split leaves the stream — and every other client's
    /// screen — exactly where it was.
    pub async fn resize(
        &self,
        id: &SessionId,
        subscriber: SubscriberId,
        cols: u16,
        rows: u16,
    ) -> TableResult<()> {
        let (effective, hub, size_changed) = {
            let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let session = sessions
                .get_mut(id)
                .ok_or_else(|| TableError::not_found(format!("no such session {id}")))?;
            let asked = (cols.max(1), rows.max(1));
            session.sizes.retain(|(who, _)| *who != subscriber);
            session.sizes.push((subscriber, asked));
            let effective = session.effective_size().unwrap_or(asked);
            log::info!(
                "session {id} ask from subscriber {subscriber}: {}x{}, effective {}x{}, pty {}x{}",
                asked.0,
                asked.1,
                effective.0,
                effective.1,
                session.cols,
                session.rows
            );
            let size_changed = effective != (session.cols, session.rows);
            (effective, session.hub.clone(), size_changed)
        };
        if size_changed {
            self.apply_size(id, effective.0, effective.1).await?;
        }
        hub.lock().unwrap_or_else(|e| e.into_inner()).repaint(id);
        Ok(())
    }

    /// The generation-2 resize: last request wins, applied straight to the pty.
    ///
    /// No ask is recorded, so this connection stays outside every gen-3 table
    /// (see [`Session::sizes`]) — including on detach, where there is nothing
    /// of its to give back.
    pub async fn resize_legacy(&self, id: &SessionId, cols: u16, rows: u16) -> TableResult<()> {
        let asked = (cols.max(1), rows.max(1));
        let (hub, size_changed) = {
            let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let session = sessions
                .get(id)
                .ok_or_else(|| TableError::not_found(format!("no such session {id}")))?;
            (session.hub.clone(), (session.cols, session.rows) != asked)
        };
        if size_changed {
            self.apply_size(id, asked.0, asked.1).await?;
        }
        hub.lock().unwrap_or_else(|e| e.into_inner()).repaint(id);
        Ok(())
    }

    /// Hand the pty to one view: its ask becomes the size, instead of the
    /// smallest ask among everyone attached.
    ///
    /// No size travels with the claim — the owner's own `resize` frames keep
    /// supplying it, and this only changes which of them the pty follows. A
    /// view nothing has attached with yet, or one that has not asked, leaves
    /// the minimum standing until it does.
    ///
    /// Same lock discipline as [`Self::resize`], and for the same reason: the
    /// decision is made under the table lock, the pty call is made after it.
    pub async fn focus(&self, id: &SessionId, view_id: &str, hover: bool) -> TableResult<()> {
        let (resize, repaint) = {
            let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let session = sessions
                .get_mut(id)
                .ok_or_else(|| TableError::not_found(format!("no such session {id}")))?;
            let already = session.focused_view.as_deref() == Some(view_id);
            session.focused_view = Some(view_id.to_owned());
            let owner = session
                .view_ids
                .iter()
                .find(|(_, id)| id == view_id)
                .map(|(owner, _)| *owner);
            let view_ask = owner
                .and_then(|owner| session.sizes.iter().find(|(who, _)| *who == owner))
                .map(|(_, size)| *size);
            let effective = session.effective_size();
            // Repeated claims from the size owner stay quiet; anything that
            // changes the owner or the size is worth a line.
            if !already || effective.is_some_and(|e| e != (session.cols, session.rows)) {
                log::info!(
                    "session {id} claim by view {view_id}: view ask {view_ask:?}, effective {effective:?}, pty {}x{}",
                    session.cols,
                    session.rows
                );
            }
            let Some(effective) = effective else {
                return Ok(());
            };
            let size_changed = effective != (session.cols, session.rows);
            let repaint = (hover || !already || size_changed).then(|| session.hub.clone());
            (size_changed.then_some(effective), repaint)
        };
        if let Some((cols, rows)) = resize {
            self.apply_size(id, cols, rows).await?;
        }
        if let Some(hub) = repaint {
            hub.lock().unwrap_or_else(|e| e.into_inner()).repaint(id);
        }
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
    async fn apply_size(&self, id: &SessionId, cols: u16, rows: u16) -> TableResult<()> {
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
    pub async fn kill(&self, id: &SessionId) -> TableResult<()> {
        match self.remove_session(id).await {
            None => Err(TableError::not_found(format!("no such session {id}"))),
            // Class A (§8.3): `removed` is already out and the child is already
            // dead, so this error means "it happened and I could not record
            // it" — never "try again".
            Some(persisted) => persisted.map_err(|err| {
                TableError::persist_failed(format!(
                    "session {id} was killed and could not be recorded: {err:#}"
                ))
            }),
        }
    }

    /// [`Self::kill`]'s body, minus the classification.
    ///
    /// Returns `false` if there is no such session.
    ///
    /// Lookup and removal are two acquisitions of the session lock with the kill
    /// between them, because nothing that can block may be done under it. A
    /// second `kill` that wins the gap removes the row and this one answers
    /// [`error_code::NOT_FOUND`] — what killing an already killed session means.
    ///
    /// `None` for a session that was not there; otherwise whether the removal
    /// reached the ledger, which is the caller's to classify.
    async fn remove_session(&self, id: &SessionId) -> Option<Result<()>> {
        // `mut` for the unix killer below, which a Windows build does not have.
        #[cfg_attr(windows, allow(unused_mut))]
        let mut session = {
            let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let Some(session) = sessions.remove(id) else {
                return None;
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
        if let Some(termination) = kill_session_process(&mut session) {
            wait_for_termination(termination);
        }
        drop(session);
        // A reservation for this id, if the create that made it is being
        // compensated, must not survive the removal it is being compensated
        // for.
        self.release(id);
        Some(self.persist())
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
                persisted: self.persisted(),
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

    /// Re-derive every session's status and push the ones that changed, and
    /// unfreeze any screen stuck in a synchronized update — see
    /// [`OutputHub::flush_stalled`].
    ///
    /// Called by the sweeper thread on [`StatusConfig::sweep_interval`]. This
    /// is the *only* writer of `info.status` after creation, which is what
    /// makes [`derive_status`] the single definition of ADE's status dots.
    pub fn sweep(&self) {
        let now = Instant::now();
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let mut changed = Vec::new();
        for session in sessions.values_mut() {
            // Lock order is sessions then hub, as everywhere else here.
            session
                .hub
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .flush_stalled(&session.info.id);
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

    /// Was the ledger behind this table readable? See
    /// [`PersistedState::authoritative`]: `false` means "these rows are not
    /// the whole truth", and nothing may be killed for being absent from them.
    pub fn ledger_authoritative(&self) -> bool {
        self.ledger_authoritative
    }

    /// What every mutation ack and every event this daemon publishes claims
    /// about durability (§8.5): false while [`StateStore::save`] is a no-op —
    /// a newer-schema ledger or one this daemon could not read — true
    /// otherwise. Nothing clears either condition, so this is constant for the
    /// process.
    pub fn persisted(&self) -> bool {
        !self.state.read_only()
    }

    /// Wait until this table is the whole truth about the sessions this daemon
    /// serves.
    ///
    /// Every frame that reads or mutates the table waits here first; the
    /// handshake deliberately does not — a client's connect budget is a process
    /// start, never a replay. Returns at once when nothing is rehydrating, which
    /// is every daemon here.
    pub async fn ready(&self) {}

    /// Write every workspace, and the metadata of every session whose terminal
    /// may still exist. Lost session rows are excluded on purpose (see
    /// [`SessionTable::load`]) — except the ones `keeps_a_live_terminal`
    /// vouches for. Workspaces are never excluded, because a workspace outlives
    /// the sessions in it.
    ///
    /// Serialized end to end by `persisting`, both tables snapshotted under
    /// **one** acquisition — sessions then workspaces, the order the note on the
    /// type gives. Both matter: separate acquisitions pair sets that never
    /// coexisted, and writing outside the guard lets a slower persist land last.
    fn persist(&self) -> Result<()> {
        let _persisting = self.persisting.lock().unwrap_or_else(|e| e.into_inner());
        self.persist_serialized()
    }

    /// [`Self::persist`]'s body, for the callers that already hold
    /// `persisting` because they need the write to be part of a longer
    /// critical section.
    fn persist_serialized(&self) -> Result<()> {
        let (records, workspaces) = {
            let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let workspaces = self.workspaces.lock().unwrap_or_else(|e| e.into_inner());
            let mut records: Vec<PersistedSession> = sessions
                .values()
                .filter(|session| !session.lost || keeps_a_live_terminal(session))
                .map(|session| PersistedSession::from_info(&session.info))
                .collect();
            // Reserved rows whose terminal is being spawned right now. A leaf
            // lock, taken under the two above and never the other way round.
            let pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            records.extend(
                pending
                    .iter()
                    .filter(|record| !sessions.contains_key(&record.id))
                    .cloned(),
            );
            records.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
            let mut infos: Vec<WorkspaceInfo> = workspaces.values().cloned().collect();
            infos.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
            (records, infos)
        };
        self.state.save(&records, &workspaces)
    }

    /// Write a session's ledger row *before* its terminal is spawned.
    ///
    /// **A shell must never exist without a row.** A daemon that dies between
    /// the spawn and the write leaves a terminal nothing can name, and a
    /// terminal nothing can name is one the next daemon has to treat as a
    /// stranger. So the row goes down first and the spawn follows it; the
    /// reservation is withdrawn again if the spawn never happens.
    ///
    /// A create whose reservation cannot be written is refused *before*
    /// anything is spawned, which is the one create failure that costs nothing.
    fn reserve(&self, record: PersistedSession) -> TableResult<()> {
        let id = record.id.clone();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(record);
        self.persist().map_err(|err| {
            self.release(&id);
            TableError::persist_failed(format!("could not reserve a ledger row: {err:#}"))
        })
    }

    /// Drop a reservation, without writing. The caller decides whether the
    /// removal has to reach the disk: on the success path the create's own
    /// persist writes the real row over it, on the failure path
    /// [`Self::abandon_reservation`] writes the row away.
    fn release(&self, id: &SessionId) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|record| &record.id != id);
    }

    /// Park here until the test that armed `test_gate` lets go; a no-op when
    /// none did, which is every other test and every real daemon.
    #[cfg(test)]
    fn test_gate(&self) {
        if let Some((arrived, go)) = self
            .test_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            let _ = arrived.send(());
            let _ = go.recv();
        }
    }

    /// A spawn that never happened: take the reserved row off the disk again.
    fn abandon_reservation(&self, id: &SessionId) {
        self.release(id);
        if let Err(err) = self.persist() {
            log::warn!("could not withdraw the ledger row reserved for {id}: {err:#}");
        }
    }
}

/// A lost row on unix is a tombstone: the pty died with the daemon that owned
/// it, and there is nothing left for a ledger row to name.
fn keeps_a_live_terminal(_session: &Session) -> bool {
    false
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
///
/// Unix only, because only a unix `Live` has a master to ask. Windows skips the
/// rule at the call site instead; see [`derive_status`].
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
/// [`terminate_groups`].
fn kill_session_process(session: &mut Session) -> Option<std::thread::JoinHandle<()>> {
    let id = session.info.id.clone();
    let pid = session.live.as_ref().and_then(|live| live.pid);
    #[cfg(unix)]
    let foreground_pid = session
        .live
        .as_ref()
        .and_then(|live| live.master.process_group_leader())
        .and_then(|pid| u32::try_from(pid).ok());
    #[cfg(not(unix))]
    let foreground_pid = None;
    let activity = session.activity.clone();
    let termination = terminate_groups(&id, [pid, foreground_pid], activity.clone());
    if activity.is_dead() {
        // Reaped before this kill began.
    } else if let Some(live) = session.live.as_mut()
        && let Err(err) = live.killer.kill()
    {
        // Already dead is the common case here, not a failure.
        log::debug!("killing {id}: {err}");
    }
    termination
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
/// So both the shell's group and the pty's current foreground group get a
/// `SIGHUP`, and after [`KILL_GRACE`] a `SIGKILL`, which nothing can trap.
/// `Kill` waits for that thread before acknowledging the request. A row whose
/// child was already reaped gets neither signal: its pid may have been
/// recycled, and the group signals would land on a stranger.
///
/// The child is a session leader (`portable-pty` calls `setsid` before `exec`),
/// so its pid *is* its process-group id. Interactive shells put foreground
/// jobs such as Codex in a different group within the same session, which is
/// why the pty's foreground group is included too. A descendant that called
/// `setsid` itself is beyond any signal we could send; only a cgroup would
/// follow it there.
fn terminate_groups(
    label: &dyn std::fmt::Display,
    pids: [Option<u32>; 2],
    activity: Arc<Activity>,
) -> Option<std::thread::JoinHandle<()>> {
    #[cfg(unix)]
    {
        // SAFETY: `getpgrp` only reads the calling process's group id.
        let own_group = unsafe { libc::getpgrp() };
        let mut groups = pids
            .into_iter()
            .flatten()
            .filter_map(|pid| libc::pid_t::try_from(pid).ok())
            .filter(|pid| *pid > 0 && *pid != own_group)
            .collect::<Vec<_>>();
        groups.sort_unstable();
        groups.dedup();
        let first_group = groups.first().copied()?;
        // Once the reaper has waited on the child, the kernel may hand its pid
        // to anyone — signalling `-pid` could kill an unrelated group. So a
        // row whose child was already reaped (killed long after it exited)
        // gets no group signal at all; a descendant that survived its leader
        // this long was deliberate. An unreaped leader keeps the pid reserved,
        // which makes the signals below safe.
        if activity.is_dead() {
            return None;
        }
        for group in &groups {
            signal_group(*group, libc::SIGHUP);
        }
        let label = label.to_string();
        let fallback_groups = groups.clone();
        let escalate = std::thread::Builder::new()
            .name(format!("ade-kill-{first_group}"))
            .spawn(move || {
                std::thread::sleep(KILL_GRACE);
                for group in &groups {
                    if signal_group(*group, libc::SIGKILL) {
                        log::warn!("{label} outlived SIGHUP; killed its process group {group}");
                    }
                }
                let deadline = Instant::now() + KILL_GRACE;
                while groups.iter().copied().any(group_exists) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
                for group in groups.into_iter().filter(|group| group_exists(*group)) {
                    log::warn!("{label}'s process group {group} still exists after SIGKILL");
                }
            });
        match escalate {
            Ok(escalate) => Some(escalate),
            Err(err) => {
                log::warn!("could not spawn the kill escalation: {err}");
                for group in fallback_groups {
                    signal_group(group, libc::SIGKILL);
                }
                None
            }
        }
    }
    #[cfg(not(unix))]
    {
        // Windows has no process groups to signal and no daemon to signal them
        // from; the killer's `TerminateProcess` is the whole story there.
        let _ = (label, pids, activity);
        None
    }
}

fn wait_for_termination(termination: std::thread::JoinHandle<()>) {
    if termination.join().is_err() {
        log::warn!("session kill escalation panicked");
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
    // SAFETY: `kill(2)` against a group in a pty session this daemon created,
    // never 0 or -1.
    unsafe { libc::kill(-pid, signal) == 0 }
}

#[cfg(unix)]
fn group_exists(pid: libc::pid_t) -> bool {
    signal_group(pid, 0)
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
    let termination = terminate_groups(
        &"an abandoned session",
        [pid, None],
        Arc::new(Activity::new()),
    );
    if let Err(err) = child.kill() {
        // Already dead is fine here too.
        log::debug!("killing an abandoned child: {err}");
    }
    if let Err(err) = std::thread::Builder::new()
        .name("ade-reap-abandoned".to_owned())
        .spawn(move || {
            if let Err(err) = child.wait() {
                log::warn!("waiting on an abandoned child: {err}");
            }
            if let Some(termination) = termination {
                wait_for_termination(termination);
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

    use ade_session::proto::{
        Frame, LayoutDoc, SessionId, SessionInfo, SessionStatus, WorkspaceInfo, error_code,
    };

    use super::{
        ATTACH_RESERVE_BYTES, Activity, Outbound, OutputHub, SessionGrid, SessionTable,
        StatusConfig, SubscriberId, is_shell_name, login_shell_from, shell, terminal_env,
    };
    #[cfg(unix)]
    use super::{KILL_GRACE, terminate_groups, wait_for_termination};
    #[cfg(unix)]
    use crate::server::OUTBOUND_QUEUE_BYTES;
    /// The same bound, for the platforms that have no [`crate::server`] to take
    /// it from — it is [`DEFAULT_SCROLLBACK_BYTES`] there too.
    #[cfg(not(unix))]
    const OUTBOUND_QUEUE_BYTES: u64 = super::DEFAULT_SCROLLBACK_BYTES as u64;
    use crate::state::{PersistedSession, StateStore};

    /// Feed `chunks` to one session in order: is a bell pending after the last?
    /// Through [`Activity`] rather than [`super::BellScan`] alone, because the
    /// scan state and the sticky-until-the-next-chunk rule must hold together.
    fn bell_after(chunks: &[&[u8]]) -> bool {
        let activity = Activity::new();
        for chunk in chunks {
            activity.record_output(chunk);
        }
        activity.snapshot().1
    }

    /// A table holding one session and the workspace whose layout names it,
    /// built without a pty: the session is adopted as a *lost* row, which
    /// [`SessionTable::kill`] removes exactly like a live one. The sweep interval
    /// is past any test's lifetime on purpose: the lock-scope tests assert on who
    /// holds the session lock, and a sweeper waking mid-assertion is a second
    /// answer to that.
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
            project_id: None,
            project_identity: None,
            project_root: session.cwd.clone(),
            project_scope_rev: 0,
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
            project_id: None,
            project_identity: None,
            project_root: "/tmp/b".to_owned(),
            project_scope_rev: 0,
            created_at: 1,
            layout_rev: 1,
            layout: LayoutDoc::single_terminal(session.id.clone()),
        };
        let borrower = WorkspaceInfo {
            id: "workspace-a".to_owned(),
            name: "a".to_owned(),
            project_id: None,
            project_identity: None,
            project_root: "/tmp/a".to_owned(),
            project_scope_rev: 0,
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

    /// Removing the row and pruning its tab are **one** critical section. Made
    /// observable by holding the workspace lock from here: a `kill` that took the
    /// two in turn would have let go of the session lock before blocking, and
    /// that gap is when a reader gets a layout naming a session that is gone.
    #[test]
    fn kill_holds_the_session_lock_across_the_layout_scrub() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let (table, session) = seeded_table(dir.path());
        let workspaces = table.workspaces.lock().expect("the workspace lock");

        let killer = {
            let table = Arc::clone(&table);
            std::thread::spawn(move || smol::block_on(table.kill(&session)))
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
    /// section too, so a `CreateSession` cannot put the record back between them
    /// and have it removed out from under a live session.
    #[test]
    fn kill_workspace_holds_the_session_lock_across_dropping_the_record() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let (table, _) = seeded_table(dir.path());
        let workspaces = table.workspaces.lock().expect("the workspace lock");

        let killer = {
            let table = Arc::clone(&table);
            std::thread::spawn(move || smol::block_on(table.kill_workspace("workspace-1")))
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

    /// Listing layouts joins the same snapshot even though it returns no session
    /// rows: otherwise it slips between removing a session and scrubbing its tab.
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

    /// Rename publishes the workspace with its sessions, so it must use kill's
    /// transaction or announce a stale workspace after `WorkspaceRemoved`.
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

    /// Make every later [`crate::state::StateStore::save`] fail without
    /// disturbing what the table already loaded: a directory cannot be renamed
    /// over, on either platform.
    pub(super) fn wedge_the_ledger(dir: &std::path::Path) {
        let path = dir.join(crate::state::STATE_FILE);
        let _ = std::fs::remove_file(&path);
        std::fs::create_dir(&path).expect("wedging the ledger");
    }

    /// One subscriber, and everything it has been sent so far.
    fn subscribed(table: &Arc<SessionTable>) -> (super::OutboundQueue, Outbound) {
        let (outbound, queue) = Outbound::new(OUTBOUND_QUEUE_BYTES);
        table.subscribe(SubscriberId::next(), &outbound);
        // The subscription snapshot is not what these tests are about.
        while queue.try_recv().is_some() {}
        (queue, outbound)
    }

    fn published(queue: &super::OutboundQueue) -> Vec<Frame> {
        let mut frames = Vec::new();
        while let Some(frame) = queue.try_recv() {
            frames.push(frame);
        }
        frames
    }

    /// §8.3 class B: a record-only mutation nothing outside this process has
    /// seen yet. A write that cannot be recorded is undone and never announced,
    /// so no client ever acts on a layout the daemon does not have.
    ///
    /// Reverting the order — publish, then persist — leaves the event in the
    /// queue and the revision in the table, and this fails on both.
    #[test]
    fn a_layout_that_cannot_be_recorded_is_rolled_back_and_never_announced() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let (table, _) = seeded_table(dir.path());
        let (queue, _outbound) = subscribed(&table);
        let before = table.list_workspaces();
        wedge_the_ledger(dir.path());

        let error = table
            .update_layout(
                "workspace-1",
                LayoutDoc::empty(),
                2,
                Some(SubscriberId::next()),
            )
            .expect_err("a layout the ledger refused must not ack success");

        assert_eq!(error.code, error_code::PERSIST_FAILED);
        assert_eq!(
            table.list_workspaces(),
            before,
            "the layout and its revision must be back where they were"
        );
        assert!(
            published(&queue).is_empty(),
            "a layout nothing recorded was announced anyway"
        );
    }

    /// The other class-B mutation, same contract.
    #[test]
    fn a_rename_that_cannot_be_recorded_is_rolled_back_and_never_announced() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let (table, _) = seeded_table(dir.path());
        let (queue, _outbound) = subscribed(&table);
        wedge_the_ledger(dir.path());

        let error = table
            .rename_workspace("workspace-1", "renamed")
            .expect_err("a rename the ledger refused must not ack success");

        assert_eq!(error.code, error_code::PERSIST_FAILED);
        assert_eq!(
            table.list_workspaces()[0].name,
            "one",
            "the old name must be back"
        );
        assert!(
            published(&queue).is_empty(),
            "a rename nothing recorded was announced anyway"
        );
    }

    #[test]
    fn a_project_scope_that_cannot_be_recorded_restores_its_root_too() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let (table, _) = seeded_table(dir.path());
        let (queue, _outbound) = subscribed(&table);
        let before = table.list_workspaces();
        wedge_the_ledger(dir.path());

        let error = table
            .update_workspace_project(
                "workspace-1",
                "viral-studio",
                "/repos/viral-studio",
                Some("/repos/viral-studio/worktree"),
                None,
            )
            .expect_err("a project scope the ledger refused must not ack success");

        assert_eq!(error.code, error_code::PERSIST_FAILED);
        assert_eq!(table.list_workspaces(), before);
        assert!(published(&queue).is_empty());
    }

    /// §8.3 class A: the child is already dead, so `removed` goes out whatever
    /// the disk says — withholding it would hang an attached terminal — and the
    /// ack is the only thing that can carry the failure.
    ///
    /// Rolling the removal back, or withholding its event, fails here; so does
    /// answering a kill nothing recorded with success.
    #[test]
    fn a_kill_that_cannot_be_recorded_still_removes_the_row_and_still_says_so() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let (table, id) = seeded_table(dir.path());
        let (queue, _outbound) = subscribed(&table);
        wedge_the_ledger(dir.path());

        let error = smol::block_on(table.kill(&id))
            .expect_err("a kill the ledger refused must not ack success");

        assert_eq!(error.code, error_code::PERSIST_FAILED);
        assert!(
            table.list().is_empty(),
            "the session is dead; the row must not come back because a write failed"
        );
        assert!(
            published(&queue)
                .iter()
                .any(|frame| matches!(frame, Frame::Removed { session_id } if *session_id == id)),
            "the removal was withheld behind a disk write"
        );
    }

    /// The workspace-level class-A kill, same contract.
    #[test]
    fn a_workspace_kill_that_cannot_be_recorded_still_removes_it_and_still_says_so() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let (table, _) = seeded_table(dir.path());
        let (queue, _outbound) = subscribed(&table);
        wedge_the_ledger(dir.path());

        let error = smol::block_on(table.kill_workspace("workspace-1"))
            .expect_err("a workspace kill the ledger refused must not ack success");

        assert_eq!(error.code, error_code::PERSIST_FAILED);
        assert!(table.list_workspaces().is_empty());
        assert!(
            published(&queue).iter().any(|frame| matches!(
                frame,
                Frame::WorkspaceRemoved { workspace_id, .. } if workspace_id == "workspace-1"
            )),
            "the removal was withheld behind a disk write"
        );
    }

    /// A table on `dir` with nothing in it, sweeping past any test's lifetime.
    fn empty_table(dir: &std::path::Path) -> Arc<SessionTable> {
        SessionTable::load(
            StateStore::new(dir),
            StatusConfig {
                needs_input_after: Duration::from_secs(600),
                sweep_interval: Duration::from_secs(600),
            },
        )
    }

    fn workspace_request(name: &str) -> super::WorkspaceRequest {
        super::WorkspaceRequest {
            root: "/tmp/proj".to_owned(),
            name: Some(name.to_owned()),
            project_id: None,
            project_identity: None,
            id: None,
        }
    }

    /// The workspace spec's row: the record exists the moment the panel row
    /// does, before any terminal, and zero sessions is a normal state rather
    /// than a create whose session half failed. §8.3 class C puts the write
    /// before the announcement.
    #[test]
    fn an_empty_create_records_a_workspace_with_no_sessions_and_announces_it() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let table = empty_table(dir.path());
        let (queue, _outbound) = subscribed(&table);

        let workspace = table
            .create_workspace(workspace_request("proj"))
            .expect("the empty create");

        assert_eq!(workspace.name, "proj");
        assert_eq!(workspace.project_scope_rev, 0);
        assert!(workspace.layout.terminal_sessions().is_empty());
        // Zero, so the client's first layout write is revision 1.
        assert_eq!(workspace.layout_rev, 0);
        assert!(table.list().is_empty(), "an empty create spawned something");
        assert!(table.workspace_sessions(&workspace.id).is_empty());
        assert_eq!(table.list_workspaces(), vec![workspace.clone()]);
        assert!(
            published(&queue).iter().any(|frame| matches!(
                frame,
                Frame::Workspace {
                    workspace: announced,
                    sessions,
                    ..
                } if announced.id == workspace.id && sessions.is_empty()
            )),
            "the row was not announced, or was announced with a session in it"
        );
        assert_eq!(
            StateStore::new(dir.path()).load().workspaces,
            vec![workspace],
            "the record was announced before it was on disk"
        );
    }

    #[test]
    fn project_scope_revision_advances_and_survives_reload() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let table = empty_table(dir.path());
        let workspace = table
            .create_workspace(workspace_request("seedance2-5"))
            .expect("workspace");
        assert_eq!(workspace.project_scope_rev, 0);

        let refused = table
            .update_workspace_project(
                &workspace.id,
                "viral-studio",
                "/home/user/Code/viral-studio",
                Some(" \t"),
                None,
            )
            .expect_err("an empty replacement root must be refused");
        assert_eq!(refused.code, error_code::INVALID_ARGUMENT);
        assert_eq!(table.list_workspaces(), vec![workspace.clone()]);

        let (updated, _) = table
            .update_workspace_project(
                &workspace.id,
                " viral-studio ",
                "/home/user/Code/viral-studio ",
                Some(" /home/user/Code/worktrees/viral-studio/seedance2-5 "),
                None,
            )
            .expect("project identity update");
        assert_eq!(updated.name, "seedance2-5");
        assert_eq!(updated.project_id.as_deref(), Some(" viral-studio "));
        assert_eq!(
            updated.project_identity.as_deref(),
            Some("/home/user/Code/viral-studio ")
        );
        assert_eq!(
            updated.project_root,
            " /home/user/Code/worktrees/viral-studio/seedance2-5 "
        );
        assert_eq!(updated.project_scope_rev, 1);

        let (updated, _) = table
            .update_workspace_project(
                &workspace.id,
                "viral-studio",
                "/home/user/Code/viral-studio",
                Some(" /home/user/Code/worktrees/viral-studio/seedance2-5 "),
                None,
            )
            .expect("project metadata update at the same root");
        assert_eq!(updated.project_scope_rev, 2);

        let (updated, _) = table
            .update_workspace_project(
                &workspace.id,
                "viral-studio",
                "/home/user/Code/viral-studio",
                Some(" /home/user/Code/worktrees/viral-studio/seedance2-5 "),
                None,
            )
            .expect("an identical project scope update");
        assert_eq!(updated.project_scope_rev, 2);

        let (updated, _) = table
            .update_workspace_project(
                &workspace.id,
                "viral-studio",
                "/home/user/Code/viral-studio",
                Some(" /home/user/Code/worktrees/viral-studio/seedance2-5 "),
                Some(5),
            )
            .expect("advancing past the client's revision");
        assert_eq!(updated.project_scope_rev, 6);

        let (updated, _) = table
            .update_workspace_project(
                &workspace.id,
                "viral-studio",
                "/home/user/Code/viral-studio",
                Some(" /home/user/Code/worktrees/viral-studio/seedance2-5 "),
                Some(5),
            )
            .expect("an already reconciled project scope update");
        assert_eq!(updated.project_scope_rev, 6);

        drop(table);
        let restored = empty_table(dir.path())
            .open_workspace(&workspace.id)
            .expect("restored workspace")
            .0;
        assert_eq!(restored, updated);
    }

    // --------------------------------- the generation-2 auto-create's parts ---

    /// A table holding one workspace and two *lost* rows in it: enough for a
    /// layout to name a session without a pty to spawn one from.
    fn table_with_two_rows(dir: &std::path::Path) -> (Arc<SessionTable>, SessionId, SessionId) {
        let row = |id: &str| SessionInfo {
            id: SessionId::new(id),
            workspace_id: "ws-2".to_owned(),
            agent_kind: "shell".to_owned(),
            instance_label: id.to_owned(),
            cwd: "/tmp/proj".to_owned(),
            created_at: 1,
            status: SessionStatus::Exited,
        };
        let (first, second) = (row("s-first"), row("s-second"));
        StateStore::new(dir)
            .save(
                &[
                    PersistedSession::from_info(&first),
                    PersistedSession::from_info(&second),
                ],
                &[WorkspaceInfo {
                    id: "ws-2".to_owned(),
                    name: "proj".to_owned(),
                    project_id: None,
                    project_identity: None,
                    project_root: "/tmp/proj".to_owned(),
                    project_scope_rev: 0,
                    created_at: 1,
                    layout_rev: 0,
                    layout: LayoutDoc::empty(),
                }],
            )
            .expect("seeding the state file");
        let table = empty_table(dir);
        (table, first.id, second.id)
    }

    /// A named auto-create must adopt the record that is already there rather
    /// than write over it — a second insert would reset a live workspace's
    /// layout and revision — and only the caller that really made it may be
    /// told it did, since that flag is the warrant to remove it again.
    #[test]
    fn ensure_workspace_adopts_a_named_record_without_resetting_it() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let table = empty_table(dir.path());
        let named = || super::WorkspaceRequest {
            root: "/tmp/proj".to_owned(),
            name: Some("proj".to_owned()),
            project_id: None,
            project_identity: None,
            id: Some("ws-named".to_owned()),
        };

        let (first, created_here) = table.ensure_workspace(named()).expect("the first ensure");
        assert!(created_here, "the first ensure made the record");
        table
            .update_layout(&first.id, LayoutDoc::empty(), 4, None)
            .expect("a later layout write");

        let (again, created_here) = table.ensure_workspace(named()).expect("the second ensure");
        assert!(
            !created_here,
            "a reuse must not claim it created the record"
        );
        assert_eq!(again.id, first.id);
        assert_eq!(again.layout_rev, 4, "the live record's revision was reset");
        assert_eq!(table.list_workspaces().len(), 1);
    }

    /// An id-less ensure cannot collide, so it stays a plain create.
    #[test]
    fn ensure_workspace_mints_a_fresh_record_when_the_request_names_none() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let table = empty_table(dir.path());

        let (first, made) = table
            .ensure_workspace(workspace_request("proj"))
            .expect("the first ensure");
        assert!(made);
        let (second, made) = table
            .ensure_workspace(workspace_request("proj"))
            .expect("the second ensure");
        assert!(made);
        assert_ne!(first.id, second.id);
    }

    /// Compensation takes back only what the failed request left behind. A
    /// workspace a concurrent create has since put a session in is no longer
    /// this refusal's to remove, and neither is one already gone.
    #[test]
    fn compensation_spares_a_workspace_something_else_is_using() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let (table, occupied, _) = table_with_two_rows(dir.path());
        let empty = table
            .create_workspace(workspace_request("empty"))
            .expect("an empty record");

        smol::block_on(table.remove_empty_workspace("ws-2")).expect("the populated workspace");
        assert_eq!(table.workspace_sessions("ws-2").len(), 2);
        assert!(table.list().iter().any(|info| info.id == occupied));

        smol::block_on(table.remove_empty_workspace(&empty.id)).expect("the empty workspace");
        smol::block_on(table.remove_empty_workspace(&empty.id)).expect("nothing left to remove");
        assert_eq!(table.list_workspaces().len(), 1, "only ws-2 should be left");
    }

    /// The auto-create's layout converges with whoever else is writing the
    /// document instead of replacing it: a fresh one-leaf write here would
    /// delete tabs a client already has on screen.
    #[test]
    fn the_auto_created_layout_merges_with_the_document_that_won() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let (table, first, second) = table_with_two_rows(dir.path());
        table
            .update_layout("ws-2", LayoutDoc::single_terminal(first.clone()), 1, None)
            .expect("the concurrent writer's layout");

        table
            .install_legacy_layout("ws-2", &second)
            .expect("the auto-created layout");

        let (workspace, _) = table.open_workspace("ws-2").expect("the workspace");
        assert_eq!(
            workspace.layout.terminal_sessions(),
            vec![first, second.clone()],
            "the concurrent document was replaced instead of appended to"
        );
        assert_eq!(workspace.layout_rev, 2);

        // Already there: nothing to write, and nothing to bump.
        table
            .install_legacy_layout("ws-2", &second)
            .expect("the second install");
        assert_eq!(
            table.open_workspace("ws-2").expect("the workspace").0,
            workspace
        );
    }

    /// At generation 2 the layout arrived inside the workspace event, and the
    /// `Created` reply carries none — so this is the one layout write whose own
    /// requester must be told, and an ordinary write still must not be.
    #[test]
    fn the_auto_created_layout_reaches_every_subscriber() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let (table, first, _) = table_with_two_rows(dir.path());
        let (outbound, queue) = Outbound::new(OUTBOUND_QUEUE_BYTES);
        let writer = SubscriberId::next();
        table.subscribe(writer, &outbound);
        while queue.try_recv().is_some() {}

        table
            .install_legacy_layout("ws-2", &first)
            .expect("the auto-created layout");
        assert!(
            published(&queue)
                .iter()
                .any(|frame| matches!(frame, Frame::LayoutChanged { rev: 1, .. })),
            "the requester was not told its own workspace's layout"
        );

        table
            .update_layout("ws-2", LayoutDoc::empty(), 2, Some(writer))
            .expect("an ordinary layout write");
        assert!(
            published(&queue).is_empty(),
            "an ordinary write echoed back to its writer"
        );
    }

    /// §8.1: a ledger that refuses the auto-created layout is an error the
    /// request carries back, never a silent `Created` over a workspace whose
    /// session is in no layout.
    #[test]
    fn a_wedged_ledger_refuses_the_auto_created_layout() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let (table, first, _) = table_with_two_rows(dir.path());
        wedge_the_ledger(dir.path());

        let refused = table
            .install_legacy_layout("ws-2", &first)
            .expect_err("a wedged ledger must refuse the write");

        assert_eq!(refused.code, error_code::PERSIST_FAILED);
        assert!(
            table
                .open_workspace("ws-2")
                .expect("the workspace")
                .0
                .layout
                .terminal_sessions()
                .is_empty(),
            "the refused layout was left applied in memory"
        );
    }

    /// A row with nothing in it is still a row after a restart: `load` keeps
    /// every persisted workspace, session or no session.
    #[test]
    fn an_empty_workspace_comes_back_empty_after_a_restart() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let created = empty_table(dir.path())
            .create_workspace(workspace_request("proj"))
            .expect("the empty create");

        let restarted = empty_table(dir.path());
        assert_eq!(restarted.list_workspaces(), vec![created.clone()]);
        assert!(restarted.workspace_sessions(&created.id).is_empty());
        assert!(restarted.list().is_empty());
    }

    /// Row delete on a row that never had a terminal. `NOT_FOUND` is for a
    /// workspace nothing knows about, not for an empty one, and the record
    /// leaves memory and the ledger together.
    #[test]
    fn killing_an_empty_workspace_removes_its_record_everywhere() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let table = empty_table(dir.path());
        let workspace = table
            .create_workspace(workspace_request("proj"))
            .expect("the empty create");
        let (queue, _outbound) = subscribed(&table);

        smol::block_on(table.kill_workspace(&workspace.id)).expect("killing an empty workspace");

        assert!(table.list_workspaces().is_empty());
        assert!(StateStore::new(dir.path()).load().workspaces.is_empty());
        assert!(
            published(&queue).iter().any(|frame| matches!(
                frame,
                Frame::WorkspaceRemoved { workspace_id, .. } if *workspace_id == workspace.id
            )),
            "the removal was not announced"
        );
        assert_eq!(
            smol::block_on(table.kill_workspace(&workspace.id))
                .expect_err("killing it twice is an error, not a second removal")
                .code,
            error_code::NOT_FOUND
        );
    }

    /// A `CreateRequest` for the strictness tests: the workspace is the only
    /// thing they vary, and nothing gets far enough to spawn.
    fn session_in(workspace_id: &str) -> super::CreateRequest {
        super::CreateRequest {
            workspace_id: workspace_id.to_owned(),
            cwd: "/tmp/proj".to_owned(),
            command: String::new(),
            env: Vec::new(),
            cols: 80,
            rows: 24,
            agent_kind: "shell".to_owned(),
            instance_label: "one".to_owned(),
            scrollback_bytes: None,
        }
    }

    /// The spec's "sessions are created only inside an existing workspace".
    /// Nothing about a session makes a record any more, so a create naming one
    /// the daemon does not hold is refused before it spawns — no pty, no
    /// reservation, no workspace conjured out of the id.
    #[test]
    fn a_session_naming_an_unknown_workspace_is_refused_and_creates_nothing() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let table = empty_table(dir.path());
        let (queue, _outbound) = subscribed(&table);

        let refused = smol::block_on(table.create(session_in("no-such-workspace")))
            .expect_err("a session in a workspace that does not exist");

        assert_eq!(refused.code, error_code::NOT_FOUND);
        assert!(table.list().is_empty());
        assert!(table.list_workspaces().is_empty(), "a workspace was minted");
        assert!(StateStore::new(dir.path()).load().sessions.is_empty());
        assert!(published(&queue).is_empty(), "a refused create announced");
    }

    /// An empty id is the field being unusable rather than an id nothing
    /// matches, so it is `invalid_argument` and not `not_found` (§2.1). The
    /// distinction is what stops a client retrying a bug as if it were a race.
    #[test]
    fn a_session_with_no_workspace_id_is_an_invalid_argument() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let table = empty_table(dir.path());

        let refused =
            smol::block_on(table.create(session_in(""))).expect_err("a session with no workspace");

        assert_eq!(refused.code, error_code::INVALID_ARGUMENT);
        assert!(table.list().is_empty());
        assert!(table.list_workspaces().is_empty());
    }

    /// §8.3 class C for the empty half: a record the ledger refused is undone
    /// and the create fails, so no client is left holding a row this daemon
    /// will not have after a restart.
    #[test]
    fn an_empty_workspace_that_cannot_be_recorded_is_taken_back() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let table = empty_table(dir.path());
        let (queue, _outbound) = subscribed(&table);
        wedge_the_ledger(dir.path());

        let error = table
            .create_workspace(workspace_request("proj"))
            .expect_err("a create the ledger refused must not ack success");

        assert_eq!(error.code, error_code::PERSIST_FAILED);
        assert!(table.list_workspaces().is_empty());
        assert!(
            !published(&queue)
                .iter()
                .any(|frame| matches!(frame, Frame::Workspace { .. })),
            "a workspace that does not exist was announced as created"
        );
    }

    /// The workspace create's own window: the record is in the map before the
    /// ledger write, so a `kill_workspace` naming it can remove it and publish
    /// `workspace_removed` while the create is still writing. Publishing
    /// `workspace` after that would leave every subscriber a row nothing will
    /// remove again — so the create rechecks and answers `NOT_FOUND` instead.
    ///
    /// The ledger needs no repair either way, and that is asserted here rather
    /// than reasoned about: both writes snapshot the live map under
    /// `persisting`, so whichever lands last still writes the removal.
    #[test]
    fn a_workspace_killed_while_it_is_being_recorded_is_never_announced() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let table = empty_table(dir.path());
        let (queue, _outbound) = subscribed(&table);
        let held = table.persisting.lock().expect("the persist guard");

        let creator = {
            let table = Arc::clone(&table);
            std::thread::spawn(move || table.create_workspace(workspace_request("proj")))
        };
        // The record is in the map before the write, which is what makes it
        // killable — and how the killer learns the id it was just minted.
        let id = wait_for(|| table.list_workspaces().first().map(|w| w.id.clone()));
        let killer = {
            let table = Arc::clone(&table);
            let id = id.clone();
            std::thread::spawn(move || smol::block_on(table.kill_workspace(&id)))
        };
        wait_for(|| table.list_workspaces().is_empty().then_some(()));

        drop(held);
        assert_eq!(
            creator
                .join()
                .expect("the creating thread")
                .expect_err("a create whose record was killed under it")
                .code,
            error_code::NOT_FOUND
        );
        killer
            .join()
            .expect("the killing thread")
            .expect("the kill");

        assert!(
            !published(&queue).iter().any(|frame| matches!(
                frame,
                Frame::Workspace { workspace, .. } if workspace.id == id
            )),
            "a workspace that was already removed was announced as created"
        );
        assert!(table.list_workspaces().is_empty());
        assert!(
            StateStore::new(dir.path()).load().workspaces.is_empty(),
            "the removed record was written back by the slower persist"
        );
    }

    /// A ledger that exists and did not parse is this daemon's ignorance, not
    /// an empty host: the rows it still holds may name terminals that are
    /// running right now. So the store goes read-only, every ack says
    /// `persisted: false`, and a mutation cannot turn a transient read failure
    /// into the permanent loss of what the file recorded.
    #[test]
    fn a_ledger_that_could_not_be_read_is_never_written_over() {
        const MALFORMED: &str = "this was never json";

        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join(crate::state::STATE_FILE);
        std::fs::write(&path, MALFORMED).expect("seeding the ledger");
        let table = empty_table(dir.path());
        let (queue, _outbound) = subscribed(&table);

        assert!(!table.ledger_authoritative());
        assert!(
            !table.persisted(),
            "a ledger this daemon cannot read is not one it records to"
        );
        table
            .create_workspace(workspace_request("proj"))
            .expect("a mutation still applies and still publishes");
        assert_eq!(table.list_workspaces().len(), 1);
        assert!(
            published(&queue).iter().any(|frame| matches!(
                frame,
                Frame::Workspace { persisted, .. } if !persisted
            )),
            "the workspace was announced as recorded"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("the ledger"),
            MALFORMED,
            "a mutation overwrote the ledger it could not read"
        );
    }

    /// A `CreateSession` writes its ledger row before its terminal exists, so
    /// `kill_workspace` cannot see it in the table. Left in `pending`, the
    /// kill's own persist writes a session whose workspace is gone — and the
    /// next daemon's load migrates that workspace back into existence, undoing
    /// the delete that won.
    #[test]
    fn a_workspace_kill_takes_the_reservations_naming_it() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let (table, _) = seeded_table(dir.path());
        let reserved = SessionInfo {
            id: SessionId::new("reserved"),
            workspace_id: "workspace-1".to_owned(),
            agent_kind: "shell".to_owned(),
            instance_label: "spawning".to_owned(),
            cwd: "/tmp".to_owned(),
            created_at: 2,
            status: SessionStatus::Working,
        };
        table
            .reserve(PersistedSession::from_info(&reserved))
            .expect("the reservation");
        assert_eq!(
            StateStore::new(dir.path()).load().sessions.len(),
            1,
            "the reservation is what this test kills around"
        );

        smol::block_on(table.kill_workspace("workspace-1")).expect("the workspace kill");

        let ledger = StateStore::new(dir.path()).load();
        assert!(
            ledger.sessions.is_empty(),
            "the ledger kept a session whose workspace was deleted: {ledger:?}"
        );
        assert!(ledger.workspaces.is_empty());
    }

    /// The rename's own window, the create's mirrored: the new name is written
    /// and a kill takes the record before the announcement goes out.
    /// Publishing then would leave every subscriber a renamed row nothing will
    /// ever remove again — and the renaming client a name it recorded against a
    /// workspace the next reconcile drops.
    #[test]
    fn a_rename_a_kill_won_answers_not_found() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let (table, _) = seeded_table(dir.path());
        let (queue, _outbound) = subscribed(&table);

        let (arrived, at_the_gate) = std::sync::mpsc::sync_channel(1);
        let (go, released) = std::sync::mpsc::sync_channel(1);
        *table.test_gate.lock().expect("the gate") = Some((arrived, released));

        let renamer = {
            let table = Arc::clone(&table);
            std::thread::spawn(move || table.rename_workspace("workspace-1", "renamed"))
        };
        at_the_gate
            .recv_timeout(Duration::from_secs(10))
            .expect("the rename to reach its recheck");
        let killer = {
            let table = Arc::clone(&table);
            std::thread::spawn(move || smol::block_on(table.kill_workspace("workspace-1")))
        };
        // The removal is applied and published before its own persist, which
        // waits on the guard the parked rename still holds.
        wait_for(|| table.list_workspaces().is_empty().then_some(()));
        go.send(()).expect("releasing the rename");

        assert_eq!(
            renamer
                .join()
                .expect("the renaming thread")
                .expect_err("a rename whose record was killed under it")
                .code,
            error_code::NOT_FOUND
        );
        killer
            .join()
            .expect("the killing thread")
            .expect("the workspace kill");
        assert!(
            !published(&queue)
                .iter()
                .any(|frame| matches!(frame, Frame::Workspace { .. })),
            "a workspace that was already removed was announced as renamed"
        );
    }

    /// Poll until `ready` answers, or fail the test. For the states another
    /// thread reaches on its own — a sleep would only guess at when.
    fn wait_for<T>(mut ready: impl FnMut() -> Option<T>) -> T {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(value) = ready() {
                return value;
            }
            assert!(std::time::Instant::now() < deadline, "timed out waiting");
            std::thread::yield_now();
        }
    }

    /// The revision, its ledger write and its broadcast are one ordered
    /// operation. §8.3 class B puts the write between the mutation and the
    /// event, so the workspace lock cannot be what spans them — `persisting`
    /// is, and it is held right through the publish. Release it any earlier
    /// and a later revision can apply, or publish, ahead of this one.
    #[test]
    fn update_layout_serializes_its_revision_through_its_broadcast() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let (table, _) = seeded_table(dir.path());
        let events = table.events.lock().expect("the event lock");

        let writer = {
            let table = Arc::clone(&table);
            std::thread::spawn(move || {
                table.update_layout(
                    "workspace-1",
                    LayoutDoc::empty(),
                    2,
                    Some(SubscriberId::next()),
                )
            })
        };
        std::thread::sleep(REACHED_THE_LOCK);
        assert!(
            table.persisting.try_lock().is_err(),
            "a later revision can land before this revision is broadcast"
        );

        drop(events);
        writer
            .join()
            .expect("the writer thread")
            .expect("the layout update");
    }

    /// Workspace removal is not announced after the transaction drops the
    /// workspace lock: a create in that gap would be hidden by the stale event.
    #[test]
    fn kill_workspace_holds_the_workspace_lock_until_removal_is_published() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let (table, _) = seeded_table(dir.path());
        let events = table.events.lock().expect("the event lock");

        let killer = {
            let table = Arc::clone(&table);
            std::thread::spawn(move || smol::block_on(table.kill_workspace("workspace-1")))
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

    /// `persist` is serialized *end to end*, the write under the same guard as
    /// the snapshot. Two that only serialized their snapshots could still reach
    /// `save` in the opposite order and leave the older pair on disk.
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
        assert!(
            terminate_groups(&"a reaped session", [Some(decoy.id()), None], activity).is_none()
        );

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
        let termination = terminate_groups(
            &"a live session",
            [Some(decoy.id()), None],
            Arc::new(Activity::new()),
        )
        .expect("a live process group should be terminated");
        wait_for_termination(termination);

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

    /// The bound is a policy, and this is the policy: the reader that stopped
    /// reading loses its connection, the one that kept reading loses nothing, and
    /// the publisher — a pty drain thread in the real daemon — waits on neither.
    ///
    /// Time-bound on purpose: unbounded growth is a slow failure this test could
    /// not otherwise see, and a *blocking* send would stop dead at the bound.
    #[test]
    fn a_subscriber_that_stops_reading_is_dropped_and_never_stalls_the_publisher() {
        use std::sync::mpsc;
        use std::thread;

        /// The unix drain thread's read size, what the bound is sized against.
        const CHUNK: usize = 8 * 1024;
        /// Enough to go over the bound, plus slack for the replay `attach` queues
        /// ahead: by construction one reader is over its bound and one is not.
        const CHUNKS: usize = (OUTBOUND_QUEUE_BYTES as usize / CHUNK) + 8;

        let id = SessionId::new("session-1");
        let (stalled, _never_read) = Outbound::new(OUTBOUND_QUEUE_BYTES);
        let (reading, drained) = Outbound::new(OUTBOUND_QUEUE_BYTES);
        let mut hub = OutputHub::new(64 * 1024, None, None);
        hub.attach(&id, SubscriberId::next(), &stalled);
        hub.attach(&id, SubscriberId::next(), &reading);

        // The well-behaved client: a thread that does nothing but read, the way
        // a connection's writer task does.
        let reader = thread::spawn(move || {
            let mut received = 0usize;
            while smol::block_on(drained.recv()).is_some() {
                received += 1;
            }
            received
        });

        let (done, published) = mpsc::channel();
        let publisher = thread::spawn(move || {
            let chunk = vec![b'x'; CHUNK];
            for _ in 0..CHUNKS {
                hub.publish(&id, &chunk);
            }
            done.send(hub.subscribers.len()).expect("reporting");
            hub
        });
        let subscribers = published
            .recv_timeout(Duration::from_secs(30))
            .expect("the publishing thread blocked on a client that stopped reading");

        assert_eq!(subscribers, 1, "the stalled subscriber was not dropped");
        assert!(
            stalled.is_closed(),
            "the stalled subscriber's queue was left open, so its connection would live on"
        );
        assert!(!reading.is_closed(), "the reading subscriber was punished");
        // The hub still holds the surviving subscriber's sender, so it has to
        // go before the reader thread can see the channel close.
        drop(publisher.join().expect("publisher thread"));
        drop(reading);
        // One replay frame from `attach`, then every chunk: the client that
        // kept reading missed nothing.
        assert_eq!(reader.join().expect("reader thread"), CHUNKS + 1);
    }

    /// **The bound counts bytes, and this is why it had to** — see [`Outbound`]
    /// for the argument: conhost writes a line at a time, so Windows frames are
    /// tens of bytes and a *frame* bound dropped healthy clients after ~24 KiB.
    ///
    /// `CHUNKS` is fifteen times the frame bound that used to be here, and half
    /// the byte bound that replaced it.
    #[test]
    fn a_subscriber_fed_many_tiny_chunks_is_not_mistaken_for_a_stalled_one() {
        const CHUNKS: usize = 4000;

        let id = SessionId::new("session-1");
        let (never_read, _queue) = Outbound::new(OUTBOUND_QUEUE_BYTES);
        let mut hub = OutputHub::new(64 * 1024, None, None);
        hub.attach(&id, SubscriberId::next(), &never_read);
        for _ in 0..CHUNKS {
            hub.publish(&id, b"ZORCALINE\r\n");
        }

        assert_eq!(
            hub.subscribers.len(),
            1,
            "a client was dropped for being sent {CHUNKS} small frames it had every chance of \
             reading"
        );
        assert!(!never_read.is_closed(), "its queue was closed anyway");
    }

    #[test]
    fn exiting_alternate_screen_repairs_a_truncated_replay() {
        let id = SessionId::new("alternate-screen-exit");
        let drawing = b"\x1b[HSTALE APP\x1b[4;1HAPP FOOTER";
        let exit = b"\x1b[?1049lSHELL AFTER";
        for capacity in [drawing.len(), 4096] {
            for split in 0..=exit.len() {
                for resized in [false, true] {
                    let mut hub = OutputHub::new(capacity, Some(SessionGrid::new(40, 8)), None);
                    hub.publish(&id, b"shell before\r\n\x1b[?1049h");
                    hub.publish(&id, drawing);

                    let (outbound, queue) = Outbound::new(OUTBOUND_QUEUE_BYTES);
                    hub.attach(&id, SubscriberId::next(), &outbound);
                    let Some(Frame::Replay {
                        bytes, truncated, ..
                    }) = queue.try_recv()
                    else {
                        panic!("expected replay");
                    };
                    assert_eq!(truncated, capacity == drawing.len());
                    let mut client = SessionGrid::new(40, 8);
                    client.feed(&bytes);
                    assert!(String::from_utf8_lossy(&client.repaint()).contains("STALE APP"));
                    if resized {
                        hub.resize_grid(50, 10);
                        client.resize(50, 10);
                        hub.repaint(&id);
                    }

                    hub.publish(&id, &exit[..split]);
                    hub.publish(&id, &exit[split..]);
                    while let Some(frame) = queue.try_recv() {
                        let Frame::Output { bytes, .. } = frame else {
                            panic!("expected live output");
                        };
                        client.feed(&bytes);
                    }
                    let actual = client.repaint();
                    let expected = hub.grid.as_ref().expect("daemon grid").repaint();
                    assert_eq!(
                        String::from_utf8_lossy(&actual),
                        String::from_utf8_lossy(&expected),
                        "shell restoration: capacity={capacity}, split={split}, resized={resized}"
                    );
                }
            }
        }
    }

    #[test]
    fn exit_repaint_preserves_following_control_sequences() {
        let cases: &[(&str, &[u8], &[u8])] = &[
            ("color", b"", b"\x1b[?1049l\x1b[31mRED\x1b[0m"),
            ("title BEL", b"", b"\x1b[?1049l\x1b]0;after title\x07PROMPT"),
            (
                "title ST",
                b"",
                b"\x1b[?1049l\x1b]2;after title\x1b\\PROMPT",
            ),
            ("UTF-8", b"", "\x1b[?1049l☃ prompt".as_bytes()),
            (
                "synchronized",
                b"",
                b"\x1b[?2026h\x1b[?1049l\x1b[31mRED\x1b[?2026l\x1b[0mTAIL",
            ),
            (
                "reentry",
                b"",
                b"\x1b[?1049lFIRST\x1b[?1049hSECOND\x1b[?1049lLAST",
            ),
            ("combined modes", b"", b"\x1b[?25;1049lPROMPT"),
            ("reset", b"", b"\x1bc\x1b[31mRESET"),
            (
                "origin",
                b"\x1b[3;6r\x1b[?6h",
                b"\x1b[?1049l\x1b[?6l\x1b[rPROMPT",
            ),
            ("DCS", b"", b"\x1b[?1049l\x1bP0qdiscard\x1b\\\x1b[31mRED"),
        ];
        for &(name, setup, output) in cases {
            let mut partitions: Vec<Vec<&[u8]>> = (0..=output.len())
                .map(|split| vec![&output[..split], &output[split..]])
                .collect();
            partitions.push(output.chunks(1).collect());
            for (partition, chunks) in partitions.into_iter().enumerate() {
                let id = SessionId::new("exit-boundary");
                let mut hub = OutputHub::new(4096, Some(SessionGrid::new(40, 8)), None);
                hub.publish(&id, b"shell before\r\n\x1b[?1049h");
                hub.publish(&id, setup);
                hub.publish(&id, b"\x1b[HAPP");
                let (outbound, queue) = Outbound::new(OUTBOUND_QUEUE_BYTES);
                hub.attach(&id, SubscriberId::next(), &outbound);
                let Some(Frame::Replay { bytes, .. }) = queue.try_recv() else {
                    panic!("expected replay");
                };
                let mut client = SessionGrid::new(40, 8);
                client.feed(&bytes);

                for chunk in chunks.into_iter().chain([b"!\x1b[0m".as_slice()]) {
                    hub.publish(&id, chunk);
                    while let Some(frame) = queue.try_recv() {
                        let Frame::Output { bytes, .. } = frame else {
                            panic!("expected output");
                        };
                        client.feed(&bytes);
                    }
                }
                assert_eq!(
                    String::from_utf8_lossy(&client.repaint()),
                    String::from_utf8_lossy(&hub.grid.as_ref().expect("daemon grid").repaint()),
                    "case={name}, partition={partition}"
                );
            }
        }
    }

    #[test]
    fn ordinary_output_does_not_trigger_a_repaint() {
        let id = SessionId::new("ordinary-output");
        let mut hub = OutputHub::new(4096, Some(SessionGrid::new(40, 8)), None);
        let (outbound, queue) = Outbound::new(OUTBOUND_QUEUE_BYTES);
        hub.attach(&id, SubscriberId::next(), &outbound);
        assert!(matches!(queue.try_recv(), Some(Frame::Replay { .. })));
        for chunk in [
            b"shell output".as_slice(),
            b"\x1b[?1049h",
            b"app output",
            b"\x1b]0;literal [?1049l\x07",
            b"\x1bPqpayload [?1049l\x1b\\",
        ] {
            hub.publish(&id, chunk);
            let Some(Frame::Output { bytes, .. }) = queue.try_recv() else {
                panic!("expected output");
            };
            assert_eq!(bytes, chunk);
            assert!(queue.try_recv().is_none(), "no unnecessary repaint");
        }
    }

    #[test]
    fn a_repaint_drops_a_subscriber_that_cannot_accept_it() {
        let id = SessionId::new("session-1");
        let (outbound, _queue) = Outbound::new(512);
        let mut hub = OutputHub::new(64 * 1024, Some(SessionGrid::new(120, 40)), None);
        hub.attach(&id, SubscriberId::next(), &outbound);

        hub.repaint(&id);

        assert!(hub.subscribers.is_empty());
        assert!(outbound.is_closed());
    }

    /// The lead-in of a synthesized repaint, and of nothing a pty writes here.
    const REPAINT_MARK: &[u8] = b"\x1b[?2026h";

    fn repaints_in(frames: &[u8]) -> usize {
        frames
            .windows(REPAINT_MARK.len())
            .filter(|window| *window == REPAINT_MARK)
            .count()
    }

    fn drain_output(queue: &super::OutboundQueue) -> Vec<u8> {
        let mut seen = Vec::new();
        while let Some(frame) = queue.try_recv() {
            if let Frame::Output { bytes, .. } = frame {
                seen.extend(bytes);
            }
        }
        seen
    }

    /// The reserve is taken in the same step that queues the frame, and a
    /// refusal leaves the connection exactly as it was.
    #[test]
    fn try_push_refuses_a_frame_that_would_eat_the_reserve() {
        let id = SessionId::new("try-push");
        let (outbound, queue) = Outbound::new(4096);
        let frame = |bytes: usize| Frame::Output {
            session_id: id.clone(),
            bytes: vec![b'x'; bytes],
        };

        assert!(outbound.try_push(frame(1024), 2048), "1280 + 2048 fits");
        let free = outbound.free_bytes();

        assert!(
            !outbound.try_push(frame(1024), 3000),
            "1280 + 3000 does not"
        );
        assert!(!outbound.is_closed(), "a refusal is not a close");
        assert_eq!(outbound.free_bytes(), free, "the refusal kept its bytes");
        assert!(queue.try_recv().is_some());
        assert!(queue.try_recv().is_none(), "the refused frame was queued");
    }

    /// A `try_push` that refuses must never be visible to anyone else: a
    /// reservation held across the refusal reads to a concurrent [`push`] as a
    /// queue over the bound, and costs a sibling session its connection.
    #[test]
    fn a_refused_try_push_is_invisible_to_a_concurrent_push() {
        const ROUNDS: usize = 10_000;

        let id = SessionId::new("try-push-race");
        let (outbound, queue) = Outbound::new(4096);
        let doomed = outbound.clone();
        let oversized = id.clone();
        let hammer = std::thread::spawn(move || {
            for _ in 0..ROUNDS {
                // 4096 + 256 of overhead: over the bound alone, always refused.
                assert!(
                    !doomed.try_push(
                        Frame::Output {
                            session_id: oversized.clone(),
                            bytes: vec![b'x'; 4096],
                        },
                        0,
                    ),
                    "an oversized frame was queued"
                );
            }
        });
        for _ in 0..ROUNDS {
            assert!(
                outbound.push(Frame::Output {
                    session_id: id.clone(),
                    bytes: vec![b'y'; 512],
                }),
                "a frame that fits an empty queue was refused"
            );
            queue.try_recv().expect("the frame just pushed");
        }
        hammer.join().expect("hammer thread");
        assert!(!outbound.is_closed());
    }

    /// A subscriber whose queue is already closed is pruned, not re-latched:
    /// otherwise a dead connection re-arms the repair every single sweep.
    #[test]
    fn flush_stalled_prunes_a_closed_subscriber_instead_of_deferring() {
        let id = SessionId::new("flush-closed");
        let (outbound, queue) = Outbound::new(OUTBOUND_QUEUE_BYTES);
        let mut hub = OutputHub::new(4096, Some(SessionGrid::new(40, 8)), None);
        hub.attach(&id, SubscriberId::next(), &outbound);
        drop(queue);
        hub.grid.as_mut().expect("daemon grid").defer_repair();

        hub.flush_stalled(&id);

        assert!(hub.subscribers.is_empty(), "the dead subscriber stayed");
        assert!(
            !hub.grid
                .as_mut()
                .expect("daemon grid")
                .take_pending_repair(),
            "a pruned subscriber re-armed the repair"
        );
    }

    /// A splice repaint is synthetic, so a subscriber with no room for it keeps
    /// its stale primary — the state it would have had without splices at all.
    #[test]
    fn a_splice_repaint_a_subscriber_cannot_take_is_skipped_not_fatal() {
        let id = SessionId::new("splice-headroom");
        let (outbound, queue) = Outbound::new(OUTBOUND_QUEUE_BYTES);
        let mut hub = OutputHub::new(64 * 1024, Some(SessionGrid::new(120, 40)), None);
        hub.publish(&id, &vec![b'X'; 120 * 40]);
        let repaint = hub.grid.as_ref().expect("daemon grid").repaint().len() as u64;
        hub.publish(&id, b"\x1b[?1049h\x1b[HAPP");
        hub.attach(&id, SubscriberId::next(), &outbound);
        drain_output(&queue);
        // Room for the reserve and a bare pty frame, but not for the repaint on
        // top of them: the guard has to fail on size, not on a queue too small
        // to hold the reserve at all.
        assert!(outbound.push(Frame::Output {
            session_id: id.clone(),
            bytes: vec![
                b'f';
                (OUTBOUND_QUEUE_BYTES - ATTACH_RESERVE_BYTES - repaint - 256) as usize
            ],
        }));
        assert!(
            outbound.free_bytes() > ATTACH_RESERVE_BYTES,
            "the size branch is not the one under test"
        );

        hub.publish(&id, b"\x1b[?1049lSHELL");

        assert_eq!(
            hub.subscribers.len(),
            1,
            "a synthetic repaint closed a live connection"
        );
        assert!(!outbound.is_closed(), "its queue was closed anyway");
        let seen = drain_output(&queue);
        assert_eq!(repaints_in(&seen), 0, "a repaint was forced past the bound");
        assert!(
            seen.ends_with(b"\x1b[?1049lSHELL"),
            "pty bytes were dropped"
        );
    }

    /// The other half of the same guard: a queue smaller than the reserve can
    /// never take a splice, however small the screen is.
    #[test]
    fn a_splice_repaint_is_skipped_when_the_queue_cannot_hold_the_reserve() {
        let id = SessionId::new("splice-reserve");
        let (outbound, queue) = Outbound::new(4096);
        let mut hub = OutputHub::new(64 * 1024, Some(SessionGrid::new(40, 8)), None);
        hub.publish(&id, b"\x1b[?1049h\x1b[HAPP");
        hub.attach(&id, SubscriberId::next(), &outbound);
        drain_output(&queue);

        hub.publish(&id, b"\x1b[?1049lSHELL");

        assert_eq!(hub.subscribers.len(), 1);
        assert!(!outbound.is_closed());
        assert_eq!(drain_output(&queue), b"\x1b[?1049lSHELL");
    }

    /// A skipped repair is not lost: it re-latches and rides the next splice.
    #[test]
    fn a_skipped_splice_repaint_is_retried_at_the_next_splice() {
        let id = SessionId::new("splice-retry");
        let (outbound, queue) = Outbound::new(OUTBOUND_QUEUE_BYTES);
        let mut hub = OutputHub::new(64 * 1024, Some(SessionGrid::new(40, 8)), None);
        hub.publish(&id, b"shell\x1b[?1049h\x1b[HAPP");
        hub.attach(&id, SubscriberId::next(), &outbound);
        drain_output(&queue);
        assert!(outbound.push(Frame::Output {
            session_id: id.clone(),
            bytes: vec![b'f'; (OUTBOUND_QUEUE_BYTES - ATTACH_RESERVE_BYTES - 256) as usize],
        }));

        hub.publish(&id, b"\x1b[?1049lSHELL");
        assert_eq!(repaints_in(&drain_output(&queue)), 0, "it had no room");

        hub.publish(&id, b"later output");

        assert_eq!(
            repaints_in(&drain_output(&queue)),
            1,
            "the skipped repair never came back"
        );
    }

    /// One synthesis per publish, however many times the chunk toggles.
    #[test]
    fn a_toggle_storm_costs_one_repaint_per_chunk() {
        let id = SessionId::new("splice-storm");
        let (outbound, queue) = Outbound::new(OUTBOUND_QUEUE_BYTES);
        let mut hub = OutputHub::new(64 * 1024, Some(SessionGrid::new(40, 8)), None);
        hub.publish(&id, b"shell");
        hub.attach(&id, SubscriberId::next(), &outbound);
        drain_output(&queue);

        hub.publish(&id, b"\x1b[?1049hA\x1b[?1049lB\x1b[?1049hC\x1b[?1049lD");

        let seen = drain_output(&queue);
        assert_eq!(repaints_in(&seen), 1, "one repaint per publish call");
        assert!(seen.ends_with(b"D"), "pty bytes were dropped");
    }

    /// An app killed mid-synchronized-update writes no further byte, so the
    /// sweep is the only thing left to end the update and repair the screen.
    #[test]
    fn a_sweep_flushes_a_synchronized_update_no_output_will_end() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let (table, id) = seeded_table(dir.path());
        let (outbound, queue) = Outbound::new(OUTBOUND_QUEUE_BYTES);
        {
            let sessions = table.sessions.lock().expect("sessions");
            let hub = sessions.get(&id).expect("the seeded session").hub.clone();
            let mut hub = hub.lock().expect("hub");
            // The seeded row is lost: give it the screen and the live stream a
            // session with a pty behind it would have.
            hub.grid = Some(SessionGrid::new(20, 5));
            hub.ending = None;
            hub.publish(&id, b"primary\x1b[?1049h\x1b[Happ");
            // A combined ESU, which vte's in-sync scan does not match: only the
            // deadline can end this update.
            hub.publish(&id, b"\x1b[?2026h\x1b[?1049l\x1b[?2026;25l");
            hub.attach(&id, SubscriberId::next(), &outbound);
        }
        drain_output(&queue);
        std::thread::sleep(Duration::from_millis(200));

        table.sweep();

        let seen = String::from_utf8_lossy(&drain_output(&queue)).into_owned();
        assert!(
            seen.contains("primary"),
            "the frozen screen was never repaired: {seen:?}"
        );
    }

    /// The prefix and the repaint repairing it ride one frame: two would let a
    /// client render the stale primary in between.
    #[test]
    fn a_splice_reaches_a_subscriber_as_one_frame() {
        let id = SessionId::new("splice-atomic");
        let (outbound, queue) = Outbound::new(OUTBOUND_QUEUE_BYTES);
        let mut hub = OutputHub::new(4096, Some(SessionGrid::new(40, 8)), None);
        hub.publish(&id, b"\x1b[?1049h\x1b[HAPP");
        hub.attach(&id, SubscriberId::next(), &outbound);
        assert!(matches!(queue.try_recv(), Some(Frame::Replay { .. })));

        hub.publish(&id, b"\x1b[?1049lSHELL");

        let mut frames = Vec::new();
        while let Some(frame) = queue.try_recv() {
            let Frame::Output { bytes, .. } = frame else {
                panic!("expected output");
            };
            frames.push(bytes);
        }
        let exit = b"\x1b[?1049l".as_slice();
        assert!(frames[0].starts_with(exit));
        assert!(
            frames[0].len() > exit.len(),
            "the repaint arrived in a frame of its own, after a stale one"
        );
        assert_eq!(frames.last().expect("a frame"), b"SHELL");
    }

    /// The zero-subscriber guard skips the repaint, never the grid feed.
    #[test]
    fn a_splice_with_no_subscribers_still_tracks_the_screen() {
        let id = SessionId::new("splice-detached");
        let mut hub = OutputHub::new(4096, Some(SessionGrid::new(40, 8)), None);
        hub.publish(&id, b"\x1b[?1049h\x1b[HAPP");
        hub.publish(&id, b"\x1b[?1049lSHELL AFTER");

        let (outbound, queue) = Outbound::new(OUTBOUND_QUEUE_BYTES);
        hub.attach(&id, SubscriberId::next(), &outbound);
        let Some(Frame::Replay { bytes, .. }) = queue.try_recv() else {
            panic!("expected replay");
        };
        let mut client = SessionGrid::new(40, 8);
        client.feed(&bytes);
        assert_eq!(
            String::from_utf8_lossy(&client.repaint()),
            String::from_utf8_lossy(&hub.grid.as_ref().expect("daemon grid").repaint()),
        );
    }

    /// Output larger than the attach budget replays only the newest of it and
    /// says so — and a full replay still leaves the queue open behind it.
    #[test]
    fn a_replay_larger_than_the_outbound_budget_is_capped_to_the_newest() {
        const BOUND: u64 = 256 * 1024;

        let id = SessionId::new("session-1");
        let (outbound, queue) = Outbound::new(BOUND);
        let mut hub = OutputHub::new(512 * 1024, None, None);
        // Three 96 KiB chunks: 288 KiB retained whole, over the 128 KiB budget.
        for marker in [b'A', b'B', b'C'] {
            hub.publish(&id, &vec![marker; 96 * 1024]);
        }

        hub.attach(&id, SubscriberId::next(), &outbound);
        let Some(Frame::Replay {
            bytes, truncated, ..
        }) = smol::block_on(queue.recv())
        else {
            panic!("attach did not queue a replay");
        };
        assert!(truncated, "capping is an omission and must say so");
        assert!(!bytes.contains(&b'A'), "the oldest chunk survived the cap");
        assert!(bytes.contains(&b'C'), "the newest chunk was lost");
        assert!(
            !outbound.is_closed(),
            "the replay blew the bound it was capped to fit under"
        );
    }
}
