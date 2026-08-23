//! The session-backend seam: the one interface the layers above use to make a
//! persistent session exist, find it again, attach to it, and take it down.
//!
//! [`crate::WorkspaceLifecycleService`] and everything over it — the sidebar,
//! the create modal, the terminal pane — talk to a [`SessionBackend`] and know
//! nothing about how the processes are actually kept alive.
//!
//! **The tmux implementation is interim** (decided 2026-08-02/03; anamnesis
//! marks #821 daemon, #822 neighbour placement, #824 no new tmux). The
//! ADE-native session daemon is the second implementation and, since
//! 2026-08-03, the default one ([`crate::DaemonBackend`]); tmux is retained
//! only behind [`crate::WorkspaceLifecycleService::with_backend`] until the
//! operator has accepted the daemon on a desktop build.
//!
//! Two of the shapes below were tmux's to keep, and one of them has since gone:
//!
//! - [`SessionBackend::attach`] hands back the argv a client runs, not a PTY
//!   stream with scrollback replayed as data. tmux's replay is a screen repaint
//!   performed by a real tmux client, and the PTY belongs to Zed's terminal.
//!   It also takes a whole [`SessionSpec`], not just an id, because
//!   attach-or-create needs the directory it may have to create the session in.
//!   **The daemon keeps the argv shape, and the ssh transport did not change
//!   that** — its argv runs `ade-daemon attach`, which is our own client and
//!   dies when the terminal closes, i.e. a detach. The worry that a remote
//!   host would force a stream-shaped attach (a per-session client process
//!   meaning a per-session ssh connection) did not materialise: a remote
//!   workspace's client is *local* too, pointed at the host's forwarded
//!   socket, so it is one more channel on the host's single ssh connection.
//! - There is no `write` / `resize`: the terminal that ran the attach argv owns
//!   the PTY, so nothing in this crate has ever written to a session. They
//!   arrive with the stream attach.
//! - [`SessionBackend::status_delivery`] used to stand in for pushed status
//!   events, because tmux has no push channel and can only answer "pull, this
//!   often". The daemon *does* push, so the answer is now a genuine choice
//!   between [`StatusDelivery::Poll`] and [`StatusDelivery::Push`] — and the
//!   pushed half is [`SessionBackend::subscribe_status`].
//!
//! **Blocking.** Implementations may shell out, and every method here is
//! synchronous. Callers must run them on a background executor.

use crate::WorkspaceStatus;
use crate::daemon_backend::DaemonUpgradeOutcome;
use ade_session::LayoutDoc;
use anyhow::{Result, bail};
use smol::channel::Receiver;
use std::{
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

/// A session's identity as its backend knows it.
///
/// The tmux backend's ids are tmux session names (`ade-<slug>-<id6>`, see
/// [`crate::tmux_session_name`]), which is what the registry caches in
/// `terminal_session_id`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(String);

impl SessionId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl From<String> for SessionId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

impl From<&str> for SessionId {
    fn from(id: &str) -> Self {
        Self(id.to_owned())
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a backend needs to bring a session into existence, or to attach to one
/// that may not exist yet.
///
/// Deliberately narrow: the daemon's spec grows agent kind and instance
/// identity, but tmux takes a name and a working directory, so that is all this
/// carries today.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSpec {
    /// The id the session is known by. Derived by the caller, not minted by the
    /// backend — see [`SessionBackend::create`].
    pub id: SessionId,
    /// Where the session's processes start.
    pub directory: PathBuf,
}

impl SessionSpec {
    pub fn new(id: impl Into<SessionId>, directory: impl Into<PathBuf>) -> Self {
        Self {
            id: id.into(),
            directory: directory.into(),
        }
    }
}

/// What an attach-or-create landed on: the argv a client runs, and the
/// backend's own id for the session behind it.
///
/// **The id is what makes the tab nameable.** A terminal opened on an argv
/// alone carries nothing that says which session it is, so it cannot be
/// captured into a [`LayoutDoc`] and a window holding one could not sync. The
/// one call that may *create* a session therefore has to say which one it made
/// — see [`crate::session_task_id`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attached {
    /// The backend's own id, which is what a layout names and what
    /// [`SessionBackend::attach_session`] takes. tmux does not mint ids, so
    /// there it is the seam id itself.
    pub session_id: String,
    pub argv: Vec<String>,
}

/// One live session, as the backend sees it — the source of truth a registry is
/// only a cache of.
///
/// A struct rather than a bare id because the daemon's listing carries status
/// and agent identity with it; tmux can report only the name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: SessionId,
}

/// One workspace the backend holds, as the source of truth describes it.
///
/// The adoption half of "the backend is the source of truth, the registry is a
/// cache": a client whose registry has never heard of a workspace — a fresh
/// install, a second machine, a database that was thrown away — can only learn
/// it exists from here. [`SessionInfo`] cannot stand in for it, because a
/// workspace nobody has a *session* in still has to become a row.
///
/// Only the fields a registry row cannot derive: the id everything is keyed by,
/// the display name (which the user may have changed, so it cannot be
/// re-derived), the root on the backend's own host, and when it was made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendWorkspace {
    /// The workspace id the backend knows it by — the string this crate caches
    /// as `terminal_session_id` and hands back to every later call.
    pub id: String,
    pub name: String,
    /// Resolved on the backend's host, so a remote one is a path over there.
    pub project_root: String,
    /// Unix seconds.
    pub created_at: u64,
}

/// How a backend reports that a session's status changed.
///
/// The daemon pushes; tmux cannot, so it asks to be polled and the interval is
/// its to choose — it is the one that knows what a tick costs. Adding the
/// pushed variant is what made every caller's poll loop a compile error, which
/// is the point of naming this at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusDelivery {
    /// Status has to be pulled: call [`SessionBackend::list`] this often.
    Poll { interval: Duration },
    /// Status arrives on its own: take the stream from
    /// [`SessionBackend::subscribe_status`] and stop timing anything.
    ///
    /// Carries nothing — the stream is fetched separately rather than held in
    /// the variant, so that asking *how* status arrives stays a cheap, repeated
    /// question while *subscribing* stays a single deliberate act with a
    /// failure mode.
    Push,
}

/// One thing that happened to one session, pushed by a [`StatusDelivery::Push`]
/// backend.
///
/// Enough to keep a list of rows current without re-listing: which session, and
/// whether it appeared, changed status, lost its process, or went away.
///
/// **A workspace's sessions are aggregated into one of these, not reported one
/// by one.** The id is the seam's — a workspace — and a workspace may hold
/// several backend sessions at once, so the change is the *workspace's*: it
/// goes [`SessionChange::Exited`] or [`SessionChange::Removed`] only when the
/// last of them does, and reports plain [`SessionChange::Status`] while any
/// sibling is still running.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusEvent {
    /// The session, named as this crate names it — the same id
    /// [`SessionBackend::list`] reports, not whatever the backend calls it
    /// underneath.
    pub id: SessionId,
    pub change: SessionChange,
}

impl StatusEvent {
    pub fn new(id: impl Into<SessionId>, change: SessionChange) -> Self {
        Self {
            id: id.into(),
            change,
        }
    }
}

/// What happened to a session.
///
/// The statuses are already mapped into [`WorkspaceStatus`], the enum the
/// sidebar's dot is coloured from: a backend's own vocabulary (the daemon's
/// working / needs-input / idle) is per-*agent* telemetry, which is deliberately
/// deferred, and the layers above this seam only ever knew session-level state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionChange {
    /// A session that did not exist a moment ago, with the status it was born
    /// in — so a subscriber that has not listed anything still knows what to
    /// draw.
    Created(WorkspaceStatus),
    /// It is still there and its status moved.
    Status(WorkspaceStatus),
    /// Its process is gone. The session itself is **not**: a row outliving its
    /// process is what makes a crashed agent visible instead of vanishing.
    Exited,
    /// The session is gone from the backend entirely — the only thing that ever
    /// does this is an explicit kill.
    Removed,
}

impl SessionChange {
    /// The status this change implies, for the changes that carry one.
    pub fn status(&self) -> Option<WorkspaceStatus> {
        match self {
            Self::Created(status) | Self::Status(status) => Some(*status),
            // An exited session's *session* status is the caller's to decide:
            // the row is disconnected, but whether the workspace is stopped or
            // waiting to be recreated is policy, not backend fact.
            Self::Exited | Self::Removed => None,
        }
    }
}

/// What one workspace's arrangement looks like on the backend right now.
///
/// The revision is half the value: it is the guard the next write has to beat,
/// so a caller that renders a layout must keep the number it came with. See
/// [`SessionBackend::update_layout`].
#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceLayout {
    pub layout: LayoutDoc,
    pub rev: u64,
}

/// A layout the backend has accepted, pushed to everyone watching.
///
/// **Including the client that wrote it.** The daemon excludes the *connection*
/// that sent the update, and this crate's event stream is a different
/// connection from its control one — so a client sees its own writes come back
/// and has to recognise them by `rev` rather than assume it never will.
#[derive(Clone, Debug, PartialEq)]
pub struct LayoutEvent {
    pub workspace_id: String,
    pub layout: LayoutDoc,
    pub rev: u64,
}

/// One thing a backend pushed, whatever it was about.
///
/// **One stream, not two.** A host has a single push channel and the daemon
/// already multiplexes every kind of event onto it; a second subscription to
/// demultiplex them would spend a whole connection on sorting.
#[derive(Clone, Debug, PartialEq)]
pub enum DaemonEvent {
    Session(StatusEvent),
    Layout(LayoutEvent),
    /// The daemon still has this workspace, but its persisted incarnation was
    /// replaced while the event stream was disconnected. Unlike removal, the
    /// window keeps owning it; its revision gate and layout are reset together.
    WorkspaceReset(LayoutEvent),
    /// A whole workspace is gone: every session in it was killed and the
    /// backend's record of it deleted. The **only** thing that produces this is
    /// an explicit [`SessionBackend::kill_workspace`] — by this client or by
    /// another one — so a client that receives it stops syncing that workspace
    /// rather than treating it as a layout it lost a race over.
    WorkspaceRemoved {
        workspace_id: String,
    },
}

/// One pushed event paired with the daemon connection that produced it.
#[derive(Clone, Debug, PartialEq)]
pub struct IdentifiedDaemonEvent {
    pub daemon_id: Option<String>,
    pub event: DaemonEvent,
}

/// One workspace-level thing that happened, as the layers above the seam see
/// it: the merged layout stream carries removals too.
///
/// **One stream, because it is one ordering.** A removal that overtook the
/// layout it removes would have a window pushing panes back into a workspace
/// the daemon has already forgotten, which is how a killed workspace comes back
/// from the dead. Every event keeps the backend key it arrived on because
/// workspace and session ids are scoped to one host.
#[derive(Clone, Debug, PartialEq)]
pub enum WorkspaceEvent {
    Layout {
        remote_host: Option<String>,
        daemon_id: Option<String>,
        event: LayoutEvent,
    },
    Reset {
        remote_host: Option<String>,
        daemon_id: Option<String>,
        event: LayoutEvent,
    },
    Removed {
        remote_host: Option<String>,
        daemon_id: Option<String>,
        workspace_id: String,
    },
}

/// A listing paired with the identity of the connection that produced it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identified<T> {
    /// `None` for an unnamed or identityless backend.
    pub daemon_id: Option<String>,
    pub items: Vec<T>,
}

/// Persistence, attach, and status for one host's sessions. Nothing else — no
/// multiplexing, no layout, no rendering; those are the editor's.
///
/// The layout methods are the exception that proves it: the backend *stores* a
/// layout document and hands it back, and never interprets one. Translating it
/// into panes is [`crate::layout`]'s job, on this side of the seam.
///
/// `expected_daemon_id` fences persisted-row operations. `None` remains
/// permissive for legacy and unclaimed rows.
pub trait SessionBackend: Send + Sync {
    /// Creates the session detached, so its processes exist before and after
    /// any client is attached. Fails if it already exists.
    ///
    /// Returns the id the session ended up with. tmux does not mint ids — the
    /// caller derives the name and gets it back — but the daemon does, so the
    /// created id comes from here rather than being assumed.
    fn create(&self, spec: &SessionSpec, expected_daemon_id: Option<&str>) -> Result<SessionId>;

    /// [`SessionBackend::create`] with the identity of the connection that
    /// created the session.
    fn create_identified(
        &self,
        spec: &SessionSpec,
        expected_daemon_id: Option<&str>,
    ) -> Result<(SessionId, Option<String>)> {
        let session = self.create(spec, expected_daemon_id)?;
        Ok((session, self.instance_id()))
    }

    /// Creates one **more** session inside a workspace that already exists, and
    /// answers with the backend's own id for it.
    ///
    /// Not [`SessionBackend::create`] under another name. That one makes a
    /// workspace's *first* session and refuses a second, because two rows
    /// claiming to be "the" session of a workspace is the state the registry
    /// cannot represent; it also reaps the tombstones a replacement supersedes.
    /// Neither belongs here: this is the deliberate second, third and fourth
    /// session — the extra terminals a window holds — and the layout document is
    /// what remembers them, one `Tab::Terminal` each.
    ///
    /// The id is the backend's, not the seam's, because that is what a layout
    /// names and what [`SessionBackend::attach_session`] takes.
    fn create_session_in_workspace(
        &self,
        _workspace_id: &str,
        _cwd: &Path,
        _expected_daemon_id: Option<&str>,
    ) -> Result<String> {
        bail!("this session backend holds one session per workspace")
    }

    /// Every live session this app owns. Sessions belonging to anything else
    /// are not listed, and a backend that is not running yet reports no
    /// sessions rather than an error.
    fn list(&self) -> Result<Vec<SessionInfo>>;

    /// [`SessionBackend::list`] paired with its producing identity.
    fn list_identified(&self) -> Result<Identified<SessionInfo>> {
        Ok(Identified {
            daemon_id: self.instance_id(),
            items: self.list()?,
        })
    }

    /// Every workspace this backend holds, whether or not the caller has ever
    /// heard of one.
    ///
    /// [`SessionBackend::list`] answers "which of the sessions I already know
    /// about are alive"; this answers "what is there", which is the only
    /// question a client with an empty registry can usefully ask. Without it
    /// the cache decides what the source of truth is allowed to contain.
    ///
    /// **An empty list, not an error, for a backend with no workspaces of its
    /// own.** Adoption is a pass over every host, and tmux having nothing to
    /// contribute is not a host failure — an error here reads as "this host
    /// could not be asked", and the layers above treat it that way.
    fn list_workspaces(&self) -> Result<Vec<BackendWorkspace>> {
        Ok(Vec::new())
    }

    /// [`SessionBackend::list_workspaces`] paired with its producing identity.
    fn list_workspaces_identified(&self) -> Result<Identified<BackendWorkspace>> {
        Ok(Identified {
            daemon_id: self.instance_id(),
            items: self.list_workspaces()?,
        })
    }

    /// Whether one session is alive: [`SessionBackend::list`] narrowed to a
    /// single id, for the probe of a single workspace.
    ///
    /// Never creates anything — the answer is a report, not a repair.
    fn exists(&self, id: &SessionId, expected_daemon_id: Option<&str>) -> Result<bool>;

    /// The argv a client runs to get onto this session, creating it first if it
    /// is gone (see the module docs on why this is an argv and takes a spec).
    ///
    /// Attach-or-create, so opening a pane twice is idempotent — and so this is
    /// *not* a status check: it will silently bring a dead session back.
    ///
    /// The session it landed on comes back with the argv ([`Attached`]): the
    /// caller cannot know which session a create made, and without it the tab
    /// it opens is not something a layout can name.
    fn attach(&self, spec: &SessionSpec, expected_daemon_id: Option<&str>) -> Result<Attached>;

    /// Drops every client attached to the session. The session and everything
    /// running in it keep going: this is what closing means, per the invariant
    /// that closing detaches and never kills.
    ///
    /// Not in the daemon sketch, but load-bearing today. Detaching a session
    /// nobody is attached to is a no-op, not an error.
    fn detach(&self, id: &SessionId) -> Result<()>;

    /// Kills the session and every process in it.
    ///
    /// Destructive and irreversible; only ever reached from a control that says
    /// so. Killing a session that is already gone succeeds, since that is the
    /// state the caller asked for.
    fn kill(&self, id: &SessionId, expected_daemon_id: Option<&str>) -> Result<()>;

    /// Kills a workspace's sessions before immediately recreating them.
    ///
    /// The directory lets remote backends clean up terminal process groups
    /// left behind by an older daemon. Other backends need no recovery beyond
    /// their ordinary kill.
    fn reset_workspace_sessions(
        &self,
        id: &SessionId,
        _directory: &Path,
        expected_daemon_id: Option<&str>,
    ) -> Result<()> {
        self.kill(id, expected_daemon_id)
    }

    /// How this backend expects status to be obtained. See [`StatusDelivery`].
    fn status_delivery(&self) -> StatusDelivery;

    /// The stream of [`DaemonEvent`]s, for a backend that pushes.
    ///
    /// **Paired with [`SessionBackend::status_delivery`], and meaningful only
    /// with it.** A [`StatusDelivery::Poll`] backend has nothing to push and
    /// says so here; a caller that matched `Poll` never reaches this, which is
    /// exactly why the two live together on one trait rather than the receiver
    /// being handed out by the enum.
    ///
    /// Each call opens its own stream — one connection's worth — so callers fan
    /// one subscription out rather than taking several. Dropping every receiver
    /// is the only unsubscribe there is.
    fn subscribe_events(&self) -> Result<Receiver<IdentifiedDaemonEvent>> {
        bail!("this session backend reports status by polling, not by pushing")
    }

    /// The argv a client runs to get onto a session that **already exists**,
    /// named by the backend's own id.
    ///
    /// The counterpart to [`SessionBackend::attach`], and deliberately not the
    /// same call: this one never creates. A layout names sessions the backend
    /// minted, and rendering one must not conjure a shell for a tab whose
    /// session has died — the daemon prunes those from the document instead.
    fn attach_session(
        &self,
        _session_id: &str,
        _expected_daemon_id: Option<&str>,
    ) -> Result<Vec<String>> {
        bail!("this session backend has no sessions of its own to attach to")
    }

    /// The workspace's stored layout and the revision guarding the next write.
    ///
    /// Errors — including "no such workspace" — mean the caller has nothing to
    /// render and should fall back to whatever it did before layouts existed.
    fn open_workspace(
        &self,
        _workspace_id: &str,
        _expected_daemon_id: Option<&str>,
    ) -> Result<WorkspaceLayout> {
        bail!("this session backend does not store workspace layouts")
    }

    /// Stores a new layout, which must be at `rev` — the revision the caller
    /// last saw, plus one.
    ///
    /// Last writer wins, guarded by that revision: a stale one is refused, and
    /// the caller's answer is to re-fetch and re-render rather than to retry.
    fn update_layout(
        &self,
        _workspace_id: &str,
        _layout: &LayoutDoc,
        _rev: u64,
        _expected_daemon_id: Option<&str>,
    ) -> Result<()> {
        bail!("this session backend does not store workspace layouts")
    }

    /// Kills one session named by the backend's own id.
    ///
    /// [`SessionBackend::kill`] takes every session of a *workspace*; this
    /// takes the one a closed tab was showing. Destructive either way — only
    /// ever reached from a control that says so.
    fn kill_session(&self, _session_id: &str, _expected_daemon_id: Option<&str>) -> Result<()> {
        bail!("this session backend has no sessions of its own to kill")
    }

    /// Gives the workspace a new display name, keeping its id.
    ///
    /// The backend is the source of truth for what a workspace is called, so a
    /// rename goes here first and the registry follows; a caller that cannot
    /// reach the backend has not renamed anything and must say so rather than
    /// writing a name only this machine would ever see.
    ///
    /// A backend with no workspace of its own says so here, exactly like
    /// [`SessionBackend::kill_workspace`] — there is no local fallback, because
    /// there is nothing to fall back *to*.
    fn rename_workspace(
        &self,
        _workspace_id: &str,
        _name: &str,
        _expected_daemon_id: Option<&str>,
    ) -> Result<()> {
        bail!("this session backend has no workspaces of its own to rename")
    }

    /// Kills every session in the workspace **and deletes the workspace
    /// record**, layout and all.
    ///
    /// The one workspace-level kill (operator ruling, 2026-08-04), and the
    /// difference from [`SessionBackend::kill`] is the record: killing the
    /// sessions leaves a workspace whose layout names dead tabs, while this
    /// leaves nothing. Every other client watching is told, so none of them
    /// goes on writing layouts into a workspace that is gone.
    ///
    /// A backend without a workspace of its own says so here rather than
    /// pretending — the layer above falls back to taking the sessions.
    fn kill_workspace(&self, _workspace_id: &str, _expected_daemon_id: Option<&str>) -> Result<()> {
        bail!("this session backend has no workspaces of its own to kill")
    }

    /// Replace the daemon binary on this backend's host, right now, because a
    /// human asked for it.
    ///
    /// Not something the seam does on its own: a backend upgrades itself
    /// opportunistically at connect time, and this is the manual way past a
    /// host whose daemon that never catches free. A backend with no separate
    /// binary to replace — a local one, tmux — says so rather than pretending
    /// it succeeded.
    fn upgrade_daemon(&self) -> Result<DaemonUpgradeOutcome> {
        bail!("this session backend has no host daemon to upgrade")
    }

    /// Whether this backend's host runs a daemon older than the client would
    /// deploy, as far as anything already knows.
    ///
    /// Read-only and non-blocking: it reports what earlier probes recorded and
    /// contacts nothing, because the caller is a UI deciding whether to draw
    /// the "upgrade host daemon" control. A backend with no separate daemon
    /// binary answers `false` — there is nothing to be behind.
    fn daemon_stale(&self) -> bool {
        false
    }

    /// Ask to be told when [`Self::daemon_stale`] starts answering something
    /// else.
    ///
    /// Without this the answer is only ever read by a render, and a render only
    /// happens for other reasons — so a probe that finds a host behind while
    /// the window sits still would put no arrow on screen until the user
    /// happened to touch something. A backend with nothing to be behind never
    /// calls the observer, which is the correct behaviour rather than a stub.
    ///
    /// The observer runs **on whichever thread found out**, which is a
    /// background one holding backend locks. It must only hand the fact
    /// onwards — post to a channel, wake a task — and must never call back into
    /// the backend.
    fn observe_daemon_freshness(&self, _observer: DaemonFreshnessObserver) {}

    /// This host's daemon identity, as of the last successful handshake —
    /// `None` before any handshake or for a backend with no identity of its
    /// own. Read-only and non-blocking, like [`Self::daemon_stale`].
    fn instance_id(&self) -> Option<String> {
        None
    }
}

/// Run when a backend learns that its host's daemon is, or is no longer,
/// behind this client's. See [`SessionBackend::observe_daemon_freshness`] for
/// what it may do.
pub type DaemonFreshnessObserver = Arc<dyn Fn() + Send + Sync>;
