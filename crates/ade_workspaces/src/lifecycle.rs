//! Workspace lifecycle: create, open, reconnect, stop, and (only when asked by
//! name) kill.
//!
//! This is the policy layer over a [`SessionBackend`] and
//! [`AdeWorkspaceRegistry`]. The backend is the source of truth for whether a
//! session is alive; the registry is a cache, and this service is what keeps
//! the cache honest.
//!
//! **Nothing here knows what keeps a session alive.** Every call goes through
//! the seam in [`crate::session_backend`]; a backend is named exactly once
//! below, in [`WorkspaceLifecycleService::backend_for_host`] — the ADE session
//! daemon since 2026-08-03, tmux before it.
//!
//! Two rules run through everything here:
//!
//! - **Closing detaches, never kills.** [`WorkspaceLifecycleService::stop_workspace`]
//!   drops the clients and leaves the session — and everything running in it —
//!   alive. Destructive controls reach
//!   [`WorkspaceLifecycleService::kill_workspace_session`],
//!   [`WorkspaceLifecycleService::kill_workspace`], or
//!   [`WorkspaceLifecycleService::reset_workspace_sessions`] by name.
//! - **A dead session is surfaced, never silently recreated.**
//!   [`WorkspaceLifecycleService::probe`] reports [`SessionState::Dead`] and
//!   keeps the dead session's name so the UI can say what died. Bringing it
//!   back takes an explicit [`WorkspaceLifecycleService::recreate_session`].
//!
//! **One backend per host, created on first use.** A workspace's `remote_host`
//! selects the backend its sessions belong to; `None` is this machine. The map
//! is kept, not rebuilt, because a host's backend owns that host's single ssh
//! connection — making a second would break the constant-connections-per-host
//! invariant. Nothing is contacted until an operation needs it, so a registry
//! full of hosts costs nothing at startup.
//!
//! **One host being down must not empty the sidebar.** [`Reconciled`] carries
//! per-host failures beside the rows rather than failing the pass, and the
//! workspaces of an unreachable host come back as [`SessionState::Unknown`]
//! with their stored status untouched — a lie about a host would be worse than
//! an admission that it could not be asked.
//!
//! **Session-level status only.** `creating` / `running` / `disconnected` /
//! `stopped` / `error` describe the *session*. Per-agent telemetry — the
//! working / idle / needs-input dots — is deliberately **deferred** to a later
//! phase. Do not grow it here; it wants the backend's own status channel.
//!
//! **Blocking.** Every backend call underneath is synchronous. Run this service
//! on a background executor.

use crate::{
    AdeWorkspace, AdeWorkspaceRegistry, Attached, BackendWorkspace, DaemonBackend, DaemonEvent,
    DaemonUpgradeOutcome, IdentifiedDaemonEvent, SESSION_PREFIX, SessionBackend, SessionId,
    SessionSpec, StatusDelivery, StatusEvent, WorkspaceEvent, WorkspaceId, WorkspaceLayout,
    WorkspaceStatus, now_whole_seconds, project_id_from_identity, project_id_from_path,
};
use ade_session::LayoutDoc;
use anyhow::{Context as _, Result, bail};
use smol::{
    channel::{Receiver, Sender},
    lock::{Mutex as AsyncMutex, MutexGuardArc},
};
use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use time::OffsetDateTime;

/// What the session backend says about a workspace's session, as distinct from
/// what the registry last recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    /// The session exists and can be attached to.
    Alive,
    /// A session was created for this workspace and is gone — the machine
    /// rebooted, the server was killed, someone ran `tmux kill-server`. Shown
    /// to the user; never repaired behind their back.
    Dead,
    /// No session has ever been created for this workspace.
    NeverCreated,
    /// The workspace's host could not be asked. Nothing is claimed about the
    /// session: it is not reported dead, and the registry's status is left
    /// exactly as it was. The reason is in [`Reconciled::host_errors`].
    Unknown,
}

/// One row of the merged runtime view: a workspace this client has used, or one
/// a host's daemon holds that it has not.
///
/// **[`AdeWorkspace`] stays persisted-only.** A discovered workspace has no
/// registry row, so it has no [`WorkspaceId`] either; minting a fake one would
/// hand a key to the many lifecycle calls that immediately look it up in
/// sqlite. Its identity is its host plus the wire id.
#[derive(Clone, Debug, PartialEq)]
pub enum WorkspaceEntry {
    Persisted(AdeWorkspace, SessionState),
    Discovered {
        remote_host: Option<String>,
        workspace: BackendWorkspace,
        state: SessionState,
    },
}

impl WorkspaceEntry {
    pub fn remote_host(&self) -> Option<&str> {
        match self {
            Self::Persisted(workspace, _) => workspace.remote_host.as_deref(),
            Self::Discovered { remote_host, .. } => remote_host.as_deref(),
        }
    }

    /// The id the host's daemon knows this workspace by.
    pub fn wire_id(&self) -> String {
        match self {
            Self::Persisted(workspace, _) => workspace.daemon_workspace_id(),
            Self::Discovered { workspace, .. } => workspace.id.clone(),
        }
    }

    /// The sidebar's grouping key. A discovery has no stored one, so it is
    /// derived from the root only for legacy daemon records.
    pub fn project_id(&self) -> String {
        match self {
            Self::Persisted(workspace, _) => workspace.project_id.clone(),
            Self::Discovered { workspace, .. } => workspace
                .project_id
                .clone()
                .unwrap_or_else(|| project_id_from_path(Path::new(&workspace.project_root))),
        }
    }

    /// Stable project-group identity, separate from the human label.
    pub fn project_identity(&self) -> String {
        match self {
            Self::Persisted(workspace, _) => workspace.project_identity(),
            Self::Discovered { workspace, .. } => workspace
                .project_identity
                .clone()
                .unwrap_or_else(|| workspace.project_root.clone()),
        }
    }

    /// What to show this workspace as. See [`display_name_for`].
    pub fn name(&self) -> String {
        match self {
            Self::Persisted(workspace, _) => workspace.name.clone(),
            Self::Discovered { workspace, .. } => {
                display_name_for(&workspace.name, &self.project_id())
            }
        }
    }

    pub fn repository_path(&self) -> PathBuf {
        match self {
            Self::Persisted(workspace, _) => workspace.repository_path.clone(),
            Self::Discovered { workspace, .. } => PathBuf::from(&workspace.project_root),
        }
    }

    pub fn state(&self) -> SessionState {
        match self {
            Self::Persisted(_, state) | Self::Discovered { state, .. } => *state,
        }
    }

    /// The registry row behind this entry, for a caller that only acts on rows.
    pub fn persisted(&self) -> Option<(&AdeWorkspace, SessionState)> {
        match self {
            Self::Persisted(workspace, state) => Some((workspace, *state)),
            Self::Discovered { .. } => None,
        }
    }
}

/// The record a caller named is not in the host's listing any more.
///
/// Typed because it is not a failure to retry: something removed the workspace
/// between the listing the user clicked and the confirmation, so the caller
/// drops the entry rather than reporting a broken host.
#[derive(Debug)]
pub(crate) struct WorkspaceGone {
    pub remote_host: Option<String>,
    pub wire_id: String,
}

impl std::fmt::Display for WorkspaceGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} no longer holds workspace {}",
            host_label(self.remote_host.as_deref()),
            self.wire_id
        )
    }
}

impl std::error::Error for WorkspaceGone {}

/// What one reconciliation pass found.
#[derive(Debug, Default)]
pub struct Reconciled {
    /// Everything the pass found: the rows this client uses first, in recency
    /// order, then what the hosts hold that it does not. A host that failed
    /// keeps the discoveries of its last successful listing.
    pub entries: Vec<WorkspaceEntry>,
    /// `(host, message)` for each host that could not be reached — the string
    /// is the host as the registry spells it, or `local` for this machine.
    /// Surfaced beside the rows, never in place of them.
    pub host_errors: Vec<(String, String)>,
}

/// How a host reads in an error line. The local backend has no name of its own.
fn host_label(host: Option<&str>) -> String {
    host.unwrap_or("local").to_owned()
}

fn next_project_scope_rev(revision: u64) -> Result<u64> {
    revision
        .checked_add(1)
        .context("project scope revision overflowed")
}

/// The registry row for a daemon record this client has just opened. Written
/// only by [`WorkspaceLifecycleService::confirm_discovered`]; a listing on its
/// own never reaches here.
///
/// **`terminal_session_id` is the whole point.** It carries the backend's id,
/// which is what [`AdeWorkspace::daemon_workspace_id`] returns once recorded —
/// so this row addresses the workspace it was confirmed from, rather than a
/// freshly derived name the daemon has never heard of. The `id` is minted here
/// because it is this client's own key and nothing else refers to it.
///
/// `created_at` is the backend's, because that is when the workspace began;
/// `opened_at` is when *this client* used it, which is what the recency order
/// and a reattach both read.
///
/// `branch` is left unset: the backend records a root, not a checkout state, and
/// guessing it from the path would be a claim nothing verified.
fn row_for_record(
    workspace: &BackendWorkspace,
    host: Option<&str>,
    daemon_id: Option<&str>,
    opened_at: OffsetDateTime,
) -> AdeWorkspace {
    let repository_path = PathBuf::from(&workspace.project_root);
    let project_id = workspace
        .project_id
        .clone()
        .unwrap_or_else(|| project_id_from_path(&repository_path));
    // Whole seconds, like everything else the registry stores; a backend that
    // reports a time no calendar has is given this client's clock rather than
    // failing the record over a timestamp.
    let created_at = i64::try_from(workspace.created_at)
        .ok()
        .and_then(|seconds| OffsetDateTime::from_unix_timestamp(seconds).ok())
        .unwrap_or_else(now_whole_seconds);
    AdeWorkspace {
        id: WorkspaceId::new(),
        name: display_name_for(&workspace.name, &project_id),
        project_id,
        project_identity: workspace.project_identity.clone(),
        repository_path,
        project_scope_rev: workspace.project_scope_rev,
        branch: None,
        remote_host: host.map(str::to_owned),
        remote_workspace_path: None,
        terminal_session_id: Some(workspace.id.clone()),
        daemon_id: daemon_id.map(str::to_owned),
        // Nothing has probed its sessions yet, and this is the status a
        // workspace nobody is attached to has. The next reconciliation
        // corrects it.
        status: WorkspaceStatus::Disconnected,
        created_at,
        last_opened_at: opened_at,
    }
}

/// The daemon a decision is scoped to. Nameless daemons fall back to
/// the route used to reach them.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum DaemonKey {
    Instance(String),
    Host(Option<String>),
}

fn daemon_key(daemon_id: Option<&str>, host: Option<&str>) -> DaemonKey {
    match daemon_id {
        Some(daemon_id) => DaemonKey::Instance(daemon_id.to_owned()),
        None => DaemonKey::Host(host.map(str::to_owned)),
    }
}

/// Whether a row belongs to the daemon reached at `host`. A row that carries an
/// identity is judged by it alone; a legacy row has only the route it was
/// reached by.
fn row_is_on(row: &AdeWorkspace, daemon: &DaemonKey, host: Option<&str>) -> bool {
    match (row.daemon_id.as_deref(), daemon) {
        (Some(persisted), DaemonKey::Instance(instance)) => persisted == instance,
        (Some(_), DaemonKey::Host(_)) => false,
        (None, _) => row.remote_host.as_deref() == host,
    }
}

/// A failed listing cannot disprove an unfenced row on this exact route.
fn row_could_be_on(row: &AdeWorkspace, daemon: &DaemonKey, host: Option<&str>) -> bool {
    match (row.daemon_id.as_deref(), daemon) {
        (Some(persisted), DaemonKey::Instance(instance)) => persisted == instance,
        (Some(_), DaemonKey::Host(_)) => row.remote_host.as_deref() == host,
        (None, _) => row.remote_host.as_deref() == host,
    }
}

/// Total recency order for a deterministic reconnect decision.
fn most_recently_opened<'a>(
    rows: impl Iterator<Item = &'a AdeWorkspace>,
) -> Option<&'a AdeWorkspace> {
    rows.min_by(|a, b| {
        b.last_opened_at
            .cmp(&a.last_opened_at)
            .then_with(|| a.id.cmp(&b.id))
    })
}

fn instance_of(daemon: &DaemonKey) -> Option<&str> {
    match daemon {
        DaemonKey::Instance(instance) => Some(instance),
        DaemonKey::Host(_) => None,
    }
}

/// One host spelling's share of a reconcile pass, held until every spelling has
/// been tried — see [`WorkspaceLifecycleService::reconcile`].
struct HostOutcome {
    host: Option<String>,
    /// Which daemon answered, resolved *after* the listing: the handshake that
    /// names it is the one this pass's first request opened.
    daemon: DaemonKey,
    /// The live session ids behind it; `None` when they could not be listed,
    /// which is what makes this host's entries read [`SessionState::Unknown`].
    live: Option<HashSet<SessionId>>,
    /// What the daemon holds; `None` when it could not be asked.
    held: Option<Vec<BackendWorkspace>>,
}

/// A daemon listing whose route and instance decision locks are still held.
struct StableListing {
    _route_decision: MutexGuardArc<()>,
    _instance_decision: Option<MutexGuardArc<()>>,
    daemon: DaemonKey,
    held: Result<Vec<BackendWorkspace>>,
}
/// A persisted daemon identity is exclusive. A legacy row has only its exact
/// route; matching it across aliases could join two different nameless daemons.
fn is_the_same_daemons_row(
    candidate: &AdeWorkspace,
    instance_id: Option<&str>,
    host: Option<&str>,
    workspace: &BackendWorkspace,
) -> bool {
    if candidate.terminal_session_id.as_deref() != Some(workspace.id.as_str()) {
        return false;
    }
    match (candidate.daemon_id.as_deref(), instance_id) {
        (Some(persisted), Some(current)) => persisted == current,
        (Some(_), None) => false,
        (None, _) => candidate.remote_host.as_deref() == host,
    }
}

/// What to call a workspace the backend named: the checkout it is rooted at,
/// unless the backend's name is one a *person* chose.
///
/// A workspace created by this app is named for the machine — the daemon's
/// record is keyed by `ade-<slug>-<id6>` and the record's name follows it — and
/// showing that in the sidebar would be showing the user a session id where a
/// project name belongs. A name that does not fit the derived shape was typed by
/// somebody (see [`WorkspaceLifecycleService::rename_workspace`]) and must
/// survive verbatim; losing a rename is worse than showing a slug.
fn display_name_for(backend_name: &str, project_id: &str) -> String {
    if is_derived_workspace_name(backend_name) {
        project_id.to_owned()
    } else {
        backend_name.to_owned()
    }
}

/// Whether a name has the shape [`crate::tmux_session_name`] produces:
/// `ade-<slug>-<six hex>`.
fn is_derived_workspace_name(name: &str) -> bool {
    let Some((head, id6)) = name.rsplit_once('-') else {
        return false;
    };
    head.strip_prefix(SESSION_PREFIX)
        .is_some_and(|slug| slug.starts_with('-'))
        && id6.len() == 6
        && id6.chars().all(|character| character.is_ascii_hexdigit())
}

/// Creates, probes, and tears down workspace sessions, keeping the registry in
/// step with the session backend.
pub struct WorkspaceLifecycleService {
    /// One backend per host, keyed by `remote_host` — `None` is this machine
    /// when the platform supports a local daemon. See the module docs.
    backends: Mutex<HashMap<Option<String>, Arc<dyn SessionBackend>>>,
    /// Everyone waiting to be told that some host's daemon freshness moved.
    /// See [`Self::watch_daemon_freshness`].
    ///
    /// Behind its own `Arc` because the observer handed to each backend holds
    /// it: closing over the service instead would have a backend keep the
    /// service alive, and the service owns the backends.
    freshness_watchers: Arc<Mutex<Vec<Sender<()>>>>,
    registry: AdeWorkspaceRegistry,
    /// Where every host's pushed events land, once somebody has subscribed. A
    /// host backend created later is hooked into this same fanout, so the
    /// caller keeps one stream however many hosts appear.
    events: Arc<EventFanout>,
    /// Hosts whose event pump is already running, so that subscribing to
    /// layouts after status — or the other way round — costs no second
    /// connection to any host.
    pumped: Mutex<HashSet<Option<String>>>,
    /// Invalidates pumps whose backend was dropped by [`Self::disconnect`].
    pump_generation: Arc<AtomicU64>,
    /// Hosts whose status stream could not be opened, reported in every
    /// [`Reconciled::host_errors`] from then on.
    ///
    /// Sticky on purpose, and one entry per host: a subscription is attempted
    /// once, when the host's backend is created, so a failed one means that
    /// host's dots only move when the user acts. That is a standing condition
    /// until the app is restarted, not a failed action to be cleared by the
    /// next successful one.
    status_errors: Mutex<HashMap<String, String>>,
    /// Serializes each daemon's decision read and writes across aliases.
    daemon_decision_locks: Mutex<HashMap<DaemonKey, Arc<AsyncMutex<()>>>>,
    /// Each daemon's last successful workspace listing.
    ///
    /// A host that cannot be reached keeps showing what it last held, as
    /// [`SessionState::Unknown`]: a transient ssh failure must not blink a
    /// running workspace off the sidebar. Keyed by daemon rather than host
    /// spelling, so two aliases share one snapshot. Only [`Self::disconnect`]
    /// clears it, because that is the one event that means the connections it
    /// was read over are gone.
    discoveries: Mutex<HashMap<DaemonKey, Vec<BackendWorkspace>>>,
    /// Canonical overlays whose daemon write failed transiently. The overlay
    /// stays useful locally, but the write is retried on the next listing.
    scope_update_retries: Mutex<HashSet<(DaemonKey, String, String)>>,
    history_backfilled: AsyncMutex<bool>,
}

impl WorkspaceLifecycleService {
    /// The service against this machine's default backend.
    ///
    /// The single place the backend is chosen. It named [`crate::TmuxBackend`]
    /// until 2026-08-03 and now names [`DaemonBackend`]; tmux stays compiled
    /// and tested behind [`Self::with_backend`] until the operator has accepted
    /// the daemon on a desktop build, and is deleted after that.
    pub fn new(registry: AdeWorkspaceRegistry) -> Self {
        #[cfg(unix)]
        {
            Self::with_backend(registry, Arc::new(DaemonBackend::new()))
        }
        #[cfg(not(unix))]
        {
            Self {
                backends: Mutex::new(HashMap::new()),
                freshness_watchers: Arc::new(Mutex::new(Vec::new())),
                registry,
                events: Arc::new(EventFanout::default()),
                pumped: Mutex::new(HashSet::new()),
                pump_generation: Arc::new(AtomicU64::new(0)),
                status_errors: Mutex::new(HashMap::new()),
                daemon_decision_locks: Mutex::new(HashMap::new()),
                discoveries: Mutex::new(HashMap::new()),
                scope_update_retries: Mutex::new(HashSet::new()),
                history_backfilled: AsyncMutex::new(false),
            }
        }
    }

    /// The service against a specific backend for *this machine*. Remote hosts
    /// still get theirs from [`Self::backend_for_host`].
    pub fn with_backend(registry: AdeWorkspaceRegistry, backend: Arc<dyn SessionBackend>) -> Self {
        Self {
            backends: Mutex::new(HashMap::from([(None, backend)])),
            freshness_watchers: Arc::new(Mutex::new(Vec::new())),
            registry,
            events: Arc::new(EventFanout::default()),
            pumped: Mutex::new(HashSet::new()),
            pump_generation: Arc::new(AtomicU64::new(0)),
            status_errors: Mutex::new(HashMap::new()),
            daemon_decision_locks: Mutex::new(HashMap::new()),
            discoveries: Mutex::new(HashMap::new()),
            scope_update_retries: Mutex::new(HashSet::new()),
            history_backfilled: AsyncMutex::new(false),
        }
    }

    /// Pre-registers the backend for one host, so a test can drive the remote
    /// paths without an ssh server — or name a key for one that is real.
    /// Production creates host backends lazily instead.
    #[cfg(test)]
    pub(crate) fn with_backend_for_host(
        self,
        host: impl Into<String>,
        backend: Arc<dyn SessionBackend>,
    ) -> Self {
        // The same registration [`Self::backend_for_host`] does, so a test
        // drives the wiring production uses rather than a shortcut past it.
        backend.observe_daemon_freshness(self.freshness_announcer());
        self.backends
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(Some(host.into()), backend);
        self
    }

    pub fn registry(&self) -> &AdeWorkspaceRegistry {
        &self.registry
    }

    /// Drop client-side daemon connections while leaving daemon-owned PTYs running.
    pub(crate) fn disconnect(&self) {
        self.pump_generation.fetch_add(1, Ordering::AcqRel);
        self.pumped
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        self.backends
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        // A snapshot outlives a failed listing, but not the connection it was
        // read over: nothing is claimed about a host whose backend is gone.
        self.discoveries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        self.scope_update_retries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
    }

    /// How the backend expects status to be obtained, so callers do not have to
    /// assume polling. See [`StatusDelivery`].
    ///
    /// Answered by this machine's backend: delivery is a property of the
    /// implementation, and every host runs the same one.
    pub fn status_delivery(&self) -> StatusDelivery {
        self.backends
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&None)
            .map_or(StatusDelivery::Push, |backend| backend.status_delivery())
    }

    /// Every host's pushed status events, merged onto one stream.
    ///
    /// **One subscriber.** The merge has a single sender, so a second call
    /// displaces the first subscriber's stream. [`crate::AdeWorkspaceStore`] is
    /// that subscriber for the whole process; views observe the store instead of
    /// coming here.
    ///
    /// Only for backends whose [`Self::status_delivery`] said
    /// [`StatusDelivery::Push`] — see [`SessionBackend::subscribe_status`].
    /// Take it once and keep it: a host backend created later is hooked into
    /// this same channel by [`Self::backend_for_host`], so the caller never has
    /// to re-subscribe when a remote workspace is first opened.
    ///
    /// Fails only when *this machine's* backend will not push, which is the
    /// whole stream. A remote host that will not push is recorded and comes
    /// back in the next [`Reconciled::host_errors`], because the rest of the
    /// stream is still worth having.
    pub fn subscribe_status(&self) -> Result<Receiver<StatusEvent>> {
        let (sender, receiver) = smol::channel::unbounded();
        // Stored before the pumps start, so a backend created while this runs
        // finds a sender to hook into rather than being silently left out.
        *self.events.status.lock().unwrap_or_else(|e| e.into_inner()) = Some(sender);
        self.ensure_pumps()?;
        Ok(receiver)
    }

    /// Every host's accepted layouts **and every killed workspace**, merged
    /// onto one stream — the same shape, and the same single-subscriber rule,
    /// as [`Self::subscribe_status`].
    ///
    /// **A client sees its own writes here.** The daemon excludes the
    /// connection that sent an update, and this is a different connection from
    /// the control one, so [`crate::layout`] tells its own echo apart by
    /// revision rather than by never receiving it.
    ///
    /// Removals ride the same stream because they are the same ordering: a
    /// removal that arrived out of order would leave a window pushing panes
    /// into a workspace that no longer exists.
    pub fn subscribe_layout(&self) -> Result<Receiver<WorkspaceEvent>> {
        let (sender, receiver) = smol::channel::unbounded();
        *self.events.layout.lock().unwrap_or_else(|e| e.into_inner()) = Some(sender);
        self.ensure_pumps()?;
        Ok(receiver)
    }

    /// Workspace inventory or metadata changes that may not have a live
    /// session to emit a status event.
    pub(crate) fn subscribe_workspace_changes(&self) -> Result<Receiver<()>> {
        let (sender, receiver) = smol::channel::unbounded();
        self.events
            .workspace_changes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(sender);
        self.ensure_pumps()?;
        Ok(receiver)
    }

    /// Opens the push channel of every host that has not got one open already.
    ///
    /// The single place a subscription happens, and idempotent per host — which
    /// is what keeps two subscribers from becoming two connections to the same
    /// daemon.
    fn ensure_pumps(&self) -> Result<()> {
        let backends: Vec<(Option<String>, Arc<dyn SessionBackend>)> = {
            let backends = self.backends.lock().unwrap_or_else(|e| e.into_inner());
            backends
                .iter()
                .map(|(host, backend)| (host.clone(), backend.clone()))
                .collect()
        };
        for (host, backend) in backends {
            if let Err(error) = self.ensure_pump(host.as_deref(), &backend) {
                match host {
                    None => return Err(error),
                    Some(host) => self.record_status_error(&host, &error),
                }
            }
        }
        Ok(())
    }

    /// One host's pump, started once. Does nothing before anybody has
    /// subscribed — [`Self::ensure_pumps`] picks up whatever already exists.
    fn ensure_pump(&self, host: Option<&str>, backend: &Arc<dyn SessionBackend>) -> Result<()> {
        if self.events.is_idle() {
            return Ok(());
        }
        let key = host.map(str::to_owned);
        {
            let mut pumped = self.pumped.lock().unwrap_or_else(|e| e.into_inner());
            if !pumped.insert(key.clone()) {
                return Ok(());
            }
        }
        let generation = self.pump_generation.load(Ordering::Acquire);
        if let Err(error) = pump_events(
            backend,
            key.clone(),
            self.events.clone(),
            self.pump_generation.clone(),
            generation,
        ) {
            self.pumped
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
            return Err(error);
        }
        Ok(())
    }

    /// Registers a new workspace and gives it a detached session.
    ///
    /// The registry row lands first, as `creating`, so a crash between the two
    /// steps leaves a visible half-made workspace rather than an orphan
    /// session. If the backend refuses, the workspace is recorded as `error`
    /// and the failure is returned — the row stays, because the user asked for
    /// it and needs to see why it did not come up.
    ///
    /// `remote_host` is `None` for this machine; otherwise `repository_path` is
    /// read as a path **on that host**, which is why nothing here touches the
    /// local filesystem to check it.
    pub async fn create_workspace_from_repository(
        &self,
        name: impl Into<String>,
        repository_path: impl Into<PathBuf>,
        branch: Option<String>,
        remote_host: Option<String>,
    ) -> Result<AdeWorkspace> {
        let requested_path = repository_path.into();
        let (repository_path, project_identity_path) = self
            .backend_for_host(remote_host.as_deref())?
            .resolve_repository(&requested_path)
            .with_context(|| {
                format!(
                    "resolving project directory {} on {}",
                    requested_path.display(),
                    host_label(remote_host.as_deref())
                )
            })?;
        let project_identity = util::path_list::PathList::new(&[project_identity_path])
            .serialize()
            .paths;
        let project_id = project_id_from_identity(&project_identity);
        self.create_workspace_scoped(
            name,
            project_id,
            Some(project_identity),
            repository_path,
            branch,
            remote_host,
        )
        .await
    }

    /// Test helper for synthetic repository paths that do not exist on a host.
    #[cfg(test)]
    pub async fn create_workspace(
        &self,
        name: impl Into<String>,
        project_id: impl Into<String>,
        repository_path: impl Into<PathBuf>,
        branch: Option<String>,
        remote_host: Option<String>,
    ) -> Result<AdeWorkspace> {
        let repository_path = repository_path.into();
        let project_identity = repository_path.to_string_lossy().into_owned();
        self.create_workspace_scoped(
            name,
            project_id,
            Some(project_identity),
            repository_path,
            branch,
            remote_host,
        )
        .await
    }

    pub async fn create_workspace_scoped(
        &self,
        name: impl Into<String>,
        project_id: impl Into<String>,
        project_identity: Option<String>,
        repository_path: impl Into<PathBuf>,
        branch: Option<String>,
        remote_host: Option<String>,
    ) -> Result<AdeWorkspace> {
        let mut workspace = AdeWorkspace::new(name, project_id, repository_path);
        workspace.project_identity = project_identity;
        workspace.branch = branch;
        workspace.remote_host = remote_host;

        self.registry
            .create_workspace(workspace.clone())
            .await
            .context("recording the new workspace")?;

        self.start_session(&mut workspace).await?;
        Ok(workspace)
    }

    pub async fn update_workspace_project_scope(
        &self,
        id: &WorkspaceId,
        project_id: &str,
        project_identity: &str,
    ) -> Result<AdeWorkspace> {
        let workspace = self.get(id)?;
        if workspace.project_id == project_id
            && workspace.project_identity.as_deref() == Some(project_identity)
        {
            return Ok(workspace);
        }

        let project_scope_rev = next_project_scope_rev(workspace.project_scope_rev)?;
        let backend = match self.backend_for(&workspace) {
            Ok(backend) => Some(backend),
            Err(error) => {
                log::warn!(
                    "could not prepare the backend before updating project scope for {id}; keeping the authoritative local scope: {error:#}"
                );
                None
            }
        };
        let daemon_workspace_id = workspace.daemon_workspace_id();
        let daemon_holds_workspace = backend.as_ref().is_some_and(|backend| {
            match backend.list_workspaces_identified() {
                Ok(listing) => listing
                    .items
                    .iter()
                    .any(|record| record.id == daemon_workspace_id),
                Err(error) => {
                    log::warn!(
                        "could not list daemon workspaces before updating project scope for {id}; keeping the authoritative local scope: {error:#}"
                    );
                    false
                }
            }
        });
        let mut project_scope_rev = project_scope_rev;
        if daemon_holds_workspace && let Some(backend) = backend.as_ref() {
            let updated = backend.update_workspace_project_scope(
                &daemon_workspace_id,
                project_id,
                project_identity,
                None,
                Some(project_scope_rev),
                workspace.daemon_id.as_deref(),
            );
            match updated {
                Ok(Some(daemon_revision)) => project_scope_rev = daemon_revision,
                Ok(None) => log::debug!(
                    "daemon does not persist project identity yet; keeping it in the local registry"
                ),
                Err(error) => log::warn!(
                    "could not persist project identity for workspace {id}; keeping the authoritative local scope: {error:#}"
                ),
            }
        }

        loop {
            let (applied, stored) = self
                .registry
                .update_project_scope(
                    id.clone(),
                    project_scope_rev,
                    project_id.to_owned(),
                    project_identity.to_owned(),
                )
                .await
                .context("recording the workspace project identity")?;
            if applied
                || (stored.project_id == project_id
                    && stored.project_identity.as_deref() == Some(project_identity))
            {
                return Ok(stored);
            }
            project_scope_rev = next_project_scope_rev(stored.project_scope_rev)?;
        }
    }

    pub async fn update_workspace_repository_scope(
        &self,
        id: &WorkspaceId,
        repository_path: PathBuf,
        project_id: &str,
        project_identity: &str,
    ) -> Result<AdeWorkspace> {
        let selected = self.get(id)?;
        let old_repository_path = selected.repository_path.clone();
        let remote_host = selected.remote_host.clone();
        let mut matching = self
            .registry
            .list_workspaces()?
            .into_iter()
            .filter(|workspace| {
                workspace.remote_host == remote_host
                    && workspace.repository_path == old_repository_path
            })
            .collect::<Vec<_>>();
        let backend = match self.backend_for(&selected) {
            Ok(backend) => Some(backend),
            Err(error) => {
                log::warn!(
                    "could not prepare the backend before moving repository scope for {id}; updating the authoritative local rows: {error:#}"
                );
                None
            }
        };
        let held = match backend
            .as_ref()
            .map(|backend| backend.list_workspaces_identified())
        {
            Some(Ok(listing)) => listing
                .items
                .into_iter()
                .map(|workspace| workspace.id)
                .collect::<HashSet<_>>(),
            Some(Err(error)) => {
                log::warn!(
                    "could not list daemon workspaces before moving repository scope for {id}; updating the authoritative local rows: {error:#}"
                );
                HashSet::new()
            }
            None => HashSet::new(),
        };
        let project_root = repository_path.to_string_lossy().into_owned();
        for workspace in &mut matching {
            if workspace.repository_path == repository_path
                && workspace.project_id == project_id
                && workspace.project_identity.as_deref() == Some(project_identity)
            {
                continue;
            }
            let mut project_scope_rev = next_project_scope_rev(workspace.project_scope_rev)?;
            let daemon_workspace_id = workspace.daemon_workspace_id();
            if held.contains(&daemon_workspace_id)
                && let Some(backend) = backend.as_ref()
            {
                match backend.update_workspace_project_scope(
                    &daemon_workspace_id,
                    project_id,
                    project_identity,
                    Some(&project_root),
                    Some(project_scope_rev),
                    workspace.daemon_id.as_deref(),
                ) {
                    Ok(Some(daemon_rev)) => project_scope_rev = daemon_rev,
                    Ok(None) => log::debug!(
                        "daemon does not persist repository identity yet; keeping it in the local registry"
                    ),
                    Err(error) => log::warn!(
                        "could not persist repository identity for workspace {}; keeping the authoritative local scope: {error:#}",
                        workspace.id
                    ),
                }
            }
            loop {
                let (applied, stored) = self
                    .registry
                    .update_repository_scope(
                        workspace.id.clone(),
                        repository_path.clone(),
                        project_scope_rev,
                        project_id.to_owned(),
                        project_identity.to_owned(),
                    )
                    .await
                    .with_context(|| {
                        format!(
                            "recording repository identity for workspace {}",
                            workspace.id
                        )
                    })?;
                if applied
                    || (stored.repository_path == repository_path
                        && stored.project_id == project_id
                        && stored.project_identity.as_deref() == Some(project_identity))
                {
                    *workspace = stored;
                    break;
                }
                project_scope_rev = next_project_scope_rev(stored.project_scope_rev)?;
            }
        }
        matching
            .into_iter()
            .find(|workspace| &workspace.id == id)
            .context("the selected workspace disappeared while moving its repository scope")
    }

    pub async fn update_discovered_workspace_project_scope(
        &self,
        remote_host: Option<&str>,
        workspace_id: &str,
        project_id: &str,
        project_identity: &str,
    ) -> Result<Option<bool>> {
        let backend = self.backend_for_host(remote_host)?;
        let listing = backend.list_workspaces_identified()?;
        let Some(workspace) = listing
            .items
            .iter()
            .find(|workspace| workspace.id == workspace_id)
        else {
            return Ok(None);
        };
        if workspace.project_id.as_deref() == Some(project_id)
            && workspace.project_identity.as_deref() == Some(project_identity)
        {
            return Ok(Some(true));
        }
        let local_scope_rev = next_project_scope_rev(workspace.project_scope_rev)?;
        let updated = backend
            .update_workspace_project_scope(
                workspace_id,
                project_id,
                project_identity,
                Some(&workspace.project_root),
                Some(local_scope_rev),
                listing.daemon_id.as_deref(),
            )
            .context("updating a discovered workspace's project identity")?;
        if updated.is_none() {
            let confirmed = self.confirm_discovered(remote_host, workspace_id).await?;
            self.update_workspace_project_scope(&confirmed.id, project_id, project_identity)
                .await
                .context("recording a discovered workspace's project identity locally")?;
        }
        Ok(Some(updated.is_some()))
    }

    /// Marks the workspace as opened now and reports the state of its session.
    ///
    /// This is what a "reconnect" is: nothing is created, the still-running
    /// session is simply found again. A [`SessionState::Dead`] result is the
    /// caller's to surface.
    pub async fn open_workspace(&self, id: &WorkspaceId) -> Result<(AdeWorkspace, SessionState)> {
        let mut workspace = self.get(id)?;
        let opened_at = now_whole_seconds();
        self.registry
            .update_last_opened_at(id.clone(), opened_at)
            .await?;
        workspace.last_opened_at = opened_at;

        let state = self.probe(&mut workspace).await?;
        Ok((workspace, state))
    }

    /// Asks the backend whether the workspace's session is alive and writes the
    /// answer back to the registry: alive is `running`, dead is `disconnected`.
    ///
    /// A dead session's `terminal_session_id` is deliberately **kept** so the
    /// UI can name what died, and nothing is recreated — see
    /// [`Self::recreate_session`]. `workspace` is updated in place so the
    /// caller's copy does not go stale.
    pub async fn probe(&self, workspace: &mut AdeWorkspace) -> Result<SessionState> {
        // Before the backend is asked for, so a workspace with nothing to probe
        // never reaches for a host — a registry row for a machine that is off
        // must not cost an ssh attempt just to say "never created".
        let Some(session) = workspace.terminal_session_id.clone().map(SessionId::from) else {
            return Ok(SessionState::NeverCreated);
        };
        let alive = self
            .backend_for(workspace)?
            .exists(&session, workspace.daemon_id.as_deref())
            .with_context(|| format!("probing session {session}"))?;
        self.record_probe(workspace, alive).await
    }

    /// Gives a workspace whose session died a new one, under the same derived
    /// name.
    ///
    /// The explicit counterpart to [`Self::probe`] never repairing anything:
    /// the user saw "disconnected" and asked for it back. Idempotent — a
    /// session that turns out to be alive is adopted rather than duplicated.
    pub async fn recreate_session(&self, id: &WorkspaceId) -> Result<AdeWorkspace> {
        let mut workspace = self.get(id)?;

        let session = SessionId::from(workspace.daemon_workspace_id());
        if self
            .backend_for(&workspace)?
            .exists(&session, workspace.daemon_id.as_deref())?
        {
            self.adopt_session(&mut workspace, session).await?;
            return Ok(workspace);
        }
        self.start_session(&mut workspace).await?;
        Ok(workspace)
    }

    /// Kills every persistent session for one worktree and starts a fresh
    /// primary session in the workspace this window was showing.
    ///
    /// Unlike [`Self::kill_workspace`], this keeps the selected daemon workspace
    /// record and its identity. Older duplicate records for the exact same
    /// project, host and repository root are killed and deleted, because one of
    /// those detached workspaces is precisely where an orphaned agent can keep
    /// its writer lock. Registry aliases that name the same daemon workspace
    /// are also deduplicated. Other projects, worktrees and the host daemon keep
    /// running.
    pub async fn reset_workspace_sessions(
        &self,
        id: &WorkspaceId,
    ) -> Result<(AdeWorkspace, Attached)> {
        let mut workspace = self.get(id)?;
        let daemon_workspace_id = workspace.daemon_workspace_id();
        let route_lock = self.daemon_decision_lock(DaemonKey::Host(workspace.remote_host.clone()));
        let _route_decision = route_lock.lock().await;
        let backend = self.backend_for(&workspace)?;
        let daemon_id = match workspace.daemon_id.clone() {
            Some(daemon_id) => Some(daemon_id),
            None => backend.list_workspaces_identified()?.daemon_id,
        };
        let instance_lock = daemon_id
            .as_deref()
            .map(|daemon_id| self.daemon_decision_lock(daemon_key(Some(daemon_id), None)));
        let _instance_decision = match instance_lock.as_ref() {
            Some(lock) => Some(lock.lock().await),
            None => None,
        };
        if workspace.daemon_id.is_none()
            && backend.list_workspaces_identified()?.daemon_id != daemon_id
        {
            bail!(
                "the daemon changed while preparing to reset workspace {}",
                workspace.id
            );
        }
        let duplicates = self
            .registry
            .list_workspaces()?
            .into_iter()
            .filter(|candidate| {
                candidate.id != workspace.id
                    && candidate.remote_host == workspace.remote_host
                    && candidate.daemon_id == workspace.daemon_id
                    && (candidate.daemon_workspace_id() == daemon_workspace_id
                        || (candidate.project_id == workspace.project_id
                            && candidate.repository_path == workspace.repository_path))
            })
            .collect::<Vec<_>>();

        for duplicate in duplicates {
            let duplicate_daemon_id = duplicate.daemon_workspace_id();
            if duplicate_daemon_id != daemon_workspace_id {
                backend
                    .kill_workspace(&duplicate_daemon_id, daemon_id.as_deref())
                    .with_context(|| format!("removing stale workspace {duplicate_daemon_id}"))?;
                self.registry
                    .delete_workspace(duplicate.id.clone())
                    .await
                    .with_context(|| format!("deleting duplicate row {}", duplicate.id))?;
            } else {
                // Two registry rows can point at one daemon record after an
                // interrupted alias rebind. The selected row owns that record;
                // deleting the other row is enough and must not kill it.
                self.registry
                    .delete_workspace(duplicate.id.clone())
                    .await
                    .with_context(|| format!("deleting duplicate row {}", duplicate.id))?;
            }
        }

        // ponytail: this is the backend's existing list-and-kill operation;
        // add an atomic ResetWorkspace protocol frame if concurrent clients
        // creating sessions during recovery becomes a real problem.
        backend
            .reset_workspace_sessions(
                &SessionId::from(daemon_workspace_id.clone()),
                &workspace.repository_path,
                daemon_id.as_deref(),
            )
            .with_context(|| format!("killing sessions in workspace {daemon_workspace_id}"))?;
        workspace.daemon_id = workspace.daemon_id.or(daemon_id);
        self.start_session(&mut workspace).await?;
        let attached = self.attach_command(&workspace)?;
        Ok((workspace, attached))
    }

    /// Removes each confirmed workspace that is still dead after an immediate
    /// second probe, returning how many registry rows were deleted.
    ///
    /// The candidate ids freeze the scope the user confirmed. Reconciliation is
    /// performed here so the caller cannot decide which of those targets are
    /// still safe to destroy, while a different workspace that becomes dead
    /// after confirmation cannot drift into scope. A candidate that became
    /// alive, lost its session identity, or became unreachable between the two
    /// probes is left alone. Each dead daemon workspace record is killed before
    /// its registry row is deleted, preventing the next reconciliation from
    /// discovering it again.
    pub async fn cleanup_dead_workspaces(&self, candidate_ids: Vec<WorkspaceId>) -> Result<usize> {
        let candidate_ids = candidate_ids.into_iter().collect::<HashSet<_>>();
        let reconciled = self.reconcile_all().await?;
        for (host, error) in reconciled.host_errors {
            log::warn!("skipping dead workspace cleanup on {host}: {error}");
        }

        let mut cleaned = 0;
        // Rows only: a discovery is nothing this client has used, so it is
        // nothing this client may destroy.
        for (workspace, state) in reconciled
            .entries
            .iter()
            .filter_map(WorkspaceEntry::persisted)
        {
            if state != SessionState::Dead || !candidate_ids.contains(&workspace.id) {
                continue;
            }

            let Some(mut current) = self.registry.get_workspace(workspace.id.clone())? else {
                continue;
            };
            let state = match self.probe(&mut current).await {
                Ok(state) => state,
                Err(error) => {
                    log::warn!(
                        "skipping dead workspace cleanup for {} after its re-probe failed: {error:#}",
                        current.id
                    );
                    continue;
                }
            };
            if state != SessionState::Dead {
                continue;
            }

            self.kill_and_delete_workspace(&current)
                .await
                .with_context(|| format!("cleaning dead workspace {}", current.id))?;
            cleaned += 1;
        }
        Ok(cleaned)
    }

    /// Records that the workspace's session now exists under its derived name,
    /// without asking the backend anything.
    ///
    /// For the caller that has just run the attach argv and had it succeed: the
    /// pane is the creator, and this is the registry catching up with what the
    /// pane already did. It is deliberately *not* a probe — probing here would
    /// race the pane's own attach-or-create, and [`Self::recreate_session`]
    /// cannot be used for the same reason ([`SessionBackend::create`] fails on
    /// a session that already exists).
    ///
    /// Only sound after an attach the caller watched succeed. Anything that
    /// merely *hopes* a session exists wants [`Self::probe`].
    pub async fn record_attached_session(
        &self,
        id: &WorkspaceId,
        daemon_id: Option<String>,
    ) -> Result<AdeWorkspace> {
        let mut workspace = self.get(id)?;

        // No backend is asked for: this is the registry writing down what the
        // pane already did, and it is as true of a remote host as a local one.
        let session = SessionId::from(workspace.daemon_workspace_id());
        let daemon_id = workspace.daemon_id.clone().or(daemon_id);
        self.registry
            .update_terminal_session_and_daemon_id(
                workspace.id.clone(),
                Some(session.to_string()),
                daemon_id.clone(),
            )
            .await?;
        workspace.terminal_session_id = Some(session.to_string());
        workspace.daemon_id = daemon_id;
        self.set_status(&mut workspace, WorkspaceStatus::Running)
            .await?;
        Ok(workspace)
    }

    /// Renames a workspace: the daemon first, the registry after.
    ///
    /// **That order is the whole method.** The backend owns what a workspace is
    /// called, so a rename the daemon refused — an unknown id, a daemon too old
    /// to know the frame, a host that could not be reached — must leave the
    /// registry alone and come back as an error the user is shown. Writing the
    /// row first would give this one machine a name no other client, and no
    /// restart, would ever agree with.
    ///
    /// **The id never moves**, so nothing has to be re-linked: the session, the
    /// stored layout and this row are all keyed by it, and
    /// [`AdeWorkspace::daemon_workspace_id`] is pinned to the recorded session
    /// name for exactly this reason. An empty name is refused here as well as by
    /// the daemon, since there is no point in a round trip to be told so.
    pub async fn rename_workspace(&self, id: &WorkspaceId, name: &str) -> Result<AdeWorkspace> {
        let mut workspace = self.get(id)?;
        let name = name.trim();
        if name.is_empty() {
            bail!("a workspace needs a name");
        }
        if workspace.name == name {
            return Ok(workspace);
        }

        self.backend_for(&workspace)?
            .rename_workspace(
                &workspace.daemon_workspace_id(),
                name,
                workspace.daemon_id.as_deref(),
            )
            .with_context(|| format!("renaming workspace {id}"))?;

        self.registry
            .update_name(id.clone(), name.to_owned())
            .await
            .context("recording the new name")?;
        workspace.name = name.to_owned();
        Ok(workspace)
    }

    /// Closes a workspace: every attached client is detached and the workspace
    /// is recorded as `stopped`.
    ///
    /// **This does not kill anything.** The session and every process in it
    /// keep running, which is the whole point — the agent that was working
    /// carries on, and the workspace can be reattached later with its state
    /// intact.
    pub async fn stop_workspace(&self, id: &WorkspaceId) -> Result<AdeWorkspace> {
        let mut workspace = self.get(id)?;

        if let Some(session) = workspace.terminal_session_id.clone().map(SessionId::from) {
            let backend = self.backend_for(&workspace)?;
            if backend.exists(&session, workspace.daemon_id.as_deref())? {
                backend
                    .detach(&session)
                    .with_context(|| format!("detaching clients from session {session}"))?;
            }
        }
        self.set_status(&mut workspace, WorkspaceStatus::Stopped)
            .await?;
        Ok(workspace)
    }

    /// Kills the whole workspace: every session in it **and the backend's
    /// record of it**, layout included, then forgets the session name and
    /// records the workspace as `stopped`.
    ///
    /// **The workspace-level kill** (operator ruling, 2026-08-04) and what the
    /// sidebar's kill control sends. Destructive and irreversible: running
    /// agents die with their sessions, the stored arrangement goes with the
    /// record, and every other client watching that workspace is told so it can
    /// stop syncing. The registry row is *not* removed — forgetting a workspace
    /// is [`AdeWorkspaceRegistry::delete_workspace`], and a killed one reads as
    /// never-created, ready to be recreated under the same name.
    ///
    pub async fn kill_workspace(&self, id: &WorkspaceId) -> Result<AdeWorkspace> {
        let mut workspace = self.get(id)?;
        let route_lock = self.daemon_decision_lock(DaemonKey::Host(workspace.remote_host.clone()));
        let _route_decision = route_lock.lock().await;
        let backend = self.backend_for(&workspace)?;
        let daemon_id = match workspace.daemon_id.clone() {
            Some(daemon_id) => Some(daemon_id),
            None => backend.list_workspaces_identified()?.daemon_id,
        };
        let instance_lock = daemon_id
            .as_deref()
            .map(|daemon_id| self.daemon_decision_lock(daemon_key(Some(daemon_id), None)));
        let _instance_decision = match instance_lock.as_ref() {
            Some(lock) => Some(lock.lock().await),
            None => None,
        };
        if workspace.daemon_id.is_none()
            && backend.list_workspaces_identified()?.daemon_id != daemon_id
        {
            bail!(
                "the daemon changed while preparing to kill workspace {}",
                workspace.id
            );
        }
        backend
            .kill_workspace(&workspace.daemon_workspace_id(), daemon_id.as_deref())
            .with_context(|| format!("killing daemon workspace {}", workspace.id))?;
        self.record_workspace_killed(&mut workspace).await?;
        Ok(workspace)
    }

    /// Kills a daemon workspace record before deleting its registry row.
    ///
    /// The row is only deleted after the record kill succeeds, so an unsupported
    /// operation or connectivity failure remains visible and retryable instead
    /// of leaving a daemon record that reconciliation can re-discover.
    async fn kill_and_delete_workspace(&self, workspace: &AdeWorkspace) -> Result<()> {
        let route_lock = self.daemon_decision_lock(DaemonKey::Host(workspace.remote_host.clone()));
        let _route_decision = route_lock.lock().await;
        let backend = self.backend_for(workspace)?;
        let daemon_id = match workspace.daemon_id.clone() {
            Some(daemon_id) => Some(daemon_id),
            None => backend.list_workspaces_identified()?.daemon_id,
        };
        let instance_lock = daemon_id
            .as_deref()
            .map(|daemon_id| self.daemon_decision_lock(daemon_key(Some(daemon_id), None)));
        let _instance_decision = match instance_lock.as_ref() {
            Some(lock) => Some(lock.lock().await),
            None => None,
        };
        if workspace.daemon_id.is_none()
            && backend.list_workspaces_identified()?.daemon_id != daemon_id
        {
            bail!(
                "the daemon changed while preparing to delete workspace {}",
                workspace.id
            );
        }
        let daemon_workspace_id = workspace.daemon_workspace_id();
        backend
            .kill_workspace(&daemon_workspace_id, daemon_id.as_deref())
            .with_context(|| format!("killing daemon workspace record {daemon_workspace_id}"))?;
        self.registry
            .delete_workspace(workspace.id.clone())
            .await
            .with_context(|| format!("deleting workspace {} from the registry", workspace.id))
    }

    /// Kills the workspace's sessions and everything running in them, while
    /// keeping the daemon workspace identity and recording it as `stopped`.
    ///
    /// **Destructive and irreversible.** Running agents die with the session
    /// and their scrollback goes with them. The workspace-record kill is
    /// [`Self::kill_workspace`]. `terminal_session_id` is also the stable daemon
    /// workspace id, so it stays recorded even while no session is alive; that
    /// lets a later recreate target the same retained record and layout.
    pub async fn kill_workspace_session(&self, id: &WorkspaceId) -> Result<AdeWorkspace> {
        let mut workspace = self.get(id)?;

        if let Some(session) = workspace.terminal_session_id.clone().map(SessionId::from) {
            let backend = self.backend_for(&workspace)?;
            // A session that is already gone needs no killing: the outcome the
            // caller asked for is the state the world is in, and the rest of
            // the method — recording `stopped` — is still owed. Same
            // probe-first shape as `stop_workspace`.
            //
            // `SessionBackend::kill` tolerates a missing session too, which
            // covers the gap between this check and the kill.
            if backend
                .exists(&session, workspace.daemon_id.as_deref())
                .with_context(|| format!("checking session {session} before killing it"))?
            {
                backend
                    .kill(&session, workspace.daemon_id.as_deref())
                    .with_context(|| format!("killing session {session}"))?;
            }
        }
        self.set_status(&mut workspace, WorkspaceStatus::Stopped)
            .await?;
        Ok(workspace)
    }

    /// Writes down what deleting a daemon workspace left behind: no daemon
    /// identity or session owner, and `stopped`.
    async fn record_workspace_killed(&self, workspace: &mut AdeWorkspace) -> Result<()> {
        if workspace.terminal_session_id.is_some() || workspace.daemon_id.is_some() {
            self.registry
                .update_terminal_session_and_daemon_id(workspace.id.clone(), None, None)
                .await?;
            workspace.terminal_session_id = None;
            workspace.daemon_id = None;
        }
        self.set_status(workspace, WorkspaceStatus::Stopped).await
    }

    /// The startup pass: probes every registered workspace, writes the results
    /// back, and asks each host it contacts what else it holds — so the sidebar
    /// opens showing what is actually running rather than what was running when
    /// the app last closed.
    pub async fn reconcile_all(&self) -> Result<Reconciled> {
        self.ensure_history_backfill().await?;
        let workspaces = self.registry.list_workspaces()?;
        self.reconcile(workspaces, true).await
    }

    /// [`Self::reconcile_all`], narrowed to one project's rows.
    ///
    /// **Rows only.** A discovery belongs to no project until something derives
    /// one from its root, so a narrowed pass that reported them would be
    /// putting other projects' workspaces on screen.
    pub async fn reconcile_project(&self, project_id: impl Into<String>) -> Result<Reconciled> {
        self.ensure_history_backfill().await?;
        let workspaces = self
            .registry
            .list_workspaces_for_project(project_id.into())?;
        self.reconcile(workspaces, false).await
    }

    /// Brings one host's session backend up and answers with everything the
    /// connect decision may choose from: the rows this client already has, and
    /// the records the host's daemon holds that it has never used.
    ///
    /// **The connect decision's one call.** A window that has just connected to
    /// a host has to decide between reattaching to a workspace and making the
    /// first one, and until 2026-08-05 it decided that from
    /// `registry().list_workspaces()` alone — so a client with an empty
    /// registry created a second workspace on a host whose daemon already held
    /// one. The registry is a cache; asking it what a host has is asking the
    /// wrong party.
    ///
    /// **No registry row is created here.** A record this client has never
    /// opened comes back as [`WorkspaceEntry::Discovered`] and stays that way
    /// until [`Self::confirm_discovered`]; a listing is not usage. Canonical
    /// Git scope may be copied onto the daemon record when supported.
    ///
    /// Ensuring is the side effect of the listing rather than a step of its
    /// own: the first request over a host's backend deploys and starts the
    /// daemon if the host has none, and rides that host's one connection like
    /// everything else. **It must therefore still fail loudly** — the caller
    /// falls back to a plain terminal on the error, and a silent success would
    /// leave it opening a workspace against a daemon that is not there.
    ///
    /// One host, not every host: [`Self::reconcile_all`] is the pass that
    /// contacts them all, and a connect must not pay for hosts it is not
    /// connecting to. Only rows owned by the daemon that answered are returned;
    /// a stale row on the same route must not beat that daemon's discovery.
    /// Nothing is probed: the caller opens what it picks, and opening probes.
    pub async fn ensure_host_workspaces(&self, host: Option<&str>) -> Result<Vec<WorkspaceEntry>> {
        let backend = self.backend_for_host(host)?;
        let (daemon, held, rows) = self.list_host_workspaces(&backend, host).await?;
        let used: HashSet<String> = rows
            .iter()
            .filter(|row| row_is_on(row, &daemon, host))
            .map(AdeWorkspace::daemon_workspace_id)
            .collect();
        Ok(rows
            .iter()
            .filter(|row| row_is_on(row, &daemon, host))
            .map(|row| WorkspaceEntry::Persisted(row.clone(), SessionState::Unknown))
            .chain(
                held.into_iter()
                    .filter(|record| !used.contains(&record.id))
                    .map(|record| WorkspaceEntry::Discovered {
                        remote_host: host.map(str::to_owned),
                        workspace: record,
                        state: SessionState::Unknown,
                    }),
            )
            .collect())
    }

    /// Resolves a fresh connection under the contacted daemon's decision lock:
    /// owned row, matching discovery, or creation. A failed listing may reuse
    /// a compatible offline row, but never authorizes creation.
    pub async fn adopt_or_create_workspace(
        &self,
        root: PathBuf,
        host: Option<String>,
        project_scope: Option<(String, String)>,
    ) -> Result<(AdeWorkspace, bool)> {
        let backend = self.backend_for_host(host.as_deref())?;
        let StableListing {
            _route_decision,
            _instance_decision,
            daemon,
            held,
        } = self
            .listing_under_daemon_lock(&backend, host.as_deref())
            .await?;

        let rows = self.registry.list_workspaces()?;
        let matching = || rows.iter().filter(|row| row.repository_path == root);

        let held = match held {
            Ok(held) => held,
            Err(error) => {
                return most_recently_opened(
                    matching().filter(|row| row_could_be_on(row, &daemon, host.as_deref())),
                )
                .cloned()
                .map(|workspace| (workspace, false))
                .ok_or(error);
            }
        };

        if let Some(existing) = most_recently_opened(matching().filter(|row| {
            row_is_on(row, &daemon, host.as_deref())
                && held
                    .iter()
                    .any(|record| record.id == row.daemon_workspace_id())
        })) {
            return Ok((existing.clone(), false));
        }

        if let Some(existing) = most_recently_opened(matching().filter(|row| {
            row_is_on(row, &daemon, host.as_deref()) && row.terminal_session_id.is_none()
        })) {
            return Ok((existing.clone(), false));
        }

        let used: HashSet<String> = rows
            .iter()
            .filter(|row| row_is_on(row, &daemon, host.as_deref()))
            .map(AdeWorkspace::daemon_workspace_id)
            .collect();
        let discovered = held
            .iter()
            .filter(|record| !used.contains(&record.id))
            .filter(|record| Path::new(&record.project_root) == root)
            .min_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        if let Some(record) = discovered {
            let confirmed = self
                .confirm_record(&daemon, host.as_deref(), record)
                .await?;
            let confirmed = match project_scope.as_ref() {
                Some((project_id, project_identity)) => {
                    self.update_workspace_project_scope(&confirmed.id, project_id, project_identity)
                        .await?
                }
                None => confirmed,
            };
            return Ok((confirmed, false));
        }

        let (project_id, project_identity) = project_scope
            .map(|(project_id, project_identity)| (project_id, Some(project_identity)))
            .unwrap_or_else(|| {
                (
                    project_id_from_path(&root),
                    Some(root.to_string_lossy().into_owned()),
                )
            });
        let workspace = self
            .create_workspace_scoped(
                project_id.clone(),
                project_id,
                project_identity,
                root,
                None,
                host,
            )
            .await?;
        Ok((workspace, true))
    }

    /// **Opening a discovered workspace**: confirms the record the user clicked
    /// and answers with the row that now addresses it.
    ///
    /// The listing is taken again under that daemon's decision lock, so a
    /// record removed since the sidebar drew it comes back as [`WorkspaceGone`]
    /// and leaves the snapshot, rather than becoming a row for a workspace that
    /// no longer exists.
    ///
    /// Three outcomes, in this order: the row this client already uses for the
    /// record, because clicking twice is not two workspaces; a quarantined row
    /// for it promoted in place, because its uuid, branch and history are
    /// metadata a listing cannot re-derive; or one new row.
    pub async fn confirm_discovered(
        &self,
        host: Option<&str>,
        wire_id: &str,
    ) -> Result<AdeWorkspace> {
        self.ensure_history_backfill().await?;
        let backend = self.backend_for_host(host)?;
        for _ in 0..3 {
            let route_lock = self.daemon_decision_lock(DaemonKey::Host(host.map(str::to_owned)));
            let _route_decision = route_lock.lock().await;
            let expected_daemon_id = match backend.instance_id() {
                Some(daemon_id) => Some(daemon_id),
                None => {
                    backend
                        .list_workspaces_identified()
                        .with_context(|| format!("listing the workspaces on {}", host_label(host)))?
                        .daemon_id
                }
            };
            let instance_lock = expected_daemon_id
                .as_deref()
                .map(|daemon_id| self.daemon_decision_lock(daemon_key(Some(daemon_id), host)));
            let _instance_decision = match instance_lock.as_ref() {
                Some(lock) => Some(lock.lock().await),
                None => None,
            };
            let listing = backend
                .list_workspaces_identified()
                .with_context(|| format!("refreshing the workspaces on {}", host_label(host)))?;
            if listing.daemon_id != expected_daemon_id {
                continue;
            }

            let daemon_id = listing.daemon_id;
            let daemon = daemon_key(daemon_id.as_deref(), host);
            let mut held = listing.items;
            let known = self.registry.list_workspaces()?;
            self.enrich_workspace_project_scopes(
                &backend,
                &daemon,
                daemon_id.as_deref(),
                host,
                &known,
                &mut held,
            )?;
            self.remember_discoveries(&daemon, &held);
            let Some(record) = held.iter().find(|record| record.id == wire_id) else {
                self.forget_discovery(&daemon, wire_id);
                return Err(WorkspaceGone {
                    remote_host: host.map(str::to_owned),
                    wire_id: wire_id.to_owned(),
                }
                .into());
            };

            return self.confirm_record(&daemon, host, record).await;
        }
        bail!(
            "the daemon identity on {} kept changing while confirming workspace {wire_id}",
            host_label(host)
        )
    }

    async fn confirm_record(
        &self,
        daemon: &DaemonKey,
        host: Option<&str>,
        record: &BackendWorkspace,
    ) -> Result<AdeWorkspace> {
        let wire_id = record.id.as_str();
        let owner = |rows: Vec<AdeWorkspace>| {
            rows.into_iter().find(|row| {
                row_is_on(row, daemon, host) && row.terminal_session_id.as_deref() == Some(wire_id)
            })
        };
        if let Some(row) = owner(self.registry.list_workspaces()?) {
            return Ok(row);
        }

        let now = now_whole_seconds();
        let remote_host = host.map(str::to_owned);
        let daemon_id = instance_of(daemon).map(str::to_owned);
        for mut row in self
            .registry
            .promotion_candidates(instance_of(daemon))?
            .into_iter()
            .filter(|row| is_the_same_daemons_row(row, instance_of(daemon), host, record))
        {
            match self
                .registry
                .confirm_workspace(row.id.clone(), remote_host.clone(), daemon_id.clone(), now)
                .await
            {
                Ok(true) => {
                    row.remote_host = remote_host;
                    row.daemon_id = daemon_id;
                    row.last_opened_at = now;
                    return Ok(row);
                }
                Ok(false) => {
                    if let Some(owner) = owner(self.registry.list_workspaces()?) {
                        return Ok(owner);
                    }
                }
                Err(error) => {
                    if let Some(owner) = owner(self.registry.list_workspaces()?) {
                        return Ok(owner);
                    }
                    return Err(error).with_context(|| format!("confirming workspace {}", row.id));
                }
            }
        }

        self.registry
            .create_workspace(row_for_record(record, host, instance_of(daemon), now))
            .await
            .with_context(|| format!("recording workspace {wire_id}"))?;
        owner(self.registry.list_workspaces()?)
            .with_context(|| format!("re-reading the row recorded for workspace {wire_id}"))
    }

    fn remember_discoveries(&self, daemon: &DaemonKey, held: &[BackendWorkspace]) {
        self.discoveries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(daemon.clone(), held.to_vec());
    }

    fn remembered_discoveries(&self, daemon: &DaemonKey) -> Vec<BackendWorkspace> {
        self.discoveries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(daemon)
            .cloned()
            .unwrap_or_default()
    }

    fn scope_update_key(
        daemon: &DaemonKey,
        workspace: &BackendWorkspace,
    ) -> (DaemonKey, String, String) {
        (
            daemon.clone(),
            workspace.id.clone(),
            workspace.project_root.clone(),
        )
    }

    fn scope_update_needs_retry(&self, daemon: &DaemonKey, workspace: &BackendWorkspace) -> bool {
        self.scope_update_retries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains(&Self::scope_update_key(daemon, workspace))
    }

    fn persist_discovery_scope(
        &self,
        backend: &Arc<dyn SessionBackend>,
        daemon: &DaemonKey,
        daemon_id: Option<&str>,
        workspace: &BackendWorkspace,
        project_id: &str,
        project_identity: &str,
        project_root: &str,
        minimum_scope_rev: u64,
    ) -> Option<u64> {
        let retry_key = Self::scope_update_key(daemon, workspace);
        match backend.update_workspace_project_scope(
            &workspace.id,
            project_id,
            project_identity,
            Some(project_root),
            Some(minimum_scope_rev),
            daemon_id,
        ) {
            Ok(updated) => {
                self.scope_update_retries
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .remove(&retry_key);
                if updated.is_none() {
                    log::debug!(
                        "daemon does not persist project identity yet; keeping the discovery scope"
                    );
                }
                updated
            }
            Err(error) => {
                self.scope_update_retries
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .insert(retry_key);
                log::warn!(
                    "could not persist the project identity of daemon workspace {}: {error:#}",
                    workspace.id
                );
                None
            }
        }
    }

    fn enrich_workspace_project_scopes(
        &self,
        backend: &Arc<dyn SessionBackend>,
        daemon: &DaemonKey,
        daemon_id: Option<&str>,
        host: Option<&str>,
        known: &[AdeWorkspace],
        workspaces: &mut [BackendWorkspace],
    ) -> Result<()> {
        let cached = self.remembered_discoveries(daemon);
        for workspace in workspaces {
            if let Some(existing) = known
                .iter()
                .filter(|known| is_the_same_daemons_row(known, daemon_id, host, workspace))
                .filter(|known| known.project_identity.is_some())
                .max_by_key(|known| known.daemon_id.is_some())
            {
                let Some(project_identity) = existing.project_identity.clone() else {
                    continue;
                };
                let scope_matches = workspace.project_id.as_deref()
                    == Some(existing.project_id.as_str())
                    && workspace.project_identity.as_deref() == Some(project_identity.as_str())
                    && Path::new(&workspace.project_root) == existing.repository_path;
                let local_scope_wins = workspace.project_identity.is_none()
                    || workspace.project_scope_rev < existing.project_scope_rev
                    || (workspace.project_scope_rev == existing.project_scope_rev
                        && !scope_matches);
                if local_scope_wins {
                    let project_root = existing.repository_path.to_string_lossy().into_owned();
                    let cached_revision = cached.iter().find(|cached| {
                        cached.id == workspace.id
                            && cached.project_root == project_root
                            && cached.project_id.as_deref() == Some(existing.project_id.as_str())
                            && cached.project_identity.as_deref() == Some(project_identity.as_str())
                            && cached.project_scope_rev >= existing.project_scope_rev
                    });
                    let project_scope_rev = if cached_revision.is_some()
                        && !self.scope_update_needs_retry(daemon, workspace)
                    {
                        cached_revision.map(|cached| cached.project_scope_rev)
                    } else {
                        self.persist_discovery_scope(
                            backend,
                            daemon,
                            daemon_id,
                            workspace,
                            &existing.project_id,
                            &project_identity,
                            &project_root,
                            existing.project_scope_rev,
                        )
                    }
                    .unwrap_or(existing.project_scope_rev);
                    workspace.project_id = Some(existing.project_id.clone());
                    workspace.project_identity = Some(project_identity);
                    workspace.project_root = project_root;
                    workspace.project_scope_rev = project_scope_rev;
                    continue;
                }
            }

            if let Some(project_identity) = workspace.project_identity.clone() {
                if workspace.project_id.is_none() {
                    let project_id = project_id_from_identity(&project_identity);
                    let fallback_revision = next_project_scope_rev(workspace.project_scope_rev)?;
                    let project_root = workspace.project_root.clone();
                    let minimum_scope_rev = fallback_revision;
                    workspace.project_id = Some(project_id.clone());
                    workspace.project_scope_rev = self
                        .persist_discovery_scope(
                            backend,
                            daemon,
                            daemon_id,
                            workspace,
                            &project_id,
                            &project_identity,
                            &project_root,
                            minimum_scope_rev,
                        )
                        .unwrap_or(fallback_revision);
                    continue;
                }
                self.scope_update_retries
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .remove(&Self::scope_update_key(daemon, workspace));
                continue;
            }

            if let Some(cached) = cached.iter().find(|cached| {
                cached.id == workspace.id && cached.project_root == workspace.project_root
            }) && cached.project_identity.is_some()
            {
                let project_identity = cached.project_identity.clone().unwrap_or_default();
                let project_id = cached
                    .project_id
                    .clone()
                    .unwrap_or_else(|| project_id_from_identity(&project_identity));
                workspace.project_id = Some(project_id.clone());
                workspace.project_identity = Some(project_identity.clone());
                workspace.project_scope_rev = cached.project_scope_rev;
                if self.scope_update_needs_retry(daemon, workspace) {
                    let project_root = workspace.project_root.clone();
                    let minimum_scope_rev = workspace.project_scope_rev;
                    if let Some(project_scope_rev) = self.persist_discovery_scope(
                        backend,
                        daemon,
                        daemon_id,
                        workspace,
                        &project_id,
                        &project_identity,
                        &project_root,
                        minimum_scope_rev,
                    ) {
                        workspace.project_scope_rev = project_scope_rev;
                    }
                }
                continue;
            }

            let project_identity_path = match backend
                .resolve_repository(Path::new(&workspace.project_root))
            {
                Ok((_, project_identity_path)) => project_identity_path,
                Err(error) => {
                    log::debug!(
                        "could not resolve the Git project for daemon workspace {} at {}: {error:#}",
                        workspace.id,
                        workspace.project_root
                    );
                    continue;
                }
            };
            let project_identity = util::path_list::PathList::new(&[project_identity_path])
                .serialize()
                .paths;
            if project_identity.is_empty() {
                log::debug!(
                    "Git returned an empty project identity for daemon workspace {} at {}",
                    workspace.id,
                    workspace.project_root
                );
                continue;
            }
            let project_id = project_id_from_identity(&project_identity);
            let fallback_revision = next_project_scope_rev(workspace.project_scope_rev)?;
            let project_root = workspace.project_root.clone();
            let minimum_scope_rev = fallback_revision;
            workspace.project_id = Some(project_id.clone());
            workspace.project_identity = Some(project_identity.clone());
            workspace.project_scope_rev = self
                .persist_discovery_scope(
                    backend,
                    daemon,
                    daemon_id,
                    workspace,
                    &project_id,
                    &project_identity,
                    &project_root,
                    minimum_scope_rev,
                )
                .unwrap_or(fallback_revision);
        }
        Ok(())
    }

    fn forget_discovery(&self, daemon: &DaemonKey, wire_id: &str) {
        if let Some(held) = self
            .discoveries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get_mut(daemon)
        {
            held.retain(|record| record.id != wire_id);
        }
    }

    fn daemon_decision_lock(&self, key: DaemonKey) -> Arc<AsyncMutex<()>> {
        self.daemon_decision_locks
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .entry(key)
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    /// Lists after locking the daemon identity that the listing belongs to.
    async fn listing_under_daemon_lock(
        &self,
        backend: &Arc<dyn SessionBackend>,
        host: Option<&str>,
    ) -> Result<StableListing> {
        for _ in 0..3 {
            let route_key = DaemonKey::Host(host.map(str::to_owned));
            let route_decision = self
                .daemon_decision_lock(route_key.clone())
                .lock_arc()
                .await;
            let expected_daemon_id = match backend.instance_id() {
                Some(daemon_id) => Some(daemon_id),
                None => match backend
                    .list_workspaces_identified()
                    .with_context(|| format!("listing the workspaces on {}", host_label(host)))
                {
                    Ok(listing) => listing.daemon_id,
                    Err(error) => {
                        return Ok(StableListing {
                            _route_decision: route_decision,
                            _instance_decision: None,
                            daemon: route_key,
                            held: Err(error),
                        });
                    }
                },
            };
            let instance_decision = match expected_daemon_id.as_deref() {
                Some(daemon_id) => Some(
                    self.daemon_decision_lock(daemon_key(Some(daemon_id), host))
                        .lock_arc()
                        .await,
                ),
                None => None,
            };
            let listing = backend
                .list_workspaces_identified()
                .with_context(|| format!("refreshing the workspaces on {}", host_label(host)));
            if listing
                .as_ref()
                .is_ok_and(|listing| listing.daemon_id != expected_daemon_id)
            {
                continue;
            }

            let daemon = daemon_key(expected_daemon_id.as_deref(), host);
            let held = listing.map(|listing| listing.items);
            if let Ok(held) = &held {
                self.remember_discoveries(&daemon, held);
            }
            return Ok(StableListing {
                _route_decision: route_decision,
                _instance_decision: instance_decision,
                daemon,
                held,
            });
        }
        bail!(
            "the daemon identity on {} kept changing while resolving a workspace",
            host_label(host)
        )
    }

    /// Lists what one host's daemon holds, remembers it, and records that
    /// daemon's identity on the rows that already mirror its records. Answers
    /// with the listing and the registry as it then stands.
    ///
    /// Canonical Git scope is copied onto daemon records when supported. The
    /// other writes only rebind or hydrate a row this client already has. A
    /// record no row points at stays a discovery.
    ///
    /// **Matched by `terminal_session_id`, which is the identity.**
    /// [`AdeWorkspace::daemon_workspace_id`] returns that column when it is
    /// recorded, so a row confirmed under a daemon workspace's id addresses
    /// that workspace for every later open, attach, rename and kill.
    ///
    /// The first successful listing reveals the daemon identity. The registry
    /// is then reread under that daemon's lock before any rebind.
    async fn list_host_workspaces(
        &self,
        backend: &Arc<dyn SessionBackend>,
        host: Option<&str>,
    ) -> Result<(DaemonKey, Vec<BackendWorkspace>, Vec<AdeWorkspace>)> {
        self.ensure_history_backfill().await?;
        for _ in 0..3 {
            let route_lock = self.daemon_decision_lock(DaemonKey::Host(host.map(str::to_owned)));
            let _route_decision = route_lock.lock().await;
            let initial = backend
                .list_workspaces_identified()
                .with_context(|| format!("listing the workspaces on {}", host_label(host)))?;
            let instance_lock = initial
                .daemon_id
                .as_deref()
                .map(|daemon_id| self.daemon_decision_lock(daemon_key(Some(daemon_id), host)));
            let _instance_decision = match instance_lock.as_ref() {
                Some(lock) => Some(lock.lock().await),
                None => None,
            };
            let listing = backend
                .list_workspaces_identified()
                .with_context(|| format!("refreshing the workspaces on {}", host_label(host)))?;
            if listing.daemon_id != initial.daemon_id {
                continue;
            }

            let instance_id = listing.daemon_id;
            let key = daemon_key(instance_id.as_deref(), host);
            let mut held = listing.items;
            let mut known = self.registry.list_workspaces()?;
            self.enrich_workspace_project_scopes(
                backend,
                &key,
                instance_id.as_deref(),
                host,
                &known,
                &mut held,
            )?;
            let mut changed = false;
            for existing in known.iter_mut().filter(|known| {
                known.project_identity.is_none()
                    && known.remote_host.as_deref() == host
                    && !held.iter().any(|workspace| {
                        is_the_same_daemons_row(known, instance_id.as_deref(), host, workspace)
                    })
            }) {
                let project_identity_path = match backend
                    .resolve_repository(&existing.repository_path)
                {
                    Ok((_, project_identity_path)) => project_identity_path,
                    Err(error) => {
                        log::debug!(
                            "could not resolve the Git project for registry workspace {} at {}: {error:#}",
                            existing.id,
                            existing.repository_path.display()
                        );
                        continue;
                    }
                };
                let project_identity = util::path_list::PathList::new(&[project_identity_path])
                    .serialize()
                    .paths;
                if project_identity.is_empty() {
                    log::debug!(
                        "Git returned an empty project identity for registry workspace {} at {}",
                        existing.id,
                        existing.repository_path.display()
                    );
                    continue;
                }
                let project_id = project_id_from_identity(&project_identity);
                let project_scope_rev = next_project_scope_rev(existing.project_scope_rev)?;
                let (applied, stored) = self
                    .registry
                    .update_project_scope(
                        existing.id.clone(),
                        project_scope_rev,
                        project_id.clone(),
                        project_identity.clone(),
                    )
                    .await
                    .with_context(|| {
                        format!(
                            "recording resolved project identity for workspace {}",
                            existing.id
                        )
                    })?;
                *existing = stored;
                changed |= applied;
            }
            for workspace in &held {
                // Prefer the row that already owns this identity over a matching
                // legacy row, or the update can collide with the unique index.
                let existing = known
                    .iter()
                    .filter(|known| {
                        is_the_same_daemons_row(known, instance_id.as_deref(), host, workspace)
                    })
                    .max_by_key(|known| known.daemon_id.is_some());
                if let Some(existing) = existing {
                    if let Some(project_identity) = workspace.project_identity.as_deref() {
                        let project_id = workspace
                            .project_id
                            .clone()
                            .unwrap_or_else(|| crate::project_id_from_identity(project_identity));
                        let daemon_wins = existing.project_identity.is_none()
                            || workspace.project_scope_rev > existing.project_scope_rev;
                        if daemon_wins {
                            self.registry
                                .update_repository_scope(
                                    existing.id.clone(),
                                    PathBuf::from(&workspace.project_root),
                                    workspace.project_scope_rev,
                                    project_id,
                                    project_identity.to_owned(),
                                )
                                .await
                                .with_context(|| {
                                    format!(
                                        "recording daemon project identity for workspace {}",
                                        existing.id
                                    )
                                })?;
                            changed = true;
                        }
                    } else if let Some(project_identity) = existing.project_identity.as_deref() {
                        let updated = backend.update_workspace_project_scope(
                            &workspace.id,
                            &existing.project_id,
                            project_identity,
                            Some(&existing.repository_path.to_string_lossy()),
                            Some(existing.project_scope_rev),
                            instance_id.as_deref(),
                        );
                        match updated {
                            Ok(Some(project_scope_rev)) => {
                                self.registry
                                    .update_repository_scope(
                                        existing.id.clone(),
                                        existing.repository_path.clone(),
                                        project_scope_rev,
                                        existing.project_id.clone(),
                                        project_identity.to_owned(),
                                    )
                                    .await?;
                                changed = true;
                            }
                            Ok(None) => {}
                            Err(error) => log::warn!(
                                "could not repair project identity for workspace {}: {error:#}",
                                existing.id
                            ),
                        }
                    }
                    if existing.remote_host.as_deref() != host || existing.daemon_id != instance_id
                    {
                        let rebound_result = self
                            .registry
                            .rebind_workspace_route(
                                existing.id.clone(),
                                existing.remote_host.clone(),
                                existing.daemon_id.clone(),
                                host.map(str::to_owned),
                                instance_id.clone(),
                            )
                            .await;
                        if let Err(error) = rebound_result
                            && self
                                .registry
                                .resolve_rebind_conflict(
                                    existing.id.clone(),
                                    existing.remote_host.clone(),
                                    existing.daemon_id.clone(),
                                    host.map(str::to_owned),
                                    instance_id.clone(),
                                    workspace.id.clone(),
                                )
                                .await?
                                .is_none()
                        {
                            return Err(error).with_context(|| {
                                format!(
                                    "rebinding workspace {} to {}",
                                    workspace.id,
                                    host_label(host)
                                )
                            });
                        }
                        changed = true;
                    }
                }
            }
            self.remember_discoveries(&key, &held);
            let rows = if changed {
                self.registry.list_workspaces()?
            } else {
                known
            };
            return Ok((key, held, rows));
        }
        bail!(
            "the daemon identity on {} kept changing while listing workspaces",
            host_label(host)
        )
    }

    async fn ensure_history_backfill(&self) -> Result<()> {
        let mut complete = self.history_backfilled.lock().await;
        if !*complete {
            self.registry
                .backfill_project_identities_from_workspace_history()
                .await
                .context("backfilling ADE project identities")?;
            *complete = true;
        }
        Ok(())
    }

    /// The argv a terminal pane runs to attach to this workspace.
    ///
    /// Attach-or-create, so reopening a pane on a live session reattaches to it
    /// with everything still running.
    ///
    /// **Local either way.** A remote workspace's argv still names *this*
    /// machine's attach client, pointed at the host's forwarded socket; it is
    /// one more channel on the host's single ssh connection, never an ssh
    /// invocation of its own.
    pub fn attach_command(&self, workspace: &AdeWorkspace) -> Result<Attached> {
        self.backend_for(workspace)?.attach(
            &Self::session_spec(workspace),
            workspace.daemon_id.as_deref(),
        )
    }

    /// Adds one more session to the workspace, and hands back its backend id
    /// with the argv that attaches to it.
    ///
    /// The plural half of [`Self::attach_command`]: that one makes or finds
    /// *the* session, this one puts a sibling beside it. For a remote workspace,
    /// `working_directory` is resolved by that host's backend.
    ///
    /// Nothing here writes the layout: the window that opened the terminal
    /// captures it and pushes it, which is the same path a split or a drag
    /// takes.
    pub async fn create_session_in_workspace(
        &self,
        workspace: &AdeWorkspace,
        working_directory: &Path,
    ) -> Result<(String, Vec<String>, Option<String>)> {
        let backend = self.backend_for(workspace)?;
        let spec = Self::session_spec(workspace);
        let (session, daemon_id) = backend
            .create_session_in_workspace_identified(
                spec.id.as_str(),
                working_directory,
                workspace.daemon_id.as_deref(),
            )
            .with_context(|| format!("creating another session in {}", spec.id))?;
        let daemon_id = workspace.daemon_id.as_deref().or(daemon_id.as_deref());
        let argv = backend.attach_session(&session, daemon_id)?;
        if workspace.daemon_id.is_none() && daemon_id.is_some() {
            self.registry
                .update_remote_host_and_daemon_id(
                    workspace.id.clone(),
                    workspace.remote_host.clone(),
                    daemon_id.map(str::to_owned),
                )
                .await?;
        }
        Ok((session, argv, daemon_id.map(str::to_owned)))
    }

    /// The argv a terminal pane runs to attach to one session **named by the
    /// backend** — the tabs of a layout.
    ///
    /// Never creates, unlike [`Self::attach_command`]: a layout names sessions
    /// that already exist, and opening a workspace must render what is there
    /// rather than spawn what is missing.
    pub fn attach_session_command(
        &self,
        workspace: &AdeWorkspace,
        session_id: &str,
    ) -> Result<Vec<String>> {
        self.backend_for(workspace)?
            .attach_session(session_id, workspace.daemon_id.as_deref())
    }

    pub fn resize_session(
        &self,
        workspace: &AdeWorkspace,
        session_id: &str,
        cols: u16,
        rows: u16,
    ) -> Result<()> {
        self.backend_for(workspace)?.resize_session(
            session_id,
            cols,
            rows,
            workspace.daemon_id.as_deref(),
        )
    }

    /// The workspace's layout as the backend holds it, with the revision that
    /// guards the next write.
    ///
    /// An error means there is nothing to render — the backend has never heard
    /// of this workspace, or could not be asked — and the caller falls back to
    /// the single-terminal open.
    pub fn open_workspace_layout(&self, workspace: &AdeWorkspace) -> Result<WorkspaceLayout> {
        self.backend_for(workspace)?.open_workspace(
            &workspace.daemon_workspace_id(),
            workspace.daemon_id.as_deref(),
        )
    }

    pub async fn open_workspace_layout_identified(
        &self,
        workspace: &mut AdeWorkspace,
    ) -> Result<WorkspaceLayout> {
        let (layout, daemon_id) = self.backend_for(workspace)?.open_workspace_identified(
            &workspace.daemon_workspace_id(),
            workspace.daemon_id.as_deref(),
        )?;
        if workspace.daemon_id.is_none() && daemon_id.is_some() {
            self.registry
                .update_remote_host_and_daemon_id(
                    workspace.id.clone(),
                    workspace.remote_host.clone(),
                    daemon_id.clone(),
                )
                .await?;
            workspace.daemon_id = daemon_id;
        }
        Ok(layout)
    }

    /// Stores a new layout for the workspace at `rev`, which must be one past
    /// the revision the caller last saw.
    ///
    /// A stale revision comes back as an error: the caller lost a race with
    /// another client and should re-fetch, not retry.
    pub fn update_layout(
        &self,
        workspace: &AdeWorkspace,
        layout: &LayoutDoc,
        rev: u64,
    ) -> Result<()> {
        self.backend_for(workspace)?.update_layout(
            &workspace.daemon_workspace_id(),
            layout,
            rev,
            workspace.daemon_id.as_deref(),
        )
    }

    /// Kills the one session a closed terminal tab was showing.
    ///
    /// Destructive, and reached from exactly one control: closing a terminal
    /// tab (operator ruling, 2026-08-04). Closing the *window* detaches and
    /// kills nothing, and the workspace-level kill stays
    /// [`Self::kill_workspace_session`].
    pub fn kill_session(&self, workspace: &AdeWorkspace, session_id: &str) -> Result<()> {
        self.backend_for(workspace)?
            .kill_session(session_id, workspace.daemon_id.as_deref())
    }

    /// Replace the session daemon on one host, because the operator asked.
    ///
    /// Blocking, like every other backend call here: it builds a binary and
    /// talks to ssh, so callers run it off the main thread. It goes through
    /// [`Self::backend_for_host`] rather than making its own backend, because
    /// the upgrade has to happen on the connection whose daemon is being
    /// replaced — a second backend would upgrade the host behind the first
    /// one's back and leave it holding a channel to a process that is gone.
    pub fn upgrade_host_daemon(&self, destination: &str) -> Result<DaemonUpgradeOutcome> {
        self.backend_for_host(Some(destination))?.upgrade_daemon()
    }

    /// Whether `destination` is known to run a daemon behind this client's.
    ///
    /// Deliberately **not** [`Self::backend_for_host`]: this is called from a
    /// render, and that one creates the backend it cannot find, which would
    /// have drawing a button register a host connection as a side effect. A
    /// host no backend has touched has never been probed, so nothing is known
    /// about it, so the answer is `false`.
    pub fn host_daemon_stale(&self, destination: &str) -> bool {
        self.backends
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&Some(destination.to_owned()))
            .is_some_and(|backend| backend.daemon_stale())
    }

    /// A stream that yields once every time some host's answer to
    /// [`Self::host_daemon_stale`] changes.
    ///
    /// **Without this the arrow is wrong for as long as the window sits
    /// still.** The verdict is recorded by a probe on a background thread, and
    /// the only thing that reads it is a render — so an arrow would appear, or
    /// stop being true, on the user's next unrelated click rather than when the
    /// fact changed. A subscriber turns each item into a `cx.notify()`.
    ///
    /// Carries no payload. A sidebar redraws all its rows or none, so *which*
    /// host moved would be a fact with no reader; every subscriber re-asks
    /// [`Self::host_daemon_stale`] for the rows it is drawing anyway.
    ///
    /// **Many subscribers, unlike [`Self::subscribe_status`]** — one per
    /// sidebar, and a second window must not silence the first. Dropping the
    /// receiver is how a closed window unsubscribes.
    pub fn watch_daemon_freshness(&self) -> Receiver<()> {
        let (sender, receiver) = smol::channel::unbounded();
        self.freshness_watchers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(sender);
        receiver
    }

    /// The one [`crate::DaemonFreshnessObserver`] every backend gets, which is
    /// what turns a backend's private discovery into the public stream.
    ///
    /// Registered per backend rather than per subscriber, so a host whose
    /// backend is created long after a sidebar exists still reaches it — the
    /// first verdict for a host is exactly the one that makes the arrow appear,
    /// and it is recorded before any subscriber could have named that host.
    fn freshness_announcer(&self) -> crate::DaemonFreshnessObserver {
        let watchers = self.freshness_watchers.clone();
        Arc::new(move || {
            // Unbounded, so a send only fails on a receiver that is gone with
            // its window. Pruning those here is what keeps a long session of
            // opened and closed windows from growing this list forever.
            watchers
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .retain(|watcher| watcher.try_send(()).is_ok());
        })
    }

    /// One listing **per host**, rather than one probe per workspace: startup
    /// reconciles every row, and each probe costs the backend a round trip.
    /// Sound because a listing reports exactly the sessions this app owns on
    /// that host. A host with nothing to probe and nothing to discover is never
    /// asked, and never connected to.
    ///
    /// Session ids are only unique *within* a host — two hosts can hold
    /// `ade-main-012345` at once — so the live sets are kept apart rather than
    /// unioned, and a workspace is only ever matched against its own host's.
    ///
    /// A host that cannot be reached fails alone: its rows come back
    /// [`SessionState::Unknown`] with their stored status untouched, its
    /// discoveries come back from the last successful listing and read
    /// `Unknown` too, and the reason lands in [`Reconciled::host_errors`].
    ///
    /// **`discover` asks every contacted host what else it holds**, so a
    /// workspace the daemon holds and this client has never used is on screen
    /// rather than invisible — as a discovery, never as a row. Only hosts this
    /// pass was already going to contact, plus those a backend already exists
    /// for: reconciliation must not turn a registry full of machines that are
    /// switched off into a wall of ssh attempts.
    async fn reconcile(
        &self,
        mut workspaces: Vec<AdeWorkspace>,
        discover: bool,
    ) -> Result<Reconciled> {
        // Every host a backend already exists for — this machine included, when
        // it has a daemon at all — because a host reached by one connect keeps
        // its discoveries without a row naming it. A map's order is nobody's;
        // this pass's must be the same twice.
        let mut discovering: Vec<Option<String>> = if discover {
            self.backends
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .keys()
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        discovering.sort();

        let mut hosts = discovering.clone();
        for workspace in &workspaces {
            if !hosts.contains(&workspace.remote_host) {
                hosts.push(workspace.remote_host.clone());
            }
        }

        let mut host_errors = self.status_errors();
        let mut outcomes: Vec<HostOutcome> = Vec::new();
        for host in hosts {
            let anything_to_probe = workspaces.iter().any(|workspace| {
                workspace.remote_host == host && workspace.terminal_session_id.is_some()
            });
            let contacting = anything_to_probe || discovering.contains(&host);
            // `None` is what makes a host's entries read `Unknown` below; an
            // empty set is a host that was not asked because it holds nothing
            // this pass could probe.
            let mut live = None;
            let mut held = None;
            let mut daemon = DaemonKey::Host(host.clone());
            if !contacting {
                live = Some(HashSet::new());
            } else {
                match self.backend_for_host(host.as_deref()) {
                    Err(error) => {
                        host_errors.push((host_label(host.as_deref()), format!("{error:#}")))
                    }
                    Ok(backend) => {
                        let current_daemon_id = backend.instance_id();
                        daemon = daemon_key(current_daemon_id.as_deref(), host.as_deref());
                        // Before the session listing, so a row this pass rebound
                        // is probed by the same pass. A host that will not
                        // answer the first question will not answer the second
                        // either, so it fails once and its rows go untouched.
                        let listed = match discover {
                            false => true,
                            true => {
                                match self.list_host_workspaces(&backend, host.as_deref()).await {
                                    Ok((listed_daemon, records, rows)) => {
                                        daemon = listed_daemon;
                                        // Rows in scope pick up whatever the rebind
                                        // just wrote for them.
                                        for workspace in &mut workspaces {
                                            if let Some(updated) =
                                                rows.iter().find(|row| row.id == workspace.id)
                                            {
                                                *workspace = updated.clone();
                                            }
                                        }
                                        held = Some(records);
                                        true
                                    }
                                    Err(error) => {
                                        host_errors.push((
                                            host_label(host.as_deref()),
                                            format!("{error:#}"),
                                        ));
                                        false
                                    }
                                }
                            }
                        };
                        if listed {
                            match backend.list_identified().context("listing live sessions") {
                                Ok(listing)
                                    if daemon_key(
                                        listing.daemon_id.as_deref(),
                                        host.as_deref(),
                                    ) == daemon =>
                                {
                                    live = Some(
                                        listing
                                            .items
                                            .into_iter()
                                            .map(|session| session.id)
                                            .collect(),
                                    );
                                }
                                Ok(listing) if !discover => {
                                    daemon =
                                        daemon_key(listing.daemon_id.as_deref(), host.as_deref());
                                    live = Some(
                                        listing
                                            .items
                                            .into_iter()
                                            .map(|session| session.id)
                                            .collect(),
                                    );
                                }
                                Ok(_) => {}
                                Err(error) => host_errors
                                    .push((host_label(host.as_deref()), format!("{error:#}"))),
                            }
                        }
                    }
                }
            }
            outcomes.push(HostOutcome {
                daemon,
                host,
                live,
                held,
            });
        }

        // Prefer a spelling that answered over another spelling of the same
        // daemon that failed, so a reachable daemon does not render its cached
        // discoveries as unknown.
        outcomes.sort_by_key(|outcome| (outcome.held.is_none(), outcome.live.is_none()));

        // The rows the user worked in most recently lead, with the uuid as a
        // tie-break so the order cannot come out differently twice.
        workspaces.sort_by(|a, b| {
            b.last_opened_at
                .cmp(&a.last_opened_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        let mut rows = Vec::with_capacity(workspaces.len());
        for mut workspace in workspaces {
            let live = outcomes
                .iter()
                .find(|outcome| {
                    outcome.host == workspace.remote_host
                        && row_is_on(&workspace, &outcome.daemon, outcome.host.as_deref())
                })
                .and_then(|outcome| outcome.live.as_ref());
            let state = match workspace.terminal_session_id.clone().map(SessionId::from) {
                None => SessionState::NeverCreated,
                Some(session) => match live {
                    Some(live) => {
                        self.record_probe(&mut workspace, live.contains(&session))
                            .await?
                    }
                    None => SessionState::Unknown,
                },
            };
            rows.push((workspace, state));
        }

        let mut rendered: HashSet<DaemonKey> = HashSet::new();
        let mut discovered: Vec<(Option<String>, BackendWorkspace, SessionState)> = Vec::new();
        for outcome in &outcomes {
            // One daemon renders once, however many spellings reached it.
            if !discover || !rendered.insert(outcome.daemon.clone()) {
                continue;
            }
            // A failed listing is not a disconnect: the host keeps whatever it
            // last held, as `Unknown`, until an authoritative listing replaces
            // it.
            let (held, listed_now) = match &outcome.held {
                Some(held) => (held.clone(), true),
                None => (self.remembered_discoveries(&outcome.daemon), false),
            };
            // Every spelling of this daemon: a wire id a row already uses is not
            // a discovery, however that row's host was typed.
            let used: HashSet<String> = rows
                .iter()
                .filter(|(row, _)| row_is_on(row, &outcome.daemon, outcome.host.as_deref()))
                .map(|(row, _)| row.daemon_workspace_id())
                .collect();
            for record in held {
                if used.contains(&record.id) {
                    continue;
                }
                let state = match (&outcome.live, listed_now) {
                    (Some(live), true) => {
                        if live.contains(&SessionId::from(record.id.clone())) {
                            SessionState::Alive
                        } else {
                            SessionState::Dead
                        }
                    }
                    _ => SessionState::Unknown,
                };
                discovered.push((outcome.host.clone(), record, state));
            }
        }
        // Daemon creation order, with the opaque id only as a tie-break: sorting
        // by the id alone would put a new discovery at a position unrelated to
        // its history.
        discovered.sort_by(|a, b| {
            (a.0.as_deref(), a.1.created_at, a.1.id.as_str()).cmp(&(
                b.0.as_deref(),
                b.1.created_at,
                b.1.id.as_str(),
            ))
        });

        let entries = rows
            .into_iter()
            .map(|(workspace, state)| WorkspaceEntry::Persisted(workspace, state))
            .chain(
                discovered
                    .into_iter()
                    .map(
                        |(remote_host, workspace, state)| WorkspaceEntry::Discovered {
                            remote_host,
                            workspace,
                            state,
                        },
                    ),
            )
            .collect();
        Ok(Reconciled {
            entries,
            host_errors,
        })
    }

    /// The backend that owns this workspace's sessions.
    fn backend_for(&self, workspace: &AdeWorkspace) -> Result<Arc<dyn SessionBackend>> {
        self.backend_for_host(workspace.remote_host.as_deref())
    }

    /// The backend for one host, created on first use and kept.
    ///
    /// The single place a backend is named. Kept because a host's backend owns
    /// that host's one ssh connection; creating a second per operation is
    /// exactly the thing the transport exists to avoid.
    fn backend_for_host(&self, host: Option<&str>) -> Result<Arc<dyn SessionBackend>> {
        let key = host.map(str::to_owned);
        {
            let backends = self.backends.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(backend) = backends.get(&key) {
                return Ok(backend.clone());
            }
        }

        let Some(host) = host else {
            bail!("local ADE workspaces require Unix; use a Unix remote host");
        };
        // Outside the lock: constructing a backend contacts nothing, but
        // holding a lock across anything fallible is how a poisoned mutex
        // becomes everyone's problem.
        let created: Arc<dyn SessionBackend> =
            Arc::new(DaemonBackend::remote(host).with_context(|| format!("reaching host {host}"))?);

        let (backend, is_new) = {
            let mut backends = self.backends.lock().unwrap_or_else(|e| e.into_inner());
            match backends.entry(key) {
                // Another caller got here first; theirs owns the connection.
                Entry::Occupied(entry) => (entry.get().clone(), false),
                Entry::Vacant(entry) => (entry.insert(created).clone(), true),
            }
        };
        if is_new {
            // First of the two, and before anything else touches this backend:
            // opening the event pump below already connects, and connecting is
            // what records the first freshness verdict. Registering after it
            // would miss exactly the verdict that puts the arrow on screen.
            backend.observe_daemon_freshness(self.freshness_announcer());
            // Never while holding `backends`: this takes `status`.
            self.forward_status(host, &backend);
        }
        Ok(backend)
    }

    /// Feeds a newly created backend's events into the merged streams, if
    /// anybody is listening.
    fn forward_status(&self, host: &str, backend: &Arc<dyn SessionBackend>) {
        if let Err(error) = self.ensure_pump(Some(host), backend) {
            self.record_status_error(host, &error);
        }
    }

    fn record_status_error(&self, host: &str, error: &anyhow::Error) {
        self.status_errors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                host.to_owned(),
                format!("status updates are off: {error:#}"),
            );
    }

    fn status_errors(&self) -> Vec<(String, String)> {
        self.status_errors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(host, message)| (host.clone(), message.clone()))
            .collect()
    }

    /// Creates the detached session for a workspace and records it as running,
    /// or records `error` and returns why.
    async fn start_session(&self, workspace: &mut AdeWorkspace) -> Result<()> {
        let spec = Self::session_spec(workspace);

        let created = self.backend_for(workspace).and_then(|backend| {
            // Unclaimed rows are permissive; persisted identities are fenced.
            let (session, daemon_id) =
                backend.create_identified(&spec, workspace.daemon_id.as_deref())?;
            Ok((session, daemon_id))
        });
        let (session, daemon_id) = match created {
            Ok(pair) => pair,
            Err(error) => {
                self.set_status(workspace, WorkspaceStatus::Error).await?;
                return Err(error).with_context(|| format!("creating session {}", spec.id));
            }
        };

        let daemon_id = workspace.daemon_id.clone().or(daemon_id);
        self.registry
            .update_remote_host_and_daemon_id(
                workspace.id.clone(),
                workspace.remote_host.clone(),
                daemon_id.clone(),
            )
            .await?;
        workspace.daemon_id = daemon_id;

        self.adopt_session(workspace, session).await
    }

    /// Points the workspace at a session known to exist and marks it running.
    async fn adopt_session(&self, workspace: &mut AdeWorkspace, session: SessionId) -> Result<()> {
        if workspace.terminal_session_id.as_deref() != Some(session.as_str()) {
            let session = session.into_string();
            self.registry
                .update_terminal_session_id(workspace.id.clone(), Some(session.clone()))
                .await?;
            workspace.terminal_session_id = Some(session);
        }
        self.set_status(workspace, WorkspaceStatus::Running).await
    }

    /// What the backend is asked to create or attach to for this workspace: the
    /// id the daemon knows it by, rooted at its checkout.
    ///
    /// [`AdeWorkspace::daemon_workspace_id`] rather than the freshly derived
    /// name, because a renamed workspace still has to attach to the session it
    /// already has.
    ///
    /// `repository_path` is taken verbatim, which for a remote workspace means
    /// a path on *its* host — the backend for that host is the one that will
    /// resolve it, and this machine's filesystem never enters into it.
    fn session_spec(workspace: &AdeWorkspace) -> SessionSpec {
        let spec = SessionSpec::new(
            workspace.daemon_workspace_id(),
            workspace.repository_path.clone(),
        );
        match workspace.project_identity.as_deref() {
            Some(project_identity) => {
                spec.with_project_scope(&workspace.project_id, project_identity)
            }
            None => spec,
        }
    }

    async fn record_probe(
        &self,
        workspace: &mut AdeWorkspace,
        alive: bool,
    ) -> Result<SessionState> {
        let (state, status) = if alive {
            (SessionState::Alive, WorkspaceStatus::Running)
        } else {
            (SessionState::Dead, WorkspaceStatus::Disconnected)
        };
        self.set_status(workspace, status).await?;
        Ok(state)
    }

    /// Writes a status only when it actually changed; reconciliation runs over
    /// every workspace on every startup and most of them will not have moved.
    async fn set_status(
        &self,
        workspace: &mut AdeWorkspace,
        status: WorkspaceStatus,
    ) -> Result<()> {
        if workspace.status != status {
            self.registry
                .update_status(workspace.id.clone(), status)
                .await?;
            workspace.status = status;
        }
        Ok(())
    }

    fn get(&self, id: &WorkspaceId) -> Result<AdeWorkspace> {
        self.registry
            .get_workspace(id.clone())?
            .with_context(|| format!("no workspace with id {id}"))
    }
}

/// Where one host's pushed events go once they are off the wire: onto whichever
/// merged stream each kind belongs to.
///
/// One of these for the whole service, shared by every host's pump. It exists
/// so that "who is listening" is a question asked at delivery time rather than
/// at subscribe time — a caller that takes the layout stream after the status
/// one must not cost a second connection to every host.
#[derive(Default)]
struct EventFanout {
    status: Mutex<Option<Sender<StatusEvent>>>,
    /// Layouts and removals both: see [`WorkspaceLifecycleService::subscribe_layout`].
    layout: Mutex<Option<Sender<WorkspaceEvent>>>,
    workspace_changes: Mutex<Vec<Sender<()>>>,
}

impl EventFanout {
    /// Whether nobody has subscribed to anything, which is the one case where
    /// opening a host's push channel would be pure cost.
    fn is_idle(&self) -> bool {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none()
            && self
                .layout
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_none()
            && self
                .workspace_changes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
    }

    /// Hands one event to its stream. `false` once both streams are gone, which
    /// is what ends a pump.
    ///
    /// A closed receiver clears its slot rather than being retried: dropping the
    /// receiver is the only unsubscribe there is, and a slot left holding a dead
    /// sender would keep [`Self::is_idle`] answering "somebody is listening"
    /// forever.
    fn deliver(&self, remote_host: &Option<String>, identified: IdentifiedDaemonEvent) -> bool {
        let IdentifiedDaemonEvent { daemon_id, event } = identified;
        match event {
            DaemonEvent::Session(event) => send_or_clear(&self.status, event),
            DaemonEvent::Layout(event) => send_or_clear(
                &self.layout,
                WorkspaceEvent::Layout {
                    remote_host: remote_host.clone(),
                    daemon_id,
                    event,
                },
            ),
            DaemonEvent::WorkspaceChanged(event) => {
                send_or_clear(
                    &self.layout,
                    WorkspaceEvent::Layout {
                        remote_host: remote_host.clone(),
                        daemon_id,
                        event,
                    },
                );
                self.notify_workspace_changes();
            }
            DaemonEvent::WorkspaceReset(event) => {
                send_or_clear(
                    &self.layout,
                    WorkspaceEvent::Reset {
                        remote_host: remote_host.clone(),
                        daemon_id,
                        event,
                    },
                );
                self.notify_workspace_changes();
            }
            DaemonEvent::WorkspaceRemoved { workspace_id } => {
                send_or_clear(
                    &self.layout,
                    WorkspaceEvent::Removed {
                        remote_host: remote_host.clone(),
                        daemon_id,
                        workspace_id,
                    },
                );
                self.notify_workspace_changes();
            }
        }
        !self.is_idle()
    }

    fn notify_workspace_changes(&self) {
        self.workspace_changes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retain(|sender| sender.try_send(()).is_ok());
    }
}

/// Push one event onto an unbounded sender, forgetting the sender if the other
/// end has gone.
fn send_or_clear<T>(slot: &Mutex<Option<Sender<T>>>, event: T) {
    let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
    let Some(sender) = slot.as_ref() else {
        return;
    };
    // Unbounded, so this never blocks and a failure means exactly one thing.
    if sender.try_send(event).is_err() {
        *slot = None;
    }
}

/// One forwarder: a backend's own push channel into the merged streams.
///
/// A plain thread rather than a task, for the same reason the backend's own
/// status reader is one — it spends its life blocked on a channel, which is the
/// one thing an executor thread must not do. It ends when the backend's stream
/// closes or every merged stream is dropped, so a backend that goes away costs
/// a thread until its stream does.
fn pump_events(
    backend: &Arc<dyn SessionBackend>,
    remote_host: Option<String>,
    fanout: Arc<EventFanout>,
    pump_generation: Arc<AtomicU64>,
    generation: u64,
) -> Result<()> {
    let events = backend.subscribe_events()?;
    std::thread::Builder::new()
        .name("ade-event-merge".to_owned())
        .spawn(move || {
            while let Ok(event) = smol::block_on(events.recv()) {
                if pump_generation.load(Ordering::Acquire) != generation {
                    break;
                }
                if !fanout.deliver(&remote_host, event) {
                    // Nobody is listening to either merged stream any more.
                    break;
                }
            }
        })
        .context("spawning an event forwarder")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Identified, LayoutEvent, SessionChange};
    use gpui::AppContext as _;

    /// A service whose backend cannot work, for the paths that must not reach
    /// it.
    async fn backendless_service(name: &'static str) -> WorkspaceLifecycleService {
        let registry = AdeWorkspaceRegistry::open_test_db(name).await;
        let backend = Arc::new(FakeBackend::failing(
            "local",
            "no session backend in this test",
        ));
        WorkspaceLifecycleService::with_backend(registry, backend)
    }

    #[cfg(not(unix))]
    #[gpui::test]
    async fn test_default_backend_reports_local_daemon_unavailable() {
        let registry = AdeWorkspaceRegistry::open_test_db(
            "test_default_backend_reports_local_daemon_unavailable",
        )
        .await;
        let error = WorkspaceLifecycleService::new(registry)
            .ensure_host_workspaces(None)
            .await
            .expect_err("a non-Unix host must not start the Unix daemon proxy");
        assert!(error.to_string().contains("require Unix"));
    }

    /// Ensuring a host has to *fail* when the host cannot be reached: the
    /// caller that asks — a window that has just connected — falls back to a
    /// plain terminal on the error, and a silent success would leave it opening
    /// a workspace against a daemon that is not there.
    #[gpui::test]
    async fn test_ensure_host_reports_a_host_it_cannot_reach() {
        let registry =
            AdeWorkspaceRegistry::open_test_db("test_ensure_host_reports_a_host_it_cannot_reach")
                .await;
        let backend = Arc::new(FakeBackend::failing(
            "local",
            "ssh: connect: no route to host",
        ));
        let service = WorkspaceLifecycleService::with_backend(registry, backend);

        assert!(service.ensure_host_workspaces(None).await.is_err());
    }

    /// The records one pass found that no row of this client's points at.
    fn discoveries(entries: &[WorkspaceEntry]) -> Vec<&WorkspaceEntry> {
        entries
            .iter()
            .filter(|entry| matches!(entry, WorkspaceEntry::Discovered { .. }))
            .collect()
    }

    fn record(id: &str, name: &str, root: &str, created_at: u64) -> BackendWorkspace {
        BackendWorkspace {
            id: id.to_owned(),
            name: name.to_owned(),
            project_id: None,
            project_identity: None,
            project_root: root.to_owned(),
            project_scope_rev: 0,
            created_at,
        }
    }

    fn row_on(
        daemon_id: &str,
        host: &str,
        wire_id: &str,
        name: &str,
        opened_at: i64,
    ) -> AdeWorkspace {
        let mut row = AdeWorkspace {
            terminal_session_id: Some(wire_id.to_owned()),
            daemon_id: Some(daemon_id.to_owned()),
            remote_host: Some(host.to_owned()),
            ..AdeWorkspace::new(name, "project-a", "/home/user/main")
        };
        row.last_opened_at = OffsetDateTime::from_unix_timestamp(opened_at).unwrap();
        row
    }

    #[gpui::test]
    async fn test_adoption_ignores_another_daemons_colliding_row() {
        let registry =
            AdeWorkspaceRegistry::open_test_db("test_adoption_ignores_other_daemon").await;
        let stale = row_on("daemon-a", "a.example", "ade-main-000001", "stale", 9_000);
        registry.create_workspace(stale.clone()).await.unwrap();
        let remote = Arc::new(
            FakeBackend::new("remote")
                .identified("daemon-b")
                .holding(vec![record(
                    "ade-main-000001",
                    "main",
                    "/home/user/main",
                    1,
                )]),
        );
        let service =
            WorkspaceLifecycleService::with_backend(registry, Arc::new(FakeBackend::new("local")))
                .with_backend_for_host("a.example", remote);

        let (adopted, created) = service
            .adopt_or_create_workspace(
                PathBuf::from("/home/user/main"),
                Some("a.example".to_owned()),
                None,
            )
            .await
            .unwrap();

        assert!(!created);
        assert_ne!(adopted.id, stale.id);
        assert_eq!(adopted.daemon_id.as_deref(), Some("daemon-b"));
        assert_eq!(
            adopted.terminal_session_id.as_deref(),
            Some("ade-main-000001")
        );
    }

    #[gpui::test]
    async fn test_adoption_reuses_an_identified_row_across_host_aliases() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_adoption_reuses_alias_row").await;
        let existing = row_on("daemon-a", "old.example", "ade-main-000001", "main", 1_000);
        registry.create_workspace(existing.clone()).await.unwrap();
        let remote = Arc::new(
            FakeBackend::new("remote")
                .identified("daemon-a")
                .holding(vec![record(
                    "ade-main-000001",
                    "main",
                    "/home/user/main",
                    1,
                )]),
        );
        let service =
            WorkspaceLifecycleService::with_backend(registry, Arc::new(FakeBackend::new("local")))
                .with_backend_for_host("new.example", remote);

        let (adopted, created) = service
            .adopt_or_create_workspace(
                PathBuf::from("/home/user/main"),
                Some("new.example".to_owned()),
                None,
            )
            .await
            .unwrap();

        assert!(!created);
        assert_eq!(adopted.id, existing.id);
        assert_eq!(service.registry().list_workspaces().unwrap().len(), 1);
    }

    #[gpui::test]
    async fn test_adoption_reuses_a_killed_workspace_row() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_adoption_reuses_killed_row").await;
        let mut retained = row_on("daemon-a", "a.example", "ade-main-000001", "main", 1_000);
        retained.terminal_session_id = None;
        retained.daemon_id = None;
        retained.status = WorkspaceStatus::Stopped;
        registry.create_workspace(retained.clone()).await.unwrap();
        let remote = Arc::new(FakeBackend::new("remote").identified("daemon-a"));
        let service =
            WorkspaceLifecycleService::with_backend(registry, Arc::new(FakeBackend::new("local")))
                .with_backend_for_host("a.example", remote);

        let (adopted, created) = service
            .adopt_or_create_workspace(
                PathBuf::from("/home/user/main"),
                Some("a.example".to_owned()),
                None,
            )
            .await
            .unwrap();

        assert!(!created);
        assert_eq!(adopted.id, retained.id);
        assert_eq!(service.registry().list_workspaces().unwrap().len(), 1);
    }

    #[gpui::test]
    async fn test_adoption_prefers_the_most_recent_owned_row() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_adoption_prefers_owned_row").await;
        let older = row_on("daemon-a", "a.example", "ade-main-000001", "older", 1_000);
        let newer = row_on("daemon-a", "a.example", "ade-main-000002", "newer", 2_000);
        registry.create_workspace(older).await.unwrap();
        registry.create_workspace(newer.clone()).await.unwrap();
        let remote = Arc::new(
            FakeBackend::new("remote")
                .identified("daemon-a")
                .holding(vec![
                    record("ade-main-000001", "older", "/home/user/main", 1),
                    record("ade-main-000002", "newer", "/home/user/main", 2),
                    record("ade-main-000003", "unopened", "/home/user/main", 0),
                ]),
        );
        let service =
            WorkspaceLifecycleService::with_backend(registry, Arc::new(FakeBackend::new("local")))
                .with_backend_for_host("a.example", remote);

        let (adopted, created) = service
            .adopt_or_create_workspace(
                PathBuf::from("/home/user/main"),
                Some("a.example".to_owned()),
                None,
            )
            .await
            .unwrap();

        assert!(!created);
        assert_eq!(adopted.id, newer.id);
        assert_eq!(service.registry().list_workspaces().unwrap().len(), 2);
    }

    #[gpui::test]
    async fn test_adoption_chooses_the_oldest_discovery_deterministically() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_adoption_oldest_discovery").await;
        let remote = Arc::new(
            FakeBackend::new("remote")
                .identified("daemon-a")
                .holding(vec![
                    record("ade-late", "late", "/home/user/main", 300),
                    record("ade-b", "b", "/home/user/main", 100),
                    record("ade-a", "a", "/home/user/main", 100),
                    record("ade-other", "other", "/home/user/other", 1),
                ]),
        );
        let service =
            WorkspaceLifecycleService::with_backend(registry, Arc::new(FakeBackend::new("local")))
                .with_backend_for_host("a.example", remote);

        let (adopted, created) = service
            .adopt_or_create_workspace(
                PathBuf::from("/home/user/main"),
                Some("a.example".to_owned()),
                None,
            )
            .await
            .unwrap();

        assert!(!created);
        assert_eq!(adopted.terminal_session_id.as_deref(), Some("ade-a"));
    }

    #[gpui::test]
    async fn test_adoption_refreshes_after_the_initial_ensure() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_adoption_refreshes").await;
        let remote = Arc::new(FakeBackend::new("remote").identified("daemon-a"));
        let service =
            WorkspaceLifecycleService::with_backend(registry, Arc::new(FakeBackend::new("local")))
                .with_backend_for_host("a.example", remote.clone());
        assert!(
            service
                .ensure_host_workspaces(Some("a.example"))
                .await
                .unwrap()
                .is_empty()
        );
        remote.hold(record("ade-main-000001", "main", "/home/user/main", 1));

        let (adopted, created) = service
            .adopt_or_create_workspace(
                PathBuf::from("/home/user/main"),
                Some("a.example".to_owned()),
                Some(("canonical-project".to_owned(), "/scope/main".to_owned())),
            )
            .await
            .unwrap();

        assert!(!created);
        assert_eq!(
            adopted.terminal_session_id.as_deref(),
            Some("ade-main-000001")
        );
        assert_eq!(adopted.project_id, "canonical-project");
        assert_eq!(adopted.project_identity.as_deref(), Some("/scope/main"));
    }

    #[gpui::test]
    async fn test_adoption_replaces_a_discovery_that_disappeared() {
        let registry =
            AdeWorkspaceRegistry::open_test_db("test_adoption_replaces_disappeared").await;
        let remote = Arc::new(
            FakeBackend::new("remote")
                .identified("daemon-a")
                .holding(vec![record(
                    "ade-main-000001",
                    "main",
                    "/home/user/main",
                    1,
                )]),
        );
        let service =
            WorkspaceLifecycleService::with_backend(registry, Arc::new(FakeBackend::new("local")))
                .with_backend_for_host("a.example", remote.clone());
        assert_eq!(
            discoveries(
                &service
                    .ensure_host_workspaces(Some("a.example"))
                    .await
                    .unwrap()
            )
            .len(),
            1
        );
        remote.workspaces.lock().unwrap().clear();

        let (created, was_created) = service
            .adopt_or_create_workspace(
                PathBuf::from("/home/user/main"),
                Some("a.example".to_owned()),
                Some(("canonical-project".to_owned(), "/scope/main".to_owned())),
            )
            .await
            .unwrap();

        assert!(was_created);
        assert_ne!(created.daemon_workspace_id(), "ade-main-000001");
        assert_eq!(created.project_id, "canonical-project");
        assert_eq!(created.project_identity.as_deref(), Some("/scope/main"));
    }

    #[gpui::test]
    async fn test_failed_adoption_reattaches_but_never_creates() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_failed_adoption").await;
        let offline = row_on("daemon-a", "a.example", "ade-main-000001", "main", 1_000);
        registry.create_workspace(offline.clone()).await.unwrap();
        let remote =
            Arc::new(FakeBackend::failing("remote", "ssh: no route").identified("daemon-a"));
        let service =
            WorkspaceLifecycleService::with_backend(registry, Arc::new(FakeBackend::new("local")))
                .with_backend_for_host("a.example", remote);

        let (reattached, created) = service
            .adopt_or_create_workspace(
                PathBuf::from("/home/user/main"),
                Some("a.example".to_owned()),
                None,
            )
            .await
            .unwrap();
        assert!(!created);
        assert_eq!(reattached.id, offline.id);

        let error = service
            .adopt_or_create_workspace(
                PathBuf::from("/home/user/other"),
                Some("a.example".to_owned()),
                None,
            )
            .await
            .expect_err("an unreachable daemon must not gain a workspace");
        assert!(error.to_string().contains("refreshing the workspaces"));
        assert_eq!(service.registry().list_workspaces().unwrap().len(), 1);
    }

    #[gpui::test]
    async fn test_failed_adoption_never_crosses_routes_without_an_identity() {
        let registry =
            AdeWorkspaceRegistry::open_test_db("test_failed_adoption_stays_on_route").await;
        let other_host = row_on("daemon-a", "a.example", "ade-main-000001", "main", 1_000);
        registry.create_workspace(other_host.clone()).await.unwrap();
        let unreachable = Arc::new(FakeBackend::failing("remote", "ssh: no route"));
        let service =
            WorkspaceLifecycleService::with_backend(registry, Arc::new(FakeBackend::new("local")))
                .with_backend_for_host("b.example", unreachable);

        service
            .adopt_or_create_workspace(
                PathBuf::from("/home/user/main"),
                Some("b.example".to_owned()),
                None,
            )
            .await
            .expect_err("a failed unidentified route must not reuse another host's row");

        assert_eq!(
            service.registry().list_workspaces().unwrap(),
            vec![other_host]
        );
    }

    #[gpui::test]
    async fn test_concurrent_adoptions_agree_on_one_row(cx: &mut gpui::TestAppContext) {
        let registry = AdeWorkspaceRegistry::open_test_db("test_concurrent_adoptions").await;
        let remote = Arc::new(
            FakeBackend::new("remote")
                .identified("daemon-a")
                .holding(vec![record(
                    "ade-main-000001",
                    "main",
                    "/home/user/main",
                    1,
                )]),
        );
        let service = Arc::new(
            WorkspaceLifecycleService::with_backend(registry, Arc::new(FakeBackend::new("local")))
                .with_backend_for_host("a.example", remote),
        );
        let lock = service.daemon_decision_lock(DaemonKey::Instance("daemon-a".to_owned()));
        let guard = lock.lock().await;
        let tasks: Vec<_> = (0..2)
            .map(|_| {
                let service = service.clone();
                cx.background_spawn(async move {
                    service
                        .adopt_or_create_workspace(
                            PathBuf::from("/home/user/main"),
                            Some("a.example".to_owned()),
                            None,
                        )
                        .await
                })
            })
            .collect();
        cx.run_until_parked();
        assert!(service.registry().list_workspaces().unwrap().is_empty());

        drop(guard);
        let mut resolved = Vec::new();
        for task in tasks {
            resolved.push(task.await.unwrap());
        }
        assert_eq!(resolved[0].0.id, resolved[1].0.id);
        assert!(!resolved[0].1 && !resolved[1].1);
        assert_eq!(service.registry().list_workspaces().unwrap().len(), 1);
    }

    #[gpui::test]
    async fn test_adoption_moves_to_an_identity_learned_by_listing(cx: &mut gpui::TestAppContext) {
        let registry = AdeWorkspaceRegistry::open_test_db("test_adoption_moves_identity").await;
        let remote = Arc::new(
            FakeBackend::new("remote")
                .listing_identities(vec![Some("daemon-a"), Some("daemon-a")])
                .holding(vec![record(
                    "ade-main-000001",
                    "main",
                    "/home/user/main",
                    1,
                )]),
        );
        let service = Arc::new(
            WorkspaceLifecycleService::with_backend(registry, Arc::new(FakeBackend::new("local")))
                .with_backend_for_host("a.example", remote.clone()),
        );
        let lock = service.daemon_decision_lock(DaemonKey::Instance("daemon-a".to_owned()));
        let guard = lock.lock().await;
        let resolving = {
            let service = service.clone();
            cx.background_spawn(async move {
                service
                    .adopt_or_create_workspace(
                        PathBuf::from("/home/user/main"),
                        Some("a.example".to_owned()),
                        None,
                    )
                    .await
            })
        };
        cx.run_until_parked();
        assert_eq!(
            remote.calls(),
            vec!["list_workspaces"],
            "the first listing may learn the identity, but no decision may pass its lock"
        );
        assert!(service.registry().list_workspaces().unwrap().is_empty());

        drop(guard);
        let (adopted, created) = resolving.await.unwrap();
        assert!(!created);
        assert_eq!(adopted.daemon_id.as_deref(), Some("daemon-a"));
    }

    #[test]
    fn test_a_healthy_alias_wins_the_discovery_snapshot() {
        let outcome = |host: &str, live| HostOutcome {
            host: Some(host.to_owned()),
            daemon: DaemonKey::Instance("daemon-a".to_owned()),
            live,
            held: Some(Vec::new()),
        };
        let mut outcomes = [
            outcome("unhealthy", None),
            outcome("healthy", Some(HashSet::new())),
        ];
        outcomes.sort_by_key(|outcome| (outcome.held.is_none(), outcome.live.is_none()));
        assert_eq!(outcomes[0].host.as_deref(), Some("healthy"));
    }

    /// The connect decision reads the **daemon**, not the registry: a client
    /// whose registry is empty still has to see what the host already holds, or
    /// it makes a second workspace beside the first (found 2026-08-05).
    ///
    /// It sees them as *discoveries*. A listing is not usage, so a pass that
    /// only looked writes nothing at all.
    #[gpui::test]
    async fn test_ensure_host_discovers_what_the_daemon_holds() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_ensure_host_discovers").await;
        let local = Arc::new(FakeBackend::new("local"));
        let host = Arc::new(
            FakeBackend::new("dev-box")
                .identified("daemon-dev-box")
                .holding(vec![
                    // Machine-named, so it reads as its checkout instead.
                    record(
                        "ade-testproj-2de8b3",
                        "ade-testproj-2de8b3",
                        "/home/user/testproj",
                        1_700_000_000,
                    ),
                    // Renamed by a person: discovery must not throw that away.
                    record(
                        "ade-scratch-0f1e2d",
                        "Investigation: vector DB",
                        "/home/user/scratch",
                        1_700_000_100,
                    ),
                ]),
        );
        let service = WorkspaceLifecycleService::with_backend(registry, local.clone())
            .with_backend_for_host("dev-box", host.clone());

        let listed = service
            .ensure_host_workspaces(Some("dev-box"))
            .await
            .unwrap();
        assert_eq!(listed.len(), 2);
        let found = |wire: &str| {
            listed
                .iter()
                .find(|entry| entry.wire_id() == wire)
                .cloned()
                .expect("the daemon's workspace is on the list")
        };

        let testproj = found("ade-testproj-2de8b3");
        assert!(matches!(testproj, WorkspaceEntry::Discovered { .. }));
        assert_eq!(testproj.name(), "testproj");
        assert_eq!(testproj.project_id(), "testproj");
        assert_eq!(
            testproj.repository_path(),
            PathBuf::from("/home/user/testproj")
        );
        assert_eq!(testproj.remote_host(), Some("dev-box"));

        let scratch = found("ade-scratch-0f1e2d");
        assert_eq!(scratch.name(), "Investigation: vector DB");
        assert_eq!(scratch.project_id(), "scratch");

        // The whole point: looking at a host records nothing about it.
        assert!(service.registry().list_workspaces().unwrap().is_empty());
        assert!(
            service
                .registry()
                .unconfirmed_workspaces()
                .unwrap()
                .is_empty()
        );

        // And this machine's backend was never asked about another host's.
        assert!(local.calls().is_empty(), "{:?}", local.calls());
    }

    #[gpui::test]
    async fn test_legacy_discoveries_share_resolved_project_scope_and_cache_it() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_legacy_discovery_scope").await;
        let project_identity_path = PathBuf::from("/repos/viral-studio");
        let project_identity =
            util::path_list::PathList::new(std::slice::from_ref(&project_identity_path))
                .serialize()
                .paths;
        let host = Arc::new(
            FakeBackend::new("dev-box")
                .identified("daemon-dev-box")
                .holding(vec![
                    record(
                        "ade-seedance2-5-2de8b3",
                        "ade-seedance2-5-2de8b3",
                        "/worktrees/viral-studio/seedance2-5",
                        1_700_000_000,
                    ),
                    record(
                        "ade-yookassa-0f1e2d",
                        "ade-yookassa-0f1e2d",
                        "/worktrees/viral-studio/yookassa",
                        1_700_000_100,
                    ),
                ])
                .resolving_repository("/worktrees/viral-studio/seedance2-5", project_identity_path),
        );
        let service = WorkspaceLifecycleService::with_backend(
            registry.clone(),
            Arc::new(FakeBackend::new("local")),
        )
        .with_backend_for_host("dev-box", host.clone());

        for _ in 0..2 {
            let listed = service
                .ensure_host_workspaces(Some("dev-box"))
                .await
                .unwrap();
            assert_eq!(discoveries(&listed).len(), 2);
            assert_eq!(
                listed
                    .iter()
                    .map(WorkspaceEntry::project_identity)
                    .collect::<HashSet<_>>(),
                HashSet::from([project_identity.clone()])
            );
            assert!(
                listed
                    .iter()
                    .all(|entry| entry.project_id() == "viral-studio")
            );
            assert!(registry.list_workspaces().unwrap().is_empty());
        }

        assert_eq!(
            host.calls()
                .iter()
                .filter(|call| call.starts_with("resolve:"))
                .count(),
            2,
            "each legacy root is resolved only on the first listing"
        );
        assert_eq!(host.project_scope_updates().len(), 2);

        let confirmed = service
            .confirm_discovered(Some("dev-box"), "ade-seedance2-5-2de8b3")
            .await
            .unwrap();
        assert_eq!(confirmed.project_id, "viral-studio");
        assert_eq!(
            confirmed.project_identity.as_deref(),
            Some(project_identity.as_str())
        );
        assert_eq!(
            host.calls()
                .iter()
                .filter(|call| call.starts_with("resolve:"))
                .count(),
            2,
            "confirmation reuses the enriched discovery"
        );
        assert_eq!(host.project_scope_updates().len(), 2);
    }

    #[gpui::test]
    async fn test_a_failed_legacy_resolution_can_heal_on_the_next_listing() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_legacy_scope_retry").await;
        let host = Arc::new(
            FakeBackend::new("dev-box")
                .identified("daemon-dev-box")
                .holding(vec![record(
                    "ade-seedance2-5-2de8b3",
                    "ade-seedance2-5-2de8b3",
                    "/worktrees/viral-studio/seedance2-5",
                    1_700_000_000,
                )]),
        );
        let service =
            WorkspaceLifecycleService::with_backend(registry, Arc::new(FakeBackend::new("local")))
                .with_backend_for_host("dev-box", host.clone());

        let first = service
            .ensure_host_workspaces(Some("dev-box"))
            .await
            .unwrap();
        assert_eq!(
            first[0].project_identity(),
            "/worktrees/viral-studio/seedance2-5"
        );

        host.set_repository_resolution(
            "/worktrees/viral-studio/seedance2-5",
            "/repos/viral-studio",
        );
        let second = service
            .ensure_host_workspaces(Some("dev-box"))
            .await
            .unwrap();
        assert_eq!(second[0].project_identity(), "/repos/viral-studio");
        service
            .ensure_host_workspaces(Some("dev-box"))
            .await
            .unwrap();
        assert_eq!(
            host.calls()
                .iter()
                .filter(|call| call.starts_with("resolve:"))
                .count(),
            2,
            "a failure retries once, then the successful result is cached"
        );
    }

    #[gpui::test]
    async fn test_a_failed_daemon_scope_write_retries_without_resolving_git_again() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_legacy_scope_write_retry").await;
        let host = Arc::new(
            FakeBackend::new("dev-box")
                .identified("daemon-dev-box")
                .holding(vec![record(
                    "ade-seedance2-5-2de8b3",
                    "ade-seedance2-5-2de8b3",
                    "/worktrees/viral-studio/seedance2-5",
                    1_700_000_000,
                )])
                .resolving_repository("/worktrees/viral-studio/seedance2-5", "/repos/viral-studio"),
        );
        host.fail_next_project_scope_update("connection reset");
        let service =
            WorkspaceLifecycleService::with_backend(registry, Arc::new(FakeBackend::new("local")))
                .with_backend_for_host("dev-box", host.clone());

        for _ in 0..3 {
            let listed = service
                .ensure_host_workspaces(Some("dev-box"))
                .await
                .unwrap();
            assert_eq!(listed[0].project_identity(), "/repos/viral-studio");
        }
        assert_eq!(
            host.calls()
                .iter()
                .filter(|call| call.starts_with("resolve:"))
                .count(),
            1,
            "the successful Git result remains cached"
        );
        assert_eq!(
            host.project_scope_updates().len(),
            2,
            "the transient write is retried once and old-daemon false is then cached"
        );
    }

    /// Opening is what records a workspace, and records it **once**: the row
    /// carries the daemon's identity and the record's wire id, a second open
    /// answers with the same row, and the record stops being offered as a
    /// discovery.
    #[gpui::test]
    async fn test_confirming_a_discovery_records_exactly_one_row() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_confirming_records_one_row").await;
        let host = Arc::new(
            FakeBackend::new("dev-box")
                .identified("daemon-dev-box")
                .holding(vec![record(
                    "ade-testproj-2de8b3",
                    "ade-testproj-2de8b3",
                    "/home/user/testproj",
                    1_700_000_000,
                )]),
        );
        let service =
            WorkspaceLifecycleService::with_backend(registry, Arc::new(FakeBackend::new("local")))
                .with_backend_for_host("dev-box", host);

        let confirmed = service
            .confirm_discovered(Some("dev-box"), "ade-testproj-2de8b3")
            .await
            .unwrap();
        assert_eq!(
            confirmed.terminal_session_id.as_deref(),
            Some("ade-testproj-2de8b3")
        );
        assert_eq!(confirmed.daemon_id.as_deref(), Some("daemon-dev-box"));
        assert_eq!(confirmed.remote_host.as_deref(), Some("dev-box"));
        assert_eq!(confirmed.name, "testproj");
        // The daemon's clock for when the workspace began, this client's for
        // when it started using it.
        assert_eq!(confirmed.created_at.unix_timestamp(), 1_700_000_000);
        assert!(confirmed.last_opened_at > confirmed.created_at);

        let rows = service.registry().list_workspaces().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, confirmed.id);

        let again = service
            .confirm_discovered(Some("dev-box"), "ade-testproj-2de8b3")
            .await
            .unwrap();
        assert_eq!(
            again.id, confirmed.id,
            "clicking twice is not two workspaces"
        );
        assert_eq!(service.registry().list_workspaces().unwrap().len(), 1);

        let listed = service
            .ensure_host_workspaces(Some("dev-box"))
            .await
            .unwrap();
        assert!(discoveries(&listed).is_empty());
        assert_eq!(listed.len(), 1);
    }

    /// A quarantined row is **promoted in place**. Its uuid, its branch and its
    /// history are metadata no listing can re-derive, and replacing the row
    /// would take all three with it.
    #[gpui::test]
    async fn test_confirming_promotes_a_quarantined_row_in_place() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_confirming_promotes").await;
        let mut quarantined = AdeWorkspace {
            terminal_session_id: Some("ade-main-2de8b3".to_owned()),
            remote_host: Some("old-alias".to_owned()),
            branch: Some("feature/x".to_owned()),
            ..AdeWorkspace::new("Renamed by hand", "project-a", "/home/user/main")
        };
        quarantined.created_at = OffsetDateTime::from_unix_timestamp(1_600_000_000).unwrap();
        registry
            .create_workspace(quarantined.clone())
            .await
            .unwrap();
        registry
            .quarantine_workspace(quarantined.id.clone())
            .await
            .unwrap();
        assert!(registry.list_workspaces().unwrap().is_empty());

        let host = Arc::new(
            FakeBackend::new("dev-box")
                .identified("daemon-dev-box")
                .holding(vec![record(
                    "ade-main-2de8b3",
                    "ade-main-2de8b3",
                    "/home/user/main",
                    1_600_000_000,
                )]),
        );
        let service =
            WorkspaceLifecycleService::with_backend(registry, Arc::new(FakeBackend::new("local")))
                .with_backend_for_host("old-alias", host);

        let confirmed = service
            .confirm_discovered(Some("old-alias"), "ade-main-2de8b3")
            .await
            .unwrap();
        assert_eq!(
            confirmed.id, quarantined.id,
            "the row is promoted, not replaced"
        );
        assert_eq!(confirmed.branch.as_deref(), Some("feature/x"));
        assert_eq!(confirmed.name, "Renamed by hand");
        assert_eq!(confirmed.created_at, quarantined.created_at);
        assert_eq!(confirmed.remote_host.as_deref(), Some("old-alias"));
        assert_eq!(confirmed.daemon_id.as_deref(), Some("daemon-dev-box"));
        assert!(confirmed.last_opened_at >= quarantined.last_opened_at);

        let rows = service.registry().list_workspaces().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, quarantined.id);
        assert!(
            service
                .registry()
                .unconfirmed_workspaces()
                .unwrap()
                .is_empty()
        );
    }

    /// A record removed between the listing the sidebar drew and the click on
    /// it is [`WorkspaceGone`], not a row for a workspace that is not there —
    /// and it leaves the snapshot, so the next pass does not show it either.
    #[gpui::test]
    async fn test_a_record_gone_before_the_open_is_not_recorded() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_gone_before_open").await;
        let local = Arc::new(FakeBackend::new("local").holding(vec![record(
            "ade-testproj-2de8b3",
            "ade-testproj-2de8b3",
            "/home/user/testproj",
            1_700_000_000,
        )]));
        let service = WorkspaceLifecycleService::with_backend(registry, local.clone());

        let listed = service.ensure_host_workspaces(None).await.unwrap();
        assert_eq!(discoveries(&listed).len(), 1);

        // Another client kills it while this one is still showing the row.
        local.kill_workspace("ade-testproj-2de8b3", None).unwrap();

        let error = service
            .confirm_discovered(None, "ade-testproj-2de8b3")
            .await
            .expect_err("a record that is gone cannot be confirmed");
        assert!(error.downcast_ref::<WorkspaceGone>().is_some(), "{error:#}");
        assert!(service.registry().list_workspaces().unwrap().is_empty());
        assert!(
            service
                .registry()
                .unconfirmed_workspaces()
                .unwrap()
                .is_empty()
        );

        let reconciled = service.reconcile_all().await.unwrap();
        assert!(discoveries(&reconciled.entries).is_empty());
    }

    /// A host that cannot be reached keeps showing what it last held, as
    /// `Unknown`. Blinking a running workspace off the sidebar because one call
    /// timed out would be a lie about the host, and one the user cannot check.
    #[gpui::test]
    async fn test_a_failed_listing_keeps_its_discoveries_as_unknown() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_failed_listing_keeps").await;
        let local = Arc::new(FakeBackend::new("local").holding(vec![record(
            "ade-testproj-2de8b3",
            "ade-testproj-2de8b3",
            "/home/user/testproj",
            1_700_000_000,
        )]));
        let service = WorkspaceLifecycleService::with_backend(registry, local.clone());

        let reconciled = service.reconcile_all().await.unwrap();
        let found = discoveries(&reconciled.entries);
        assert_eq!(found.len(), 1);
        // The fake holds no session under that id, so it reads as gone.
        assert_eq!(found[0].state(), SessionState::Dead);

        local.goes_down("ssh: connect: no route to host");
        let reconciled = service.reconcile_all().await.unwrap();
        let found = discoveries(&reconciled.entries);
        assert_eq!(
            found.len(),
            1,
            "a transient failure must not empty the sidebar"
        );
        assert_eq!(found[0].wire_id(), "ade-testproj-2de8b3");
        assert_eq!(
            found[0].state(),
            SessionState::Unknown,
            "nothing is claimed about a host that could not be asked"
        );
        assert_eq!(
            reconciled.host_errors.len(),
            1,
            "{:?}",
            reconciled.host_errors
        );

        // And a host that will not answer is still no reason to write a row.
        assert!(service.registry().list_workspaces().unwrap().is_empty());
    }

    /// **One daemon, however its host is spelled.** A destination typed two ways
    /// gets a backend each, but its records must appear once, and a row
    /// confirmed through one spelling must be reused — not discovered again —
    /// through the other.
    #[gpui::test]
    async fn test_aliases_of_one_daemon_do_not_duplicate_its_record() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_aliases_do_not_duplicate").await;
        let local = Arc::new(FakeBackend::new("local"));
        let remote = Arc::new(
            FakeBackend::new("remote")
                .identified("daemon-1")
                .holding(vec![record(
                    "ade-viral-studio-2de8b3",
                    "ade-viral-studio-2de8b3",
                    "/home/user/Code/viral-studio",
                    1_700_000_000,
                )]),
        );
        let service = WorkspaceLifecycleService::with_backend(registry, local)
            .with_backend_for_host("100.78.83.67", remote.clone())
            .with_backend_for_host("fevm1.local", remote);

        let reconciled = service.reconcile_all().await.unwrap();
        assert_eq!(
            discoveries(&reconciled.entries).len(),
            1,
            "one daemon reached twice holds one workspace, not two"
        );

        let original = service
            .confirm_discovered(Some("100.78.83.67"), "ade-viral-studio-2de8b3")
            .await
            .unwrap();
        assert_eq!(original.daemon_id.as_deref(), Some("daemon-1"));

        let reconciled = service.reconcile_all().await.unwrap();
        assert!(discoveries(&reconciled.entries).is_empty());
        assert_eq!(reconciled.entries.len(), 1);

        // The other spelling reaches the same daemon, so it finds the row by
        // identity rather than recording a second one.
        let again = service
            .confirm_discovered(Some("fevm1.local"), "ade-viral-studio-2de8b3")
            .await
            .unwrap();
        assert_eq!(again.id, original.id);
        assert_eq!(
            again.terminal_session_id, original.terminal_session_id,
            "its session history moves with it"
        );
        assert_eq!(service.registry().list_workspaces().unwrap().len(), 1);
    }

    #[gpui::test]
    async fn test_a_working_alias_wins_over_a_failed_alias_of_the_same_daemon() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_working_alias_wins").await;
        let record = record(
            "ade-viral-studio-2de8b3",
            "ade-viral-studio-2de8b3",
            "/home/user/Code/viral-studio",
            1_700_000_000,
        );
        let failed = Arc::new(
            FakeBackend::failing("failed", "ssh: connect: timed out")
                .identified("daemon-1")
                .holding(vec![record.clone()]),
        );
        let healthy = Arc::new(
            FakeBackend::new("healthy")
                .identified("daemon-1")
                .holding(vec![record]),
        );
        healthy
            .create(
                &SessionSpec::new("ade-viral-studio-2de8b3", "/home/user/Code/viral-studio"),
                None,
            )
            .unwrap();
        let service =
            WorkspaceLifecycleService::with_backend(registry, Arc::new(FakeBackend::new("local")))
                .with_backend_for_host("a-failed", failed)
                .with_backend_for_host("z-healthy", healthy);

        let reconciled = service.reconcile_all().await.unwrap();
        let found = discoveries(&reconciled.entries);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].remote_host(), Some("z-healthy"));
        assert_eq!(found[0].state(), SessionState::Alive);
    }

    /// A daemon too old to identify itself is scoped to its exact route.
    #[gpui::test]
    async fn test_an_identityless_daemons_route_is_its_identity() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_identityless_route").await;
        let local = Arc::new(FakeBackend::new("local"));
        let remote = Arc::new(FakeBackend::new("remote").holding(vec![record(
            "ade-viral-studio-2de8b3",
            "ade-viral-studio-2de8b3",
            "/home/user/Code/viral-studio",
            1_700_000_000,
        )]));
        let service = WorkspaceLifecycleService::with_backend(registry, local)
            .with_backend_for_host("100.78.83.67", remote.clone())
            .with_backend_for_host("fevm1.local", remote);

        let original = service
            .confirm_discovered(Some("100.78.83.67"), "ade-viral-studio-2de8b3")
            .await
            .unwrap();

        let listed = service
            .ensure_host_workspaces(Some("fevm1.local"))
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(discoveries(&listed).len(), 1);
        let stored = service
            .registry()
            .get_workspace(original.id.clone())
            .unwrap()
            .unwrap();
        assert_eq!(stored.remote_host.as_deref(), Some("100.78.83.67"));

        let confirmed = service
            .confirm_discovered(Some("fevm1.local"), "ade-viral-studio-2de8b3")
            .await
            .unwrap();
        assert_ne!(confirmed.id, original.id);
        assert_eq!(confirmed.remote_host.as_deref(), Some("fevm1.local"));
        assert_eq!(service.registry().list_workspaces().unwrap().len(), 2);
    }

    /// **Every operation on a persisted row names the daemon that row belongs
    /// to.** Two daemons can hold the same workspace id, so an unfenced kill,
    /// attach or probe is one that a stranger may answer.
    #[gpui::test]
    async fn test_operations_on_a_persisted_row_carry_its_daemon() {
        let registry =
            AdeWorkspaceRegistry::open_test_db("test_operations_carry_their_daemon").await;
        let backend = Arc::new(FakeBackend::new("a-box").identified("daemon-a"));
        let service =
            WorkspaceLifecycleService::with_backend(registry, Arc::new(FakeBackend::new("local")))
                .with_backend_for_host("a.example", backend.clone());

        let owned = AdeWorkspace {
            terminal_session_id: Some("ade-main-2de8b3".to_owned()),
            daemon_id: Some("daemon-a".to_owned()),
            remote_host: Some("a.example".to_owned()),
            ..AdeWorkspace::new("main", "project-a", "/home/user/main")
        };
        service
            .registry()
            .create_workspace(owned.clone())
            .await
            .unwrap();

        let mut probed = owned.clone();
        service.probe(&mut probed).await.unwrap();
        service.attach_command(&owned).unwrap();
        service.kill_workspace(&owned.id).await.unwrap();

        assert_eq!(
            backend.calls(),
            vec![
                "exists:ade-main-2de8b3|daemon-a",
                "attach:ade-main-2de8b3|daemon-a",
                "kill_workspace:ade-main-2de8b3|daemon-a",
            ]
        );

        // A row from before identities were recorded stays permissive: there is
        // nothing to fence it to, and refusing it would strand it.
        let legacy = AdeWorkspace {
            terminal_session_id: Some("ade-old-000001".to_owned()),
            remote_host: Some("a.example".to_owned()),
            ..AdeWorkspace::new("old", "project-b", "/home/user/old")
        };
        service
            .registry()
            .create_workspace(legacy.clone())
            .await
            .unwrap();
        service.attach_command(&legacy).unwrap();
        assert!(
            backend
                .calls()
                .contains(&"attach:ade-old-000001".to_owned()),
            "{:?}",
            backend.calls()
        );
    }

    /// A listing from a daemon a row is not fenced to says nothing about that
    /// row — not even that its session died.
    #[gpui::test]
    async fn test_another_daemons_listing_cannot_probe_a_fenced_row() {
        let registry =
            AdeWorkspaceRegistry::open_test_db("test_another_daemons_listing_cannot_probe").await;
        let replacement = Arc::new(FakeBackend::new("a-box").identified("daemon-b"));
        let service =
            WorkspaceLifecycleService::with_backend(registry, Arc::new(FakeBackend::new("local")))
                .with_backend_for_host("a.example", replacement);

        let owned = AdeWorkspace {
            terminal_session_id: Some("ade-main-2de8b3".to_owned()),
            daemon_id: Some("daemon-a".to_owned()),
            remote_host: Some("a.example".to_owned()),
            status: WorkspaceStatus::Running,
            ..AdeWorkspace::new("main", "project-a", "/home/user/main")
        };
        service
            .registry()
            .create_workspace(owned.clone())
            .await
            .unwrap();

        let reconciled = service.reconcile_all().await.unwrap();
        let (row, state) = reconciled
            .entries
            .iter()
            .filter_map(WorkspaceEntry::persisted)
            .find(|(row, _)| row.id == owned.id)
            .expect("the row is reconciled, not dropped");
        assert_eq!(state, SessionState::Unknown);
        assert_eq!(
            row.status,
            WorkspaceStatus::Running,
            "a stranger's listing must not move the recorded status"
        );
    }

    /// A persisted daemon identity is **exclusive**: another daemon holding a
    /// record with the same wire id may not claim this client's row, and its own
    /// record shows up as the discovery it is.
    #[gpui::test]
    async fn test_a_persisted_daemon_id_identifies_and_owns_its_row() {
        let registry =
            AdeWorkspaceRegistry::open_test_db("test_persisted_daemon_id_owns_its_row").await;
        let local = Arc::new(FakeBackend::new("local"));

        let cold_started = AdeWorkspace {
            terminal_session_id: Some("ade-main-2de8b3".to_owned()),
            daemon_id: Some("daemon-a".to_owned()),
            remote_host: None,
            ..AdeWorkspace::new("main", "project-a", "/home/user/main")
        };
        registry
            .create_workspace(cold_started.clone())
            .await
            .unwrap();

        let held = || {
            vec![record(
                "ade-main-2de8b3",
                "ade-main-2de8b3",
                "/home/user/main",
                1_700_000_000,
            )]
        };
        let daemon_a = Arc::new(
            FakeBackend::new("daemon-a-box")
                .identified("daemon-a")
                .holding(held()),
        );
        let daemon_b = Arc::new(
            FakeBackend::new("daemon-b-box")
                .identified("daemon-b")
                .holding(held()),
        );
        let service = WorkspaceLifecycleService::with_backend(registry, local)
            .with_backend_for_host("a.example", daemon_a);

        let listed = service
            .ensure_host_workspaces(Some("a.example"))
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        let (row, _) = listed[0].persisted().expect("daemon A's own row");
        assert_eq!(row.id, cold_started.id);
        assert_eq!(row.remote_host.as_deref(), Some("a.example"));

        // The same alias now reaches another daemon with a colliding record.
        service
            .backends
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(Some("a.example".to_owned()), daemon_b);
        let listed = service
            .ensure_host_workspaces(Some("a.example"))
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed.iter().all(|entry| entry.persisted().is_none()));
        let a_row = service
            .registry()
            .get_workspace(cold_started.id.clone())
            .unwrap()
            .expect("daemon A's row survives untouched");
        assert_eq!(
            a_row.remote_host.as_deref(),
            Some("a.example"),
            "daemon B must not have reclaimed it"
        );
        assert_eq!(a_row.daemon_id.as_deref(), Some("daemon-a"));
        assert_eq!(
            discoveries(&listed).len(),
            1,
            "daemon B's colliding record is a discovery, not a row it stole"
        );
        assert_eq!(
            service.registry().list_workspaces().unwrap().len(),
            1,
            "and a listing writes nothing, colliding or not"
        );
    }

    #[gpui::test]
    async fn test_a_persisted_daemon_id_wins_over_a_newer_legacy_match() {
        let registry = AdeWorkspaceRegistry::open_test_db(
            "test_persisted_daemon_id_wins_over_newer_legacy_match",
        )
        .await;
        let session_id = "ade-main-2de8b3";

        let mut exact = AdeWorkspace {
            terminal_session_id: Some(session_id.to_owned()),
            daemon_id: Some("daemon-a".to_owned()),
            remote_host: Some("old-alias".to_owned()),
            ..AdeWorkspace::new("main", "project-a", "/home/user/main")
        };
        exact.last_opened_at = OffsetDateTime::from_unix_timestamp(1_000).unwrap();
        exact.project_identity = Some("/home/user/main".to_owned());
        exact.project_scope_rev = 2;
        registry.create_workspace(exact.clone()).await.unwrap();

        let mut legacy = AdeWorkspace {
            terminal_session_id: Some(session_id.to_owned()),
            remote_host: Some("new-alias".to_owned()),
            ..AdeWorkspace::new("legacy", "project-a", "/home/user/main")
        };
        legacy.last_opened_at = OffsetDateTime::from_unix_timestamp(2_000).unwrap();
        legacy.project_id = "wrong-project".to_owned();
        legacy.project_identity = Some("/home/user/wrong-project".to_owned());
        legacy.project_scope_rev = 3;
        registry.create_workspace(legacy.clone()).await.unwrap();

        let backend = Arc::new(
            FakeBackend::new("daemon-a")
                .identified("daemon-a")
                .holding(vec![record(
                    session_id,
                    session_id,
                    "/home/user/main",
                    1_700_000_000,
                )]),
        );
        let service =
            WorkspaceLifecycleService::with_backend(registry, Arc::new(FakeBackend::new("local")))
                .with_backend_for_host("new-alias", backend);

        let listed = service
            .ensure_host_workspaces(Some("new-alias"))
            .await
            .unwrap();
        let rows: Vec<&AdeWorkspace> = listed
            .iter()
            .filter_map(WorkspaceEntry::persisted)
            .map(|(workspace, _)| workspace)
            .collect();
        let exact = rows
            .iter()
            .find(|workspace| workspace.id == exact.id)
            .expect("the identified row survives");
        assert_eq!(exact.remote_host.as_deref(), Some("new-alias"));
        assert_eq!(exact.daemon_id.as_deref(), Some("daemon-a"));
        assert_eq!(exact.project_identity.as_deref(), Some("/home/user/main"));
        let legacy = rows
            .iter()
            .find(|workspace| workspace.id == legacy.id)
            .expect("the legacy row is not rebound");
        assert_eq!(legacy.project_id, "wrong-project");
        assert!(legacy.daemon_id.is_none());
    }

    /// Two windows opening the same record at once get the **same** row: the
    /// decision is taken under the daemon's lock, and the loser reads back the
    /// owner the winner recorded rather than replacing it.
    #[gpui::test]
    async fn test_concurrent_confirmations_agree_on_one_row(cx: &mut gpui::TestAppContext) {
        let registry = AdeWorkspaceRegistry::open_test_db("test_concurrent_confirmations").await;
        let local = Arc::new(FakeBackend::new("local"));
        let remote = Arc::new(
            FakeBackend::new("remote")
                .identified("daemon-1")
                .holding(vec![record(
                    "ade-viral-studio-2de8b3",
                    "ade-viral-studio-2de8b3",
                    "/home/user/Code/viral-studio",
                    1_700_000_000,
                )]),
        );
        let service = Arc::new(
            WorkspaceLifecycleService::with_backend(registry, local)
                .with_backend_for_host("100.78.83.67", remote.clone())
                .with_backend_for_host("fevm1.local", remote.clone()),
        );

        let lock = service.daemon_decision_lock(DaemonKey::Instance("daemon-1".to_owned()));
        let guard = lock.lock().await;

        let by_ip = {
            let service = service.clone();
            cx.background_spawn(async move {
                service
                    .confirm_discovered(Some("100.78.83.67"), "ade-viral-studio-2de8b3")
                    .await
            })
        };
        let by_name = {
            let service = service.clone();
            cx.background_spawn(async move {
                service
                    .confirm_discovered(Some("fevm1.local"), "ade-viral-studio-2de8b3")
                    .await
            })
        };
        cx.run_until_parked();

        assert!(
            remote.calls().is_empty(),
            "the listing a decision acts on is taken under the lock, not before it: {:?}",
            remote.calls()
        );
        assert_eq!(service.registry().list_workspaces().unwrap().len(), 0);

        drop(guard);
        let (by_ip, by_name) = (by_ip.await.unwrap(), by_name.await.unwrap());

        assert_eq!(
            by_ip.id, by_name.id,
            "both callers get the daemon's one row"
        );
        assert_eq!(
            service.registry().list_workspaces().unwrap().len(),
            1,
            "one daemon record must own exactly one row, however many callers opened it at once"
        );
    }

    #[gpui::test]
    async fn test_discovery_refreshes_a_listing_after_taking_the_daemon_lock() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_discovery_refreshes_listing").await;
        let remote = Arc::new(
            FakeBackend::new("remote")
                .identified("daemon-a")
                .holding(vec![BackendWorkspace {
                    id: "ade-stale-000001".to_owned(),
                    name: "stale".to_owned(),
                    project_id: None,
                    project_identity: None,
                    project_root: "/repos/stale".to_owned(),
                    project_scope_rev: 0,
                    created_at: 1,
                }]),
        );
        remote.replace_workspaces_after_next_list(Vec::new());
        let service =
            WorkspaceLifecycleService::with_backend(registry, Arc::new(FakeBackend::new("local")))
                .with_backend_for_host("remote", remote.clone());

        assert!(
            service
                .ensure_host_workspaces(Some("remote"))
                .await
                .unwrap()
                .is_empty(),
            "a workspace killed before the locked refresh must not be adopted"
        );
        assert_eq!(
            remote
                .calls()
                .iter()
                .filter(|call| *call == "list_workspaces")
                .count(),
            2
        );
    }

    #[gpui::test]
    async fn test_project_scope_update_keeps_dead_row_recreatable_when_record_is_missing() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_scope_dead_missing_record").await;
        let mut row = AdeWorkspace::new(
            "seedance2-5",
            "seedance2-5",
            "/worktrees/viral-studio/seedance2-5",
        );
        row.terminal_session_id = Some("ade-seedance2-5-000001".to_owned());
        registry.create_workspace(row.clone()).await.unwrap();
        let service = WorkspaceLifecycleService::with_backend(
            registry.clone(),
            Arc::new(FakeBackend::new("local")),
        );

        let updated = service
            .update_workspace_project_scope(&row.id, "viral-studio", "/repos/viral-studio")
            .await
            .unwrap();
        assert_eq!(updated.project_id, "viral-studio");
        assert_eq!(
            updated.project_identity.as_deref(),
            Some("/repos/viral-studio")
        );
        assert_eq!(
            registry
                .get_workspace(row.id)
                .unwrap()
                .unwrap()
                .project_identity
                .as_deref(),
            Some("/repos/viral-studio")
        );
    }

    #[gpui::test]
    async fn test_repository_scope_update_moves_root_and_identity_together() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_repository_scope_update").await;
        let mut row = AdeWorkspace::new("feature", "project-a", "/repos/project-a-old");
        row.daemon_id = Some("daemon-a".to_owned());
        row.terminal_session_id = Some("ade-project-a-000001".to_owned());
        registry.create_workspace(row.clone()).await.unwrap();
        let mut sibling = AdeWorkspace::new("other", "project-a", "/repos/project-a-old");
        sibling.daemon_id = Some("daemon-a".to_owned());
        sibling.terminal_session_id = Some("ade-project-a-000002".to_owned());
        registry.create_workspace(sibling.clone()).await.unwrap();
        let backend = Arc::new(
            FakeBackend::new("local")
                .identified("daemon-a")
                .holding(vec![
                    record("ade-project-a-000001", "feature", "/repos/project-a-old", 1),
                    record("ade-project-a-000002", "other", "/repos/project-a-old", 1),
                ]),
        );
        let service = WorkspaceLifecycleService::with_backend(registry.clone(), backend.clone());

        let updated = service
            .update_workspace_repository_scope(
                &row.id,
                PathBuf::from("/repos/project-a-new"),
                "project-a-new",
                "/repos/project-a-main",
            )
            .await
            .unwrap();
        assert_eq!(updated.repository_path, Path::new("/repos/project-a-new"));
        assert_eq!(updated.project_scope_rev, 1);
        assert_eq!(updated.project_id, "project-a-new");
        assert_eq!(
            updated.project_identity.as_deref(),
            Some("/repos/project-a-main")
        );
        assert_eq!(registry.get_workspace(row.id).unwrap(), Some(updated));
        let stored_sibling = registry.get_workspace(sibling.id).unwrap().unwrap();
        assert_eq!(
            stored_sibling.repository_path,
            Path::new("/repos/project-a-new")
        );
        assert_eq!(stored_sibling.project_scope_rev, 1);
        assert_eq!(stored_sibling.project_id, "project-a-new");
        assert_eq!(
            stored_sibling.project_identity.as_deref(),
            Some("/repos/project-a-main")
        );
        assert_eq!(backend.project_scope_updates().len(), 2);
        assert_eq!(
            backend.project_scope_updates()[0].3.as_deref(),
            Some("/repos/project-a-new")
        );
    }

    #[gpui::test]
    async fn test_old_daemon_discovery_gets_a_canonical_local_scope() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_old_daemon_discovery_scope").await;
        let remote = Arc::new(
            FakeBackend::new("remote")
                .identified("daemon-a")
                .holding(vec![BackendWorkspace {
                    id: "ade-seedance2-5-000001".to_owned(),
                    name: "seedance2-5".to_owned(),
                    project_id: None,
                    project_identity: None,
                    project_root: "/worktrees/viral-studio/seedance2-5".to_owned(),
                    project_scope_rev: 0,
                    created_at: 1,
                }]),
        );
        let service = WorkspaceLifecycleService::with_backend(
            registry.clone(),
            Arc::new(FakeBackend::new("local")),
        )
        .with_backend_for_host("remote", remote);

        assert_eq!(
            service
                .update_discovered_workspace_project_scope(
                    Some("remote"),
                    "ade-seedance2-5-000001",
                    "viral-studio",
                    "/repos/viral-studio",
                )
                .await
                .unwrap(),
            Some(false),
            "the old backend cannot persist project metadata"
        );
        let rows = registry.list_workspaces().unwrap();
        assert_eq!(rows.len(), 1, "the discovered record is adopted once");
        assert_eq!(rows[0].project_id, "viral-studio");
        assert_eq!(
            rows[0].project_identity.as_deref(),
            Some("/repos/viral-studio")
        );
        assert_eq!(
            rows[0].terminal_session_id.as_deref(),
            Some("ade-seedance2-5-000001")
        );
    }

    #[gpui::test]
    async fn test_reconciliation_hydrates_project_scope_from_daemon() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_scope_hydrates_from_daemon").await;
        let mut row = AdeWorkspace::new(
            "seedance2-5",
            "seedance2-5",
            "/worktrees/viral-studio/seedance2-5",
        );
        row.remote_host = Some("remote".to_owned());
        row.daemon_id = Some("daemon-a".to_owned());
        row.terminal_session_id = Some("ade-seedance2-5-000001".to_owned());
        registry.create_workspace(row.clone()).await.unwrap();
        let remote = Arc::new(
            FakeBackend::new("remote")
                .identified("daemon-a")
                .holding(vec![BackendWorkspace {
                    id: "ade-seedance2-5-000001".to_owned(),
                    name: "seedance2-5".to_owned(),
                    project_id: Some("viral-studio".to_owned()),
                    project_identity: Some("/repos/viral-studio".to_owned()),
                    project_root: "/worktrees/viral-studio/seedance2-5".to_owned(),
                    project_scope_rev: 0,
                    created_at: 1,
                }]),
        );
        let service = WorkspaceLifecycleService::with_backend(
            registry.clone(),
            Arc::new(FakeBackend::new("local")),
        )
        .with_backend_for_host("remote", remote);

        service
            .ensure_host_workspaces(Some("remote"))
            .await
            .unwrap();
        let stored = registry.get_workspace(row.id).unwrap().unwrap();
        assert_eq!(stored.project_id, "viral-studio");
        assert_eq!(
            stored.project_identity.as_deref(),
            Some("/repos/viral-studio")
        );
    }

    #[gpui::test]
    async fn test_reconciliation_hydrates_root_and_scope_from_daemon_together() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_root_hydrates_from_daemon").await;
        let mut row = AdeWorkspace::new("feature", "viral-studio", "/repos/viral-studio-old");
        row.project_identity = Some("/repos/viral-studio".to_owned());
        row.remote_host = Some("remote".to_owned());
        row.daemon_id = Some("daemon-a".to_owned());
        row.terminal_session_id = Some("ade-viral-studio-000001".to_owned());
        registry.create_workspace(row.clone()).await.unwrap();
        let remote = Arc::new(
            FakeBackend::new("remote")
                .identified("daemon-a")
                .holding(vec![BackendWorkspace {
                    id: "ade-viral-studio-000001".to_owned(),
                    name: "feature".to_owned(),
                    project_id: Some("viral-studio".to_owned()),
                    project_identity: Some("/repos/viral-studio-main".to_owned()),
                    project_root: "/repos/viral-studio-new".to_owned(),
                    project_scope_rev: 1,
                    created_at: 1,
                }]),
        );
        let service = WorkspaceLifecycleService::with_backend(
            registry.clone(),
            Arc::new(FakeBackend::new("local")),
        )
        .with_backend_for_host("remote", remote);

        service
            .ensure_host_workspaces(Some("remote"))
            .await
            .unwrap();
        let stored = registry.get_workspace(row.id).unwrap().unwrap();
        assert_eq!(stored.repository_path, Path::new("/repos/viral-studio-new"));
        assert_eq!(stored.project_scope_rev, 1);
        assert_eq!(stored.project_id, "viral-studio");
        assert_eq!(
            stored.project_identity.as_deref(),
            Some("/repos/viral-studio-main")
        );
    }

    #[gpui::test]
    async fn test_equal_root_revision_keeps_the_local_pending_root() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_equal_root_revision").await;
        let mut row = AdeWorkspace::new("feature", "viral-studio", "/repos/viral-studio-new");
        row.project_identity = Some("/repos/viral-studio-main".to_owned());
        row.project_scope_rev = 1;
        row.remote_host = Some("remote".to_owned());
        row.daemon_id = Some("daemon-a".to_owned());
        row.terminal_session_id = Some("ade-viral-studio-000001".to_owned());
        registry.create_workspace(row.clone()).await.unwrap();
        let remote = Arc::new(
            FakeBackend::new("remote")
                .identified("daemon-a")
                .holding(vec![BackendWorkspace {
                    id: "ade-viral-studio-000001".to_owned(),
                    name: "feature".to_owned(),
                    project_id: Some("viral-studio".to_owned()),
                    project_identity: Some("/repos/viral-studio-main".to_owned()),
                    project_root: "/repos/viral-studio-old".to_owned(),
                    project_scope_rev: 1,
                    created_at: 1,
                }]),
        );
        let service = WorkspaceLifecycleService::with_backend(
            registry.clone(),
            Arc::new(FakeBackend::new("local")),
        )
        .with_backend_for_host("remote", remote.clone());

        service
            .ensure_host_workspaces(Some("remote"))
            .await
            .unwrap();
        let stored = registry.get_workspace(row.id).unwrap().unwrap();
        assert_eq!(stored.repository_path, Path::new("/repos/viral-studio-new"));
        assert_eq!(stored.project_scope_rev, 1);
        assert_eq!(
            remote.project_scope_updates()[0].3.as_deref(),
            Some("/repos/viral-studio-new")
        );
    }

    #[gpui::test]
    async fn test_reconciliation_hydrates_scope_from_a_resolved_legacy_record() {
        let registry =
            AdeWorkspaceRegistry::open_test_db("test_scope_hydrates_from_legacy_record").await;
        let mut row = AdeWorkspace::new(
            "seedance2-5",
            "seedance2-5",
            "/worktrees/viral-studio/seedance2-5",
        );
        row.remote_host = Some("remote".to_owned());
        row.daemon_id = Some("daemon-a".to_owned());
        row.terminal_session_id = Some("ade-seedance2-5-000001".to_owned());
        registry.create_workspace(row.clone()).await.unwrap();
        let remote = Arc::new(
            FakeBackend::new("remote")
                .identified("daemon-a")
                .holding(vec![BackendWorkspace {
                    id: "ade-seedance2-5-000001".to_owned(),
                    name: "seedance2-5".to_owned(),
                    project_id: None,
                    project_identity: None,
                    project_root: "/worktrees/viral-studio/seedance2-5".to_owned(),
                    project_scope_rev: 0,
                    created_at: 1,
                }])
                .resolving_repository("/worktrees/viral-studio/seedance2-5", "/repos/viral-studio"),
        );
        let service = WorkspaceLifecycleService::with_backend(
            registry.clone(),
            Arc::new(FakeBackend::new("local")),
        )
        .with_backend_for_host("remote", remote);

        service
            .ensure_host_workspaces(Some("remote"))
            .await
            .unwrap();
        let stored = registry.get_workspace(row.id).unwrap().unwrap();
        assert_eq!(stored.project_id, "viral-studio");
        assert_eq!(
            stored.project_identity.as_deref(),
            Some("/repos/viral-studio")
        );
    }

    #[gpui::test]
    async fn test_missing_legacy_record_resolves_scope_without_a_daemon_workspace() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_scope_hydrates_dead_row").await;
        let mut row = AdeWorkspace::new(
            "seedance2-5",
            "seedance2-5",
            "/worktrees/viral-studio/seedance2-5",
        );
        row.remote_host = Some("remote".to_owned());
        row.daemon_id = Some("daemon-a".to_owned());
        row.terminal_session_id = Some("ade-seedance2-5-000001".to_owned());
        registry.create_workspace(row.clone()).await.unwrap();
        let remote = Arc::new(
            FakeBackend::new("remote")
                .identified("daemon-a")
                .resolving_repository("/worktrees/viral-studio/seedance2-5", "/repos/viral-studio"),
        );
        let service = WorkspaceLifecycleService::with_backend(
            registry.clone(),
            Arc::new(FakeBackend::new("local")),
        )
        .with_backend_for_host("remote", remote.clone());

        for _ in 0..2 {
            let listed = service
                .ensure_host_workspaces(Some("remote"))
                .await
                .unwrap();
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].project_id(), "viral-studio");
            assert_eq!(listed[0].project_identity(), "/repos/viral-studio");
        }
        assert_eq!(
            remote
                .calls()
                .iter()
                .filter(|call| call.starts_with("resolve:"))
                .count(),
            1,
            "the persisted scope is the success cache"
        );
        let stored = registry.get_workspace(row.id).unwrap().unwrap();
        assert_eq!(stored.project_id, "viral-studio");
        assert_eq!(
            stored.project_identity.as_deref(),
            Some("/repos/viral-studio")
        );
    }

    #[gpui::test]
    async fn test_missing_legacy_record_resolves_on_its_route_after_daemon_restart() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_scope_hydrates_stale_daemon").await;
        let mut row = AdeWorkspace::new(
            "seedance2-5",
            "seedance2-5",
            "/worktrees/viral-studio/seedance2-5",
        );
        row.remote_host = Some("remote".to_owned());
        row.daemon_id = Some("daemon-a".to_owned());
        row.terminal_session_id = Some("ade-seedance2-5-000001".to_owned());
        registry.create_workspace(row.clone()).await.unwrap();
        let remote = Arc::new(
            FakeBackend::new("remote")
                .identified("daemon-b")
                .resolving_repository("/worktrees/viral-studio/seedance2-5", "/repos/viral-studio"),
        );
        let service = WorkspaceLifecycleService::with_backend(
            registry.clone(),
            Arc::new(FakeBackend::new("local")),
        )
        .with_backend_for_host("remote", remote.clone());

        service
            .ensure_host_workspaces(Some("remote"))
            .await
            .unwrap();
        let stored = registry.get_workspace(row.id).unwrap().unwrap();
        assert_eq!(stored.project_id, "viral-studio");
        assert_eq!(
            stored.project_identity.as_deref(),
            Some("/repos/viral-studio")
        );
        assert_eq!(
            remote
                .calls()
                .iter()
                .filter(|call| call.starts_with("resolve:"))
                .count(),
            1
        );
    }

    #[gpui::test]
    async fn test_persisted_multi_root_scope_beats_a_legacy_daemon_record() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_multi_root_scope_precedence").await;
        let project_identity = util::path_list::PathList::new(&[
            PathBuf::from("/repos/project-a"),
            PathBuf::from("/repos/project-b"),
        ])
        .serialize()
        .paths;
        let mut row =
            AdeWorkspace::new("project-a", "multi-project", "/worktrees/project-a/feature");
        row.project_identity = Some(project_identity.clone());
        row.remote_host = Some("remote".to_owned());
        row.daemon_id = Some("daemon-a".to_owned());
        row.terminal_session_id = Some("ade-project-a-000001".to_owned());
        registry.create_workspace(row.clone()).await.unwrap();
        let remote = Arc::new(
            FakeBackend::new("remote")
                .identified("daemon-a")
                .holding(vec![record(
                    "ade-project-a-000001",
                    "project-a",
                    "/worktrees/project-a/feature",
                    1,
                )]),
        );
        let service = WorkspaceLifecycleService::with_backend(
            registry.clone(),
            Arc::new(FakeBackend::new("local")),
        )
        .with_backend_for_host("remote", remote.clone());

        for _ in 0..2 {
            let listed = service
                .ensure_host_workspaces(Some("remote"))
                .await
                .unwrap();
            assert_eq!(listed[0].project_id(), "multi-project");
            assert_eq!(listed[0].project_identity(), project_identity);
        }
        assert!(
            remote
                .calls()
                .iter()
                .all(|call| !call.starts_with("resolve:")),
            "an authoritative multi-root identity must not be replaced by a Git lookup"
        );
        assert_eq!(remote.project_scope_updates().len(), 1);
        let stored = registry.get_workspace(row.id).unwrap().unwrap();
        assert_eq!(stored.project_id, "multi-project");
        assert_eq!(stored.project_identity, Some(project_identity));
    }

    #[gpui::test]
    async fn test_discovery_retries_when_the_listing_changes_daemon() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_discovery_retries_identity").await;
        let remote = Arc::new(
            FakeBackend::new("remote")
                .identified("daemon-b")
                .listing_identities(vec![Some("daemon-a"), Some("daemon-b"), Some("daemon-b")])
                .holding(vec![BackendWorkspace {
                    id: "ade-current-000001".to_owned(),
                    name: "current".to_owned(),
                    project_id: None,
                    project_identity: None,
                    project_root: "/repos/current".to_owned(),
                    project_scope_rev: 0,
                    created_at: 1,
                }]),
        );
        let service =
            WorkspaceLifecycleService::with_backend(registry, Arc::new(FakeBackend::new("local")))
                .with_backend_for_host("remote", remote.clone());

        let listed = service
            .ensure_host_workspaces(Some("remote"))
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(discoveries(&listed).len(), 1);
        assert_eq!(
            remote
                .calls()
                .iter()
                .filter(|call| *call == "list_workspaces")
                .count(),
            4
        );
    }

    #[gpui::test]
    async fn test_legacy_kill_and_alias_discovery_share_the_daemon_lock(
        cx: &mut gpui::TestAppContext,
    ) {
        let registry = AdeWorkspaceRegistry::open_test_db("test_legacy_kill_alias_discovery").await;
        let daemon_workspace = BackendWorkspace {
            id: "ade-shared-000001".to_owned(),
            name: "shared".to_owned(),
            project_id: None,
            project_identity: None,
            project_root: "/repos/shared".to_owned(),
            project_scope_rev: 0,
            created_at: 1,
        };
        let backend = Arc::new(
            FakeBackend::new("remote")
                .identified("daemon-a")
                .holding(vec![daemon_workspace.clone()]),
        );
        let service = Arc::new(
            WorkspaceLifecycleService::with_backend(registry, Arc::new(FakeBackend::new("local")))
                .with_backend_for_host("old-alias", backend.clone())
                .with_backend_for_host("new-alias", backend.clone()),
        );
        let mut legacy = row_for_record(
            &daemon_workspace,
            Some("old-alias"),
            None,
            now_whole_seconds(),
        );
        legacy.status = WorkspaceStatus::Running;
        service
            .registry()
            .create_workspace(legacy.clone())
            .await
            .unwrap();

        let lock = service.daemon_decision_lock(DaemonKey::Instance("daemon-a".to_owned()));
        let guard = lock.lock().await;
        let discovery = {
            let service = service.clone();
            cx.background_spawn(
                async move { service.ensure_host_workspaces(Some("new-alias")).await },
            )
        };
        let kill = {
            let service = service.clone();
            let id = legacy.id.clone();
            cx.background_spawn(async move { service.kill_workspace(&id).await })
        };
        cx.run_until_parked();
        drop(guard);
        discovery.await.unwrap();
        kill.await.unwrap();

        let rows = service.registry().list_workspaces().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, legacy.id);
        assert!(rows[0].terminal_session_id.is_none());
        assert!(rows[0].daemon_id.is_none());
        assert!(backend.workspaces.lock().unwrap().is_empty());
    }

    #[gpui::test]
    async fn test_manual_remote_create_uses_the_git_project_identity() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_manual_remote_project_scope").await;
        let local = Arc::new(FakeBackend::new("local"));
        let checkout =
            PathBuf::from("/home/user/Code/worktrees/viral-studio/vast-dune/seedance2-5");
        let project_identity = PathBuf::from("/home/user/Code/viral-studio");
        let remote = Arc::new(
            FakeBackend::new("remote")
                .resolving_repository(checkout.clone(), project_identity.clone()),
        );
        let service = WorkspaceLifecycleService::with_backend(registry, local.clone())
            .with_backend_for_host("build-box", remote.clone());
        let requested = PathBuf::from("~/Code/worktrees/viral-studio/vast-dune/seedance2-5/src");

        let created = service
            .create_workspace_from_repository(
                "seedance2-5",
                requested.clone(),
                None,
                Some("build-box".to_owned()),
            )
            .await
            .expect("manual workspace creation should resolve its repository");

        assert_eq!(created.repository_path, checkout);
        assert_eq!(created.project_id, "viral-studio");
        assert_eq!(
            created.project_identity,
            Some(project_identity.display().to_string())
        );
        assert_eq!(created.remote_host.as_deref(), Some("build-box"));
        assert!(
            remote
                .calls()
                .contains(&format!("resolve:{}", requested.display()))
        );
        assert!(local.calls().is_empty());
    }

    /// A workspace the daemon holds and the registry does not becomes a row in
    /// the same pass that probes it — the sidebar showed nothing for one on
    /// 2026-08-05 because reconciliation only ever probed rows it already had.
    /// A workspace the daemon holds and this client has never used is on screen
    /// in the same pass that probes the rows — the sidebar showed nothing for
    /// one on 2026-08-05 — as a discovery, never as a row.
    #[gpui::test]
    async fn test_reconcile_discovers_daemon_only_workspaces() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_reconcile_discovers").await;
        let local = Arc::new(FakeBackend::new("local"));
        let service = WorkspaceLifecycleService::with_backend(registry, local.clone());

        let known = service
            .create_workspace("known", "project-a", "/repos/zed", None, None)
            .await
            .unwrap();
        local.hold(record(
            "ade-testproj-2de8b3",
            "ade-testproj-2de8b3",
            "/home/user/testproj",
            1_700_000_000,
        ));

        let reconciled = service.reconcile_all().await.unwrap();
        assert!(
            reconciled.host_errors.is_empty(),
            "{:?}",
            reconciled.host_errors
        );
        assert_eq!(reconciled.entries.len(), 2);

        // Rows lead; what the host merely holds follows them.
        let (row, _) = reconciled.entries[0]
            .persisted()
            .expect("this client's row leads");
        assert_eq!(row.id, known.id);

        let found = discoveries(&reconciled.entries);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].wire_id(), "ade-testproj-2de8b3");
        assert_eq!(found[0].name(), "testproj");
        // Probed in the same pass: the fake holds no session under that id, so
        // it reads as the gone one it is.
        assert_eq!(found[0].state(), SessionState::Dead);
        assert_eq!(
            service.registry().list_workspaces().unwrap().len(),
            1,
            "a discovery is not a row"
        );

        // A second pass records nothing further either.
        let again = service.reconcile_all().await.unwrap();
        assert_eq!(again.entries.len(), 2);
        assert_eq!(service.registry().list_workspaces().unwrap().len(), 1);
    }

    #[test]
    fn test_a_derived_workspace_name_is_recognised() {
        // What `tmux_session_name` produces, which is a session id in the place
        // a project name belongs.
        assert!(is_derived_workspace_name("ade-testproj-2de8b3"));
        assert!(is_derived_workspace_name("ade-feature-auth-012345"));
        // A name somebody typed, which is kept verbatim.
        assert!(!is_derived_workspace_name("testproj"));
        assert!(!is_derived_workspace_name("Investigation: vector DB"));
        assert!(!is_derived_workspace_name("my-project"));
        // The right shape but the wrong prefix, or the wrong id.
        assert!(!is_derived_workspace_name("zed-main-012345"));
        assert!(!is_derived_workspace_name("ade-main-01234"));
        assert!(!is_derived_workspace_name("ade-main-zzzzzz"));

        assert_eq!(
            display_name_for("ade-testproj-2de8b3", "testproj"),
            "testproj"
        );
        assert_eq!(display_name_for("a name", "testproj"), "a name");
    }

    #[gpui::test]
    async fn test_never_created_needs_no_backend() {
        let service = backendless_service("test_never_created_needs_no_backend").await;
        let mut workspace = AdeWorkspace::new("main", "project-a", "/repos/zed");
        service
            .registry()
            .create_workspace(workspace.clone())
            .await
            .unwrap();

        // No session id means nothing to ask the backend about, so the failing
        // backend never comes up.
        assert_eq!(
            service.probe(&mut workspace).await.unwrap(),
            SessionState::NeverCreated
        );
        assert_eq!(workspace.status, WorkspaceStatus::Creating);

        let (opened, state) = service.open_workspace(&workspace.id).await.unwrap();
        assert_eq!(state, SessionState::NeverCreated);
        assert_eq!(opened.status, WorkspaceStatus::Creating);
    }

    #[gpui::test]
    async fn test_backend_failure_records_error_status() {
        let service = backendless_service("test_backend_failure_records_error_status").await;

        let error = service
            .create_workspace("main", "project-a", "/repos/zed", None, None)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("no session backend in this test"));

        // The row survives, marked so the user can see what happened.
        let stored = service.registry().list_workspaces().unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].status, WorkspaceStatus::Error);
        assert!(stored[0].terminal_session_id.is_none());
    }

    /// Every entry point a remote workspace can reach lands on the backend for
    /// **its** host, and this machine's is never touched.
    ///
    /// The five refusal assertions this replaces (a remote workspace erroring
    /// out of probe / stop / kill / recreate / attach) are the same five paths,
    /// now asserted to route rather than to refuse.
    #[gpui::test]
    async fn test_a_remote_workspace_is_served_by_its_hosts_backend() {
        let registry =
            AdeWorkspaceRegistry::open_test_db("test_a_remote_workspace_is_served_by_its_host")
                .await;
        let local = Arc::new(FakeBackend::new("local"));
        let host = Arc::new(FakeBackend::new("dev-box"));
        let service = WorkspaceLifecycleService::with_backend(registry, local.clone())
            .with_backend_for_host("dev-box", host.clone());

        // The path is read on the host, so it is never checked here.
        let workspace = service
            .create_workspace(
                "main",
                "project-a",
                "/srv/checkouts/zed",
                Some("main".into()),
                Some("dev-box".into()),
            )
            .await
            .unwrap();
        let session = workspace.tmux_session_name();
        assert_eq!(workspace.remote_host.as_deref(), Some("dev-box"));
        assert_eq!(workspace.status, WorkspaceStatus::Running);
        assert_eq!(host.calls(), vec![format!("create:{session}")]);

        // Probe.
        let mut probed = workspace.clone();
        assert_eq!(
            service.probe(&mut probed).await.unwrap(),
            SessionState::Alive
        );

        // Attach: the argv is the host backend's, not this machine's.
        let argv = service.attach_command(&workspace).unwrap().argv;
        assert_eq!(argv, vec!["dev-box-attach".to_owned(), session.clone()]);

        // Recreate on a live session adopts rather than duplicating.
        let recreated = service.recreate_session(&workspace.id).await.unwrap();
        assert_eq!(
            recreated.terminal_session_id.as_deref(),
            Some(session.as_str())
        );

        // Stop detaches on the host; kill kills there.
        let stopped = service.stop_workspace(&workspace.id).await.unwrap();
        assert_eq!(stopped.status, WorkspaceStatus::Stopped);
        assert!(host.calls().contains(&format!("detach:{session}")));

        let killed = service.kill_workspace_session(&workspace.id).await.unwrap();
        assert_eq!(killed.status, WorkspaceStatus::Stopped);
        assert_eq!(
            killed.terminal_session_id.as_deref(),
            Some(session.as_str())
        );
        assert_eq!(killed.daemon_id, workspace.daemon_id);
        assert!(host.calls().contains(&format!("kill:{session}")));

        assert!(
            local.calls().is_empty(),
            "this machine's backend was asked about a remote workspace: {:?}",
            local.calls()
        );
    }

    /// A host that cannot be reached fails alone: local rows still reconcile,
    /// the failure is named against its host, and nothing is claimed about that
    /// host's sessions.
    #[gpui::test]
    async fn test_one_unreachable_host_does_not_block_the_others() {
        let registry =
            AdeWorkspaceRegistry::open_test_db("test_one_unreachable_host_does_not_block").await;
        let local = Arc::new(FakeBackend::new("local"));
        let down = Arc::new(FakeBackend::failing("h1", "ssh: connect: no route to host"));
        let service = WorkspaceLifecycleService::with_backend(registry, local.clone())
            .with_backend_for_host("h1", down.clone());

        let here = service
            .create_workspace("here", "project-a", "/repos/zed", None, None)
            .await
            .unwrap();
        // The host's own creation fails, which is a visible `error` row — and
        // the session id it never got is what makes it `NeverCreated` below.
        let there = {
            let mut there = AdeWorkspace::new("there", "project-a", "/srv/zed");
            there.remote_host = Some("h1".into());
            there.terminal_session_id = Some(there.tmux_session_name());
            there.status = WorkspaceStatus::Running;
            service
                .registry()
                .create_workspace(there.clone())
                .await
                .unwrap();
            there
        };

        let reconciled = service.reconcile_all().await.unwrap();
        let state_of = |id: &WorkspaceId| {
            reconciled
                .entries
                .iter()
                .filter_map(WorkspaceEntry::persisted)
                .find(|(workspace, _)| &workspace.id == id)
                .map(|(_, state)| state)
        };

        // Both rows are there. A host being down never empties the sidebar.
        assert_eq!(reconciled.entries.len(), 2);
        assert_eq!(state_of(&here.id), Some(SessionState::Alive));
        assert_eq!(state_of(&there.id), Some(SessionState::Unknown));

        assert_eq!(reconciled.host_errors.len(), 1);
        let (host, message) = &reconciled.host_errors[0];
        assert_eq!(host, "h1");
        assert!(message.contains("no route to host"), "{message}");

        // Nothing was written about the host that could not be asked: its row
        // still says what it last said.
        let stored = service
            .registry()
            .get_workspace(there.id.clone())
            .unwrap()
            .unwrap();
        assert_eq!(stored.status, WorkspaceStatus::Running);
        // Adoption refreshes once under its daemon lock. The host that is down
        // fails on the first listing and is not asked again.
        assert_eq!(
            local.calls(),
            vec![
                format!("create:{}", here.tmux_session_name()),
                "list_workspaces".to_owned(),
                "list_workspaces".to_owned(),
                "list".to_owned()
            ]
        );
        assert_eq!(down.calls(), vec!["list_workspaces".to_owned()]);
    }

    /// Two hosts' events arrive on one stream, including a host whose backend
    /// only appears after the caller subscribed.
    #[gpui::test]
    async fn test_status_from_every_host_arrives_on_one_stream() {
        let registry =
            AdeWorkspaceRegistry::open_test_db("test_status_from_every_host_one_stream").await;
        let local = Arc::new(FakeBackend::new("local"));
        let host = Arc::new(FakeBackend::new("h1"));
        let service = WorkspaceLifecycleService::with_backend(registry, local.clone())
            .with_backend_for_host("h1", host.clone());

        assert_eq!(service.status_delivery(), StatusDelivery::Push);
        let events = service.subscribe_status().unwrap();

        local.push(StatusEvent::new(
            "ade-here-000001",
            SessionChange::Status(WorkspaceStatus::Running),
        ));
        host.push(StatusEvent::new("ade-there-000002", SessionChange::Exited));

        let mut seen = vec![
            smol::block_on(events.recv()).unwrap(),
            smol::block_on(events.recv()).unwrap(),
        ];
        seen.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(
            seen,
            vec![
                StatusEvent::new(
                    "ade-here-000001",
                    SessionChange::Status(WorkspaceStatus::Running)
                ),
                StatusEvent::new("ade-there-000002", SessionChange::Exited),
            ]
        );
    }

    /// Two merged streams, one push channel per host.
    ///
    /// The point of the fanout: taking the layout stream after the status one
    /// must not cost a second connection to every host, because a host's push
    /// channel is one of the two connections the transport invariant allows.
    #[gpui::test]
    async fn test_status_and_layouts_share_one_subscription_per_host() {
        let registry =
            AdeWorkspaceRegistry::open_test_db("test_status_and_layouts_share_one").await;
        let local = Arc::new(FakeBackend::new("local"));
        let service = WorkspaceLifecycleService::with_backend(registry, local.clone());

        let statuses = service.subscribe_status().unwrap();
        let layouts = service.subscribe_layout().unwrap();
        let workspace_changes = service.subscribe_workspace_changes().unwrap();
        assert_eq!(
            local
                .calls()
                .iter()
                .filter(|call| *call == "subscribe")
                .count(),
            1,
            "a second subscriber must ride the first subscription"
        );

        local.push(StatusEvent::new("ade-here-000001", SessionChange::Exited));
        local.push_layout(LayoutEvent {
            workspace_id: "ade-here-000001".to_owned(),
            layout: LayoutDoc::empty(),
            rev: 7,
        });

        // Each kind lands on its own stream, in the order the daemon sent them.
        assert_eq!(
            smol::block_on(statuses.recv()).unwrap(),
            StatusEvent::new("ade-here-000001", SessionChange::Exited)
        );
        let WorkspaceEvent::Layout {
            remote_host,
            daemon_id,
            event: layout,
        } = smol::block_on(layouts.recv()).unwrap()
        else {
            panic!("a pushed layout must arrive as one");
        };
        assert_eq!(remote_host, None);
        assert_eq!(daemon_id, None);
        assert_eq!(layout.workspace_id, "ade-here-000001");
        assert_eq!(layout.rev, 7);

        local.push_workspace_changed(LayoutEvent {
            workspace_id: "ade-here-000001".to_owned(),
            layout: LayoutDoc::empty(),
            rev: 7,
        });
        assert!(matches!(
            smol::block_on(layouts.recv()).unwrap(),
            WorkspaceEvent::Layout { .. }
        ));
        smol::block_on(workspace_changes.recv()).unwrap();

        local.push_workspace_reset(LayoutEvent {
            workspace_id: "ade-here-000001".to_owned(),
            layout: LayoutDoc::empty(),
            rev: 1,
        });
        let WorkspaceEvent::Reset {
            remote_host,
            daemon_id,
            event: reset,
        } = smol::block_on(layouts.recv()).unwrap()
        else {
            panic!("an incarnation replacement must remain distinct from a kill");
        };
        assert_eq!(remote_host, None);
        assert_eq!(daemon_id, None);
        assert_eq!(reset.workspace_id, "ade-here-000001");
        assert_eq!(reset.rev, 1);
        smol::block_on(workspace_changes.recv()).unwrap();

        // A killed workspace rides the same stream, so a client cannot see a
        // layout for a workspace it has already been told is gone.
        local.push_workspace_removed("ade-here-000001");
        assert_eq!(
            smol::block_on(layouts.recv()).unwrap(),
            WorkspaceEvent::Removed {
                remote_host: None,
                daemon_id: None,
                workspace_id: "ade-here-000001".to_owned()
            }
        );
        smol::block_on(workspace_changes.recv()).unwrap();
    }

    #[gpui::test]
    async fn test_workspace_events_keep_their_source_host() {
        let registry =
            AdeWorkspaceRegistry::open_test_db("test_workspace_events_keep_source_host").await;
        let local = Arc::new(FakeBackend::new("local"));
        let remote = Arc::new(FakeBackend::new("h1"));
        let service = WorkspaceLifecycleService::with_backend(registry, local.clone())
            .with_backend_for_host("h1", remote.clone());
        let events = service.subscribe_layout().unwrap();

        local.push_workspace_removed("same-id");
        remote.push_workspace_removed("same-id");

        let mut received = vec![
            smol::block_on(events.recv()).unwrap(),
            smol::block_on(events.recv()).unwrap(),
        ];
        received.sort_by_key(|event| match event {
            WorkspaceEvent::Layout { remote_host, .. }
            | WorkspaceEvent::Reset { remote_host, .. }
            | WorkspaceEvent::Removed { remote_host, .. } => remote_host.clone(),
        });
        assert_eq!(
            received,
            vec![
                WorkspaceEvent::Removed {
                    remote_host: None,
                    daemon_id: None,
                    workspace_id: "same-id".to_owned(),
                },
                WorkspaceEvent::Removed {
                    remote_host: Some("h1".to_owned()),
                    daemon_id: None,
                    workspace_id: "same-id".to_owned(),
                },
            ]
        );
    }

    #[gpui::test]
    async fn test_disconnect_replaces_the_event_pump_with_the_new_backend() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_disconnect_replaces_pump").await;
        let old = Arc::new(FakeBackend::new("old"));
        let service = WorkspaceLifecycleService::with_backend(registry, old.clone());
        let events = service.subscribe_status().unwrap();

        service.disconnect();
        let new = Arc::new(FakeBackend::new("new"));
        service
            .backends
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(None, new.clone());
        service.ensure_pumps().unwrap();

        old.push(StatusEvent::new(
            "stale",
            SessionChange::Status(WorkspaceStatus::Running),
        ));
        new.push(StatusEvent::new(
            "current",
            SessionChange::Status(WorkspaceStatus::Running),
        ));

        assert_eq!(
            smol::block_on(events.recv()).unwrap().id.as_str(),
            "current",
            "the disconnected backend's pump must not feed the live stream"
        );
        assert_eq!(
            new.calls()
                .iter()
                .filter(|call| *call == "subscribe")
                .count(),
            1,
            "the replacement backend needs its own pump"
        );
    }

    #[gpui::test]
    async fn test_killing_sessions_preserves_daemon_mapping_for_recreate() {
        let registry = AdeWorkspaceRegistry::open_test_db(
            "test_killing_sessions_preserves_daemon_mapping_for_recreate",
        )
        .await;
        let backend = Arc::new(FakeBackend::new("local").identified("daemon-a"));
        let service = WorkspaceLifecycleService::with_backend(registry, backend.clone());
        let workspace = service
            .create_workspace("main", "project-a", "/repos/zed", None, None)
            .await
            .expect("the workspace session should be created");
        let daemon_workspace_id = workspace.daemon_workspace_id();

        let killed = service
            .kill_workspace_session(&workspace.id)
            .await
            .expect("the workspace sessions should be killed");
        assert_eq!(
            killed.terminal_session_id.as_deref(),
            Some(daemon_workspace_id.as_str())
        );
        assert_eq!(killed.daemon_id.as_deref(), Some("daemon-a"));
        assert_eq!(
            service
                .registry()
                .get_workspace(workspace.id.clone())
                .expect("the retained mapping should remain readable"),
            Some(killed.clone())
        );

        let (opened, state) = service
            .open_workspace(&workspace.id)
            .await
            .expect("the retained workspace should reopen");
        assert_eq!(state, SessionState::Dead);
        assert_eq!(opened.daemon_workspace_id(), daemon_workspace_id);

        let recreated = service
            .recreate_session(&workspace.id)
            .await
            .expect("the retained workspace should recreate its session");
        assert_eq!(recreated.daemon_workspace_id(), daemon_workspace_id);
        assert_eq!(recreated.daemon_id.as_deref(), Some("daemon-a"));
        assert!(
            backend
                .calls()
                .contains(&format!("create:{daemon_workspace_id}|daemon-a"))
        );
        assert!(
            !backend
                .calls()
                .iter()
                .any(|call| call.starts_with("kill_workspace:"))
        );
    }

    /// The workspace-level kill, and what it does to the registry.
    #[gpui::test]
    async fn test_killing_a_workspace_is_one_call_and_forgets_its_session() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_killing_a_workspace_is_one").await;
        let backend = Arc::new(FakeBackend::new("local"));
        let service = WorkspaceLifecycleService::with_backend(registry, backend.clone());

        let workspace = service
            .create_workspace("main", "project-a", "/repos/zed", None, None)
            .await
            .unwrap();
        let session = workspace.tmux_session_name();

        let killed = service.kill_workspace(&workspace.id).await.unwrap();

        // One workspace-level call, named by the id the daemon knows the
        // workspace by — not a session kill, and not a listing first.
        assert!(
            backend
                .calls()
                .contains(&format!("kill_workspace:{session}")),
            "{:?}",
            backend.calls()
        );
        assert!(
            !backend
                .calls()
                .iter()
                .any(|call| call == &format!("kill:{session}")),
            "the workspace kill must not also take the session by hand: {:?}",
            backend.calls()
        );

        // The registry keeps the row and forgets the session: a killed
        // workspace reads as never-created, ready to be recreated.
        assert_eq!(killed.status, WorkspaceStatus::Stopped);
        assert!(killed.terminal_session_id.is_none());
        assert_eq!(
            service
                .registry()
                .get_workspace(workspace.id.clone())
                .unwrap(),
            Some(killed)
        );
    }

    #[gpui::test]
    async fn test_killed_workspace_can_be_recreated_on_a_new_daemon() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_kill_then_recreate_on_b").await;
        let daemon_a = Arc::new(FakeBackend::new("a").identified("daemon-a"));
        let service = WorkspaceLifecycleService::with_backend(registry, daemon_a.clone());
        let workspace = service
            .create_workspace("main", "project-a", "/repos/zed", None, None)
            .await
            .unwrap();

        let killed = service.kill_workspace(&workspace.id).await.unwrap();
        assert!(killed.terminal_session_id.is_none());
        assert!(killed.daemon_id.is_none());

        let daemon_b = Arc::new(FakeBackend::new("b").identified("daemon-b"));
        service
            .backends
            .lock()
            .unwrap()
            .insert(None, daemon_b.clone());
        let recreated = service.recreate_session(&workspace.id).await.unwrap();
        assert_eq!(recreated.daemon_id.as_deref(), Some("daemon-b"));
        assert!(recreated.terminal_session_id.is_some());
        assert!(
            daemon_a
                .calls()
                .iter()
                .any(|call| call.contains("kill_workspace:") && call.ends_with("|daemon-a"))
        );
        assert!(
            daemon_b
                .calls()
                .iter()
                .any(|call| call.starts_with("create:"))
        );
    }

    #[gpui::test]
    async fn test_a_workspace_kill_refusal_never_falls_back_to_the_session() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_kill_falls_back").await;
        let backend = Arc::new(FakeBackend::new("local").without_workspace_kill());
        let service = WorkspaceLifecycleService::with_backend(registry, backend.clone());

        let workspace = service
            .create_workspace("main", "project-a", "/repos/zed", None, None)
            .await
            .unwrap();
        let session = workspace.tmux_session_name();

        service
            .kill_workspace(&workspace.id)
            .await
            .expect_err("a workspace-record refusal must remain visible");

        assert!(
            !backend.calls().contains(&format!("kill:{session}")),
            "a workspace refusal must not become a session kill: {:?}",
            backend.calls()
        );
        assert_eq!(
            service
                .registry()
                .get_workspace(workspace.id.clone())
                .unwrap(),
            Some(workspace)
        );
    }

    #[gpui::test]
    async fn test_an_identified_workspace_kill_never_falls_back_after_refusal() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_identified_kill_refusal").await;
        let backend = Arc::new(
            FakeBackend::new("local")
                .identified("daemon-a")
                .without_workspace_kill(),
        );
        let service = WorkspaceLifecycleService::with_backend(registry, backend.clone());
        let workspace = service
            .create_workspace("main", "project-a", "/repos/zed", None, None)
            .await
            .unwrap();
        let session = workspace.daemon_workspace_id();

        service
            .kill_workspace(&workspace.id)
            .await
            .expect_err("an identified daemon refusal must propagate");

        assert!(
            backend
                .calls()
                .contains(&format!("kill_workspace:{session}|daemon-a"))
        );
        assert!(
            !backend
                .calls()
                .contains(&format!("kill:{session}|daemon-a"))
        );
        assert_eq!(
            service
                .registry()
                .get_workspace(workspace.id.clone())
                .unwrap(),
            Some(workspace)
        );
    }

    #[gpui::test]
    async fn test_resetting_workspace_sessions_deletes_duplicates_and_preserves_target() {
        let registry =
            AdeWorkspaceRegistry::open_test_db("test_reset_workspace_sessions_scoped").await;
        let backend = Arc::new(FakeBackend::new("local"));
        let other_host = Arc::new(FakeBackend::new("other-host"));
        let service = WorkspaceLifecycleService::with_backend(registry, backend.clone())
            .with_backend_for_host("other.example", other_host.clone());

        let target = service
            .create_workspace("target", "project-a", "/repos/target", None, None)
            .await
            .expect("target workspace should be created");
        let duplicate = service
            .create_workspace("stale", "project-a", "/repos/target", None, None)
            .await
            .expect("stale workspace should be created");
        let sibling = service
            .create_workspace("sibling", "project-b", "/repos/sibling", None, None)
            .await
            .expect("sibling workspace should be created");
        let same_path_other_host = service
            .create_workspace(
                "remote",
                "project-a",
                "/repos/target",
                None,
                Some("other.example".to_owned()),
            )
            .await
            .expect("same path on another host should be created");
        let sibling_before = service
            .registry()
            .get_workspace(sibling.id.clone())
            .expect("sibling lookup should succeed")
            .expect("sibling should be registered");
        let other_host_before = service
            .registry()
            .get_workspace(same_path_other_host.id.clone())
            .expect("other-host lookup should succeed")
            .expect("other-host workspace should be registered");
        let target_session = target.daemon_workspace_id();
        let duplicate_session = duplicate.daemon_workspace_id();
        let sibling_session = sibling.daemon_workspace_id();

        let (reset, attached) = service
            .reset_workspace_sessions(&target.id)
            .await
            .expect("reset should recreate the target session");

        assert_eq!(reset.id, target.id);
        assert_eq!(reset.status, WorkspaceStatus::Running);
        assert_eq!(reset.terminal_session_id, target.terminal_session_id);
        assert_eq!(attached.session_id, target_session);
        let calls = backend.calls();
        assert_eq!(
            calls
                .iter()
                .filter(|call| *call == &format!("reset:{target_session}:/repos/target"))
                .count(),
            1,
            "reset must use the backend's old-daemon recovery path: {calls:?}"
        );
        assert_eq!(
            calls
                .iter()
                .filter(|call| *call == &format!("kill:{target_session}"))
                .count(),
            1,
            "only the target workspace should be killed: {calls:?}"
        );
        assert_eq!(
            calls
                .iter()
                .filter(|call| *call == &format!("create:{target_session}"))
                .count(),
            2,
            "the target should have its original and replacement sessions: {calls:?}"
        );
        assert_eq!(
            calls
                .iter()
                .filter(|call| *call == &format!("kill_workspace:{duplicate_session}"))
                .count(),
            1,
            "the stale daemon workspace record must be removed: {calls:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|call| call == &format!("kill_workspace:{target_session}")),
            "a duplicate row sharing the target identity must not kill it: {calls:?}"
        );
        assert!(
            !calls.iter().any(|call| {
                call == &format!("kill:{sibling_session}")
                    || call == &format!("kill_workspace:{sibling_session}")
            }),
            "the sibling must remain alive: {calls:?}"
        );
        assert_eq!(
            service
                .registry()
                .get_workspace(duplicate.id.clone())
                .expect("stale workspace lookup should succeed"),
            None,
            "a killed duplicate must not be re-adoptable through its old row"
        );
        assert_eq!(
            service
                .registry()
                .get_workspace(target.id.clone())
                .expect("target lookup should succeed"),
            Some(reset)
        );
        assert_eq!(
            service
                .registry()
                .get_workspace(sibling.id.clone())
                .expect("sibling lookup should still succeed"),
            Some(sibling_before)
        );
        assert_eq!(
            service
                .registry()
                .get_workspace(same_path_other_host.id.clone())
                .expect("other-host lookup should still succeed"),
            Some(other_host_before)
        );
        assert!(
            !other_host
                .calls()
                .iter()
                .any(|call| call.starts_with("kill:") || call.starts_with("kill_workspace:")),
            "the same path on another host is a different worktree"
        );
    }

    #[gpui::test]
    async fn test_reset_waits_for_the_daemon_ownership_lock(cx: &mut gpui::TestAppContext) {
        let registry = AdeWorkspaceRegistry::open_test_db("test_reset_waits_for_ownership").await;
        let backend = Arc::new(FakeBackend::new("local").identified("daemon-a"));
        let service = Arc::new(WorkspaceLifecycleService::with_backend(
            registry,
            backend.clone(),
        ));
        let workspace = service
            .create_workspace("main", "project-a", "/repos/zed", None, None)
            .await
            .unwrap();
        let lock = service.daemon_decision_lock(DaemonKey::Instance("daemon-a".to_owned()));
        let guard = lock.lock().await;
        let reset = {
            let service = service.clone();
            let id = workspace.id.clone();
            cx.background_spawn(async move { service.reset_workspace_sessions(&id).await })
        };
        cx.run_until_parked();
        assert!(
            !backend
                .calls()
                .iter()
                .any(|call| call.starts_with("reset:"))
        );

        drop(guard);
        reset.await.unwrap();
        assert!(
            backend
                .calls()
                .iter()
                .any(|call| call.starts_with("reset:"))
        );
    }

    #[gpui::test]
    async fn test_resetting_workspace_sessions_preserves_another_project_at_the_same_path() {
        let registry = AdeWorkspaceRegistry::open_test_db(
            "test_reset_workspace_sessions_preserves_other_project",
        )
        .await;
        let backend = Arc::new(FakeBackend::new("local"));
        let service = WorkspaceLifecycleService::with_backend(registry, backend.clone());

        let target = service
            .create_workspace("target", "project-a", "/repos/shared", None, None)
            .await
            .expect("target workspace should be created");
        let other_project = service
            .create_workspace("other", "project-b", "/repos/shared", None, None)
            .await
            .expect("other project's workspace should be created");
        let other_session = other_project.daemon_workspace_id();

        service
            .reset_workspace_sessions(&target.id)
            .await
            .expect("target workspace should be reset");

        assert!(
            !backend.calls().iter().any(|call| {
                call == &format!("kill:{other_session}")
                    || call == &format!("kill_workspace:{other_session}")
            }),
            "resetting one project must not kill another project's workspace: {:?}",
            backend.calls()
        );
        assert_eq!(
            service
                .registry()
                .get_workspace(other_project.id.clone())
                .expect("other-project lookup should succeed"),
            Some(other_project),
            "resetting one project must not delete another project's registry row"
        );
    }

    #[gpui::test]
    async fn test_reset_preserves_the_same_workspace_id_on_another_daemon() {
        let registry =
            AdeWorkspaceRegistry::open_test_db("test_reset_preserves_other_daemon").await;
        let backend = Arc::new(FakeBackend::new("local").identified("daemon-a"));
        let service = WorkspaceLifecycleService::with_backend(registry, backend.clone());
        let session_id = "ade-main-000001".to_owned();
        let target = AdeWorkspace {
            terminal_session_id: Some(session_id.clone()),
            daemon_id: Some("daemon-a".to_owned()),
            ..AdeWorkspace::new("main", "project-a", "/repos/shared")
        };
        let other = AdeWorkspace {
            terminal_session_id: Some(session_id),
            daemon_id: Some("daemon-b".to_owned()),
            ..AdeWorkspace::new("main-copy", "project-a", "/repos/shared")
        };
        service
            .registry()
            .create_workspace(target.clone())
            .await
            .unwrap();
        service
            .registry()
            .create_workspace(other.clone())
            .await
            .unwrap();

        service.reset_workspace_sessions(&target.id).await.unwrap();

        assert_eq!(
            service.registry().get_workspace(other.id.clone()).unwrap(),
            Some(other)
        );
        assert!(!backend.calls().iter().any(|call| call.contains("daemon-b")));
    }

    #[gpui::test]
    async fn test_cleanup_dead_workspaces_reprobes_before_kill_and_delete() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_cleanup_dead_workspaces").await;
        let backend = Arc::new(FakeBackend::new("local"));
        let service = WorkspaceLifecycleService::with_backend(registry, backend.clone());

        let first_dead = service
            .create_workspace("first-dead", "project-a", "/repos/first", None, None)
            .await
            .expect("first dead workspace should be created");
        let second_dead = service
            .create_workspace("second-dead", "project-b", "/repos/second", None, None)
            .await
            .expect("second dead workspace should be created");
        let revived = service
            .create_workspace("revived", "project-c", "/repos/revived", None, None)
            .await
            .expect("revived workspace should be created");
        let alive = service
            .create_workspace("alive", "project-d", "/repos/alive", None, None)
            .await
            .expect("alive workspace should be created");
        let unconfirmed_dead = service
            .create_workspace(
                "unconfirmed-dead",
                "project-e",
                "/repos/unconfirmed",
                None,
                None,
            )
            .await
            .expect("unconfirmed workspace should be created");
        let confirmed = vec![
            first_dead.id.clone(),
            second_dead.id.clone(),
            revived.id.clone(),
        ];

        backend.remove_session(&first_dead.daemon_workspace_id());
        backend.remove_session(&second_dead.daemon_workspace_id());
        backend.remove_session(&revived.daemon_workspace_id());
        backend.remove_session(&unconfirmed_dead.daemon_workspace_id());
        backend.revive_after_next_list(revived.daemon_workspace_id());

        assert_eq!(
            service
                .cleanup_dead_workspaces(confirmed)
                .await
                .expect("dead workspace cleanup should succeed"),
            2
        );

        for workspace in [&first_dead, &second_dead] {
            assert_eq!(
                service
                    .registry()
                    .get_workspace(workspace.id.clone())
                    .expect("cleaned workspace lookup should succeed"),
                None
            );
            assert!(
                backend.calls().contains(&format!(
                    "kill_workspace:{}",
                    workspace.daemon_workspace_id()
                )),
                "the daemon record must be killed before its row is removed: {:?}",
                backend.calls()
            );
        }
        for workspace in [&revived, &alive, &unconfirmed_dead] {
            assert!(
                service
                    .registry()
                    .get_workspace(workspace.id.clone())
                    .expect("preserved workspace lookup should succeed")
                    .is_some(),
                "an alive or unconfirmed workspace must survive cleanup"
            );
            assert!(
                !backend.calls().contains(&format!(
                    "kill_workspace:{}",
                    workspace.daemon_workspace_id()
                )),
                "an alive or unconfirmed workspace must not be killed: {:?}",
                backend.calls()
            );
        }
    }

    #[gpui::test]
    async fn test_cleanup_keeps_row_when_workspace_record_kill_fails() {
        let registry =
            AdeWorkspaceRegistry::open_test_db("test_cleanup_keeps_row_on_kill_failure").await;
        let backend = Arc::new(FakeBackend::new("local").without_workspace_kill());
        let service = WorkspaceLifecycleService::with_backend(registry, backend.clone());
        let workspace = service
            .create_workspace("dead", "project-a", "/repos/dead", None, None)
            .await
            .expect("workspace should be created");
        let daemon_workspace_id = workspace.daemon_workspace_id();
        backend.remove_session(&daemon_workspace_id);

        let error = service
            .cleanup_dead_workspaces(vec![workspace.id.clone()])
            .await
            .expect_err("a failed workspace-record kill must fail cleanup");

        assert!(
            format!("{error:#}").contains("killing daemon workspace record"),
            "the record-kill failure should remain visible: {error:#}"
        );
        let retained = service
            .registry()
            .get_workspace(workspace.id.clone())
            .expect("workspace lookup should succeed")
            .expect("a failed record kill must retain the registry row");
        assert_eq!(
            retained.terminal_session_id.as_deref(),
            Some(daemon_workspace_id.as_str())
        );
        let calls = backend.calls();
        assert!(
            calls.contains(&format!("kill_workspace:{daemon_workspace_id}")),
            "cleanup must attempt the workspace-record kill: {calls:?}"
        );
        assert!(
            !calls.contains(&format!("kill:{daemon_workspace_id}")),
            "cleanup must not convert a record-kill failure into session-only success: {calls:?}"
        );
    }

    /// The whole remote path against **real** ssh to `localhost`: `--ensure`
    /// over a short-lived connection, one `ssh -L` forward, and create / list /
    /// attach / kill as channels on it.
    ///
    /// "Remote" is this box — the ssh, the forward, the daemon and the channel
    /// multiplexing are all real, only the network is short. Gated on loopback
    /// ssh working and on `ADE_TEST_DAEMON_BIN` naming a built daemon, exactly
    /// like `ade_session`'s own loopback tests, so a machine without either
    /// still gets a green `cargo test`.
    ///
    /// The host's paths are a temp dir rather than `~/.ade`: a test must not
    /// install a daemon into the operator's real home and leave it running.
    #[cfg(unix)]
    #[gpui::test]
    async fn test_a_remote_workspace_end_to_end_over_loopback_ssh() {
        let Some(extra_args) = loopback_ssh_args() else {
            return;
        };
        let daemon_binary = match std::env::var("ADE_TEST_DAEMON_BIN") {
            Ok(binary) if !binary.is_empty() => binary,
            _ => {
                eprintln!("skipping: ADE_TEST_DAEMON_BIN is not set");
                return;
            }
        };

        let dir = tempfile::TempDir::new().expect("temp dir");
        let remote_socket = dir.path().join("daemon.sock").display().to_string();
        let remote_state = dir.path().join("state").display().to_string();
        let local_socket = dir.path().join("forwarded.sock");
        // Dropped before the temp dir, since the pid file it needs lives in it.
        let _daemon = DaemonGuard {
            state_dir: remote_state.clone().into(),
            host: ade_session::SshHost::new("localhost").with_extra_args(extra_args.clone()),
        };

        let backend = Arc::new(DaemonBackend::remote_at(
            "localhost",
            extra_args,
            local_socket.clone(),
            (daemon_binary, remote_socket, remote_state),
        ));
        let registry =
            AdeWorkspaceRegistry::open_test_db("test_remote_workspace_over_loopback").await;
        let service =
            WorkspaceLifecycleService::with_backend(registry, Arc::new(FakeBackend::new("local")))
                .with_backend_for_host("localhost", backend);

        // Creating brings the daemon up over one ssh command, establishes the
        // forward, and makes the session through it.
        let workspace = service
            .create_workspace(
                "remote spike",
                "project-a",
                dir.path(),
                None,
                Some("localhost".into()),
            )
            .await
            .expect("creating a session on the host");
        assert_eq!(workspace.status, WorkspaceStatus::Running);
        assert!(
            local_socket.exists(),
            "the forward bound its local socket at {}",
            local_socket.display()
        );

        // Listing goes back through the same forward.
        let reconciled = service.reconcile_all().await.expect("reconciling");
        assert!(
            reconciled.host_errors.is_empty(),
            "{:?}",
            reconciled.host_errors
        );
        assert_eq!(
            reconciled.entries.first().map(WorkspaceEntry::state),
            Some(SessionState::Alive)
        );

        // The attach argv is a local client pointed at the forwarded socket —
        // one more channel on the host's single ssh connection, no ssh in it.
        let argv = service
            .attach_command(&workspace)
            .expect("an attach argv")
            .argv;
        assert_eq!(argv[1], "attach");
        assert_eq!(argv[3], "--socket");
        assert_eq!(argv[4], local_socket.display().to_string());
        assert!(
            !argv.iter().any(|argument| argument == "ssh"),
            "a per-terminal ssh would break the one-connection-per-host rule: {argv:?}"
        );

        let killed = service
            .kill_workspace_session(&workspace.id)
            .await
            .expect("killing through the forward");
        assert_eq!(killed.terminal_session_id, workspace.terminal_session_id);
        assert_eq!(killed.daemon_id, workspace.daemon_id);
        let after = service.reconcile_all().await.expect("reconciling again");
        assert_eq!(
            after.entries.first().map(WorkspaceEntry::state),
            Some(SessionState::Dead)
        );
    }

    /// The same path over the **TCP-mode** forward: `ssh -L
    /// 127.0.0.1:<port>:<remote.sock>` rather than a local Unix socket.
    ///
    /// This is the transport a Windows client is on — its ssh cannot bind a
    /// local Unix socket, while the far end of the forward stays a Unix socket
    /// because that end belongs to the remote sshd. Linux is the only place ADE
    /// has tests, so it is forced on here rather than shipping unrun.
    #[cfg(unix)]
    #[gpui::test]
    async fn test_a_remote_workspace_end_to_end_over_a_tcp_forward() {
        let Some(extra_args) = loopback_ssh_args() else {
            return;
        };
        let daemon_binary = match std::env::var("ADE_TEST_DAEMON_BIN") {
            Ok(binary) if !binary.is_empty() => binary,
            _ => {
                eprintln!("skipping: ADE_TEST_DAEMON_BIN is not set");
                return;
            }
        };

        let dir = tempfile::TempDir::new().expect("temp dir");
        let remote_socket = dir.path().join("daemon.sock").display().to_string();
        let remote_state = dir.path().join("state").display().to_string();
        let _daemon = DaemonGuard {
            state_dir: remote_state.clone().into(),
            host: ade_session::SshHost::new("localhost").with_extra_args(extra_args.clone()),
        };

        let backend = Arc::new(
            DaemonBackend::remote_over_tcp_at(
                "localhost",
                extra_args,
                (daemon_binary, remote_socket, remote_state),
            )
            .expect("reserving a loopback port"),
        );
        let registry = AdeWorkspaceRegistry::open_test_db("test_remote_workspace_over_tcp").await;
        let service =
            WorkspaceLifecycleService::with_backend(registry, Arc::new(FakeBackend::new("local")))
                .with_backend_for_host("localhost", backend);

        // `--ensure` over one ssh command, the TCP forward, then the session
        // created through a channel on it.
        let workspace = service
            .create_workspace(
                "remote spike",
                "project-a",
                dir.path(),
                None,
                Some("localhost".into()),
            )
            .await
            .expect("creating a session on the host");
        assert_eq!(workspace.status, WorkspaceStatus::Running);

        // Listing goes back through the same forward.
        let reconciled = service.reconcile_all().await.expect("reconciling");
        assert!(
            reconciled.host_errors.is_empty(),
            "{:?}",
            reconciled.host_errors
        );
        assert_eq!(
            reconciled.entries.first().map(WorkspaceEntry::state),
            Some(SessionState::Alive)
        );

        // The attach argv is a local client pointed at the forwarded *port* —
        // still one more channel on the host's single ssh connection, still no
        // ssh in it.
        let argv = service
            .attach_command(&workspace)
            .expect("an attach argv")
            .argv;
        assert_eq!(argv[1], "attach");
        assert_eq!(argv[3], "--tcp");
        assert!(
            argv[4].starts_with("127.0.0.1:"),
            "the client is pointed at a loopback address: {argv:?}"
        );
        assert!(
            !argv.iter().any(|argument| argument == "--socket"),
            "a tcp-mode link never names a socket: {argv:?}"
        );
        assert!(
            !argv.iter().any(|argument| argument == "ssh"),
            "a per-terminal ssh would break the one-connection-per-host rule: {argv:?}"
        );

        let killed = service
            .kill_workspace_session(&workspace.id)
            .await
            .expect("killing through the forward");
        assert_eq!(killed.terminal_session_id, workspace.terminal_session_id);
        assert_eq!(killed.daemon_id, workspace.daemon_id);
        let after = service.reconcile_all().await.expect("reconciling again");
        assert_eq!(
            after.entries.first().map(WorkspaceEntry::state),
            Some(SessionState::Dead)
        );
    }

    /// The extra ssh arguments for `localhost` as a remote host, or `None`
    /// (having said so) when this box has no loopback ssh. The probe is a real
    /// handshake, so it runs at most once per test binary.
    #[cfg(unix)]
    fn loopback_ssh_args() -> Option<Vec<String>> {
        use ade_session::deploy::HostExec as _;
        use std::sync::OnceLock;

        static AVAILABLE: OnceLock<bool> = OnceLock::new();

        let key = PathBuf::from(std::env::var("HOME").expect("HOME"))
            .join(".ssh")
            .join("id_ed25519_ade_test");
        let args = vec![
            "-i".to_owned(),
            key.display().to_string(),
            "-o".to_owned(),
            "IdentitiesOnly=yes".to_owned(),
            "-o".to_owned(),
            "StrictHostKeyChecking=accept-new".to_owned(),
        ];
        let available = *AVAILABLE.get_or_init(|| {
            ade_session::SshHost::new("localhost")
                .with_extra_args(args.clone())
                .run(&["true".to_owned()])
                .is_ok_and(|output| output.success())
        });
        if !available {
            eprintln!("skipping: no loopback ssh");
            return None;
        }
        Some(args)
    }

    /// Kills whatever daemon a test caused to be started. One started over ssh
    /// is in its own session with no parent that outlives it — deliberately
    /// unkillable by accident — so the pid file is the only handle on it, and
    /// killing it over the same ssh host it was started on is fitting.
    #[cfg(unix)]
    struct DaemonGuard {
        state_dir: PathBuf,
        host: ade_session::SshHost,
    }

    #[cfg(unix)]
    impl Drop for DaemonGuard {
        fn drop(&mut self) {
            use ade_session::deploy::HostExec as _;

            let Some(pid) = ade_session_daemon::state::StateStore::new(&self.state_dir).read_pid()
            else {
                return;
            };
            let _ = self
                .host
                .run(&["kill".to_owned(), "-9".to_owned(), pid.to_string()]);
        }
    }

    /// Records what it was asked, so a test can assert *which* backend a call
    /// reached — which is the whole question once there is more than one.
    struct FakeBackend {
        label: String,
        /// Every call fails with this, for the host that is down. Settable
        /// after construction, for a host that goes down between passes.
        failure: Mutex<Option<String>>,
        /// Whether this backend has workspaces of its own, i.e. whether it is
        /// daemon-shaped or tmux-shaped.
        workspace_kill: bool,
        sessions: Mutex<Vec<SessionId>>,
        sessions_after_next_list: Mutex<Vec<SessionId>>,
        /// What this backend holds that the registry may not know about, i.e.
        /// what discovery has to find.
        workspaces: Mutex<Vec<BackendWorkspace>>,
        workspaces_after_next_list: Mutex<Option<Vec<BackendWorkspace>>>,
        repository_resolution: Mutex<Option<(PathBuf, PathBuf)>>,
        project_scope_updates: Mutex<
            Vec<(
                String,
                String,
                String,
                Option<String>,
                Option<u64>,
                Option<String>,
            )>,
        >,
        project_scope_update_failure: Mutex<Option<String>>,
        listing_instance_ids: Mutex<Vec<Option<String>>>,
        calls: Mutex<Vec<String>>,
        status: Mutex<Option<Sender<IdentifiedDaemonEvent>>>,
        /// Whether this backend's host is behind the client, as a hash
        /// comparison would have found — and who asked to be told when that
        /// moves. The two halves of what the sidebar's upgrade arrow reads.
        stale: Mutex<bool>,
        freshness_observers: Mutex<Vec<crate::DaemonFreshnessObserver>>,
        /// Two backends sharing one identity stand in for aliases of one daemon.
        instance_id: Option<String>,
    }

    impl FakeBackend {
        fn new(label: &str) -> Self {
            Self {
                label: label.to_owned(),
                failure: Mutex::new(None),
                workspace_kill: true,
                sessions: Mutex::new(Vec::new()),
                sessions_after_next_list: Mutex::new(Vec::new()),
                workspaces: Mutex::new(Vec::new()),
                workspaces_after_next_list: Mutex::new(None),
                repository_resolution: Mutex::new(None),
                project_scope_updates: Mutex::new(Vec::new()),
                project_scope_update_failure: Mutex::new(None),
                listing_instance_ids: Mutex::new(Vec::new()),
                calls: Mutex::new(Vec::new()),
                status: Mutex::new(None),
                stale: Mutex::new(false),
                freshness_observers: Mutex::new(Vec::new()),
                instance_id: None,
            }
        }

        /// A host already found to be running an older daemon than this client
        /// would deploy. Silent, because at construction nobody is listening.
        fn behind(self) -> Self {
            *self.stale.lock().unwrap() = true;
            self
        }

        fn identified(self, instance_id: &str) -> Self {
            Self {
                instance_id: Some(instance_id.to_owned()),
                ..self
            }
        }

        /// A verdict landing later, the way a probe on a background thread
        /// records one — recorded first, announced second.
        fn note_freshness(&self, stale: bool) {
            *self.stale.lock().unwrap() = stale;
            let observers = self.freshness_observers.lock().unwrap().clone();
            for observer in observers {
                observer();
            }
        }

        /// Workspaces this backend already holds, as a daemon nobody on this
        /// machine has talked to would.
        fn holding(self, workspaces: Vec<BackendWorkspace>) -> Self {
            *self.workspaces.lock().unwrap() = workspaces;
            self
        }

        fn resolving_repository(
            self,
            repository_path: impl Into<PathBuf>,
            project_identity_path: impl Into<PathBuf>,
        ) -> Self {
            *self.repository_resolution.lock().unwrap() =
                Some((repository_path.into(), project_identity_path.into()));
            self
        }

        fn set_repository_resolution(
            &self,
            repository_path: impl Into<PathBuf>,
            project_identity_path: impl Into<PathBuf>,
        ) {
            *self.repository_resolution.lock().unwrap() =
                Some((repository_path.into(), project_identity_path.into()));
        }

        fn hold(&self, workspace: BackendWorkspace) {
            self.workspaces.lock().unwrap().push(workspace);
        }

        fn replace_workspaces_after_next_list(&self, workspaces: Vec<BackendWorkspace>) {
            *self.workspaces_after_next_list.lock().unwrap() = Some(workspaces);
        }

        fn listing_identities(self, identities: Vec<Option<&str>>) -> Self {
            *self.listing_instance_ids.lock().unwrap() = identities
                .into_iter()
                .map(|identity| identity.map(str::to_owned))
                .collect();
            self
        }

        fn failing(label: &str, message: &str) -> Self {
            Self {
                failure: Mutex::new(Some(message.to_owned())),
                ..Self::new(label)
            }
        }

        /// A host that was answering and stops, as a dropped ssh connection
        /// does between two refreshes.
        fn goes_down(&self, message: &str) {
            *self.failure.lock().unwrap() = Some(message.to_owned());
        }

        /// tmux-shaped: sessions, but nothing a workspace kill could take.
        fn without_workspace_kill(self) -> Self {
            Self {
                workspace_kill: false,
                ..self
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn project_scope_updates(
            &self,
        ) -> Vec<(
            String,
            String,
            String,
            Option<String>,
            Option<u64>,
            Option<String>,
        )> {
            self.project_scope_updates.lock().unwrap().clone()
        }

        fn fail_next_project_scope_update(&self, message: &str) {
            *self.project_scope_update_failure.lock().unwrap() = Some(message.to_owned());
        }

        fn remove_session(&self, id: &str) {
            self.sessions
                .lock()
                .unwrap()
                .retain(|session| session.as_str() != id);
        }

        /// Makes a session absent from one listing but present for the probe
        /// immediately after it, reproducing another client reviving a dead
        /// workspace while cleanup is deciding what it may destroy.
        fn revive_after_next_list(&self, id: String) {
            self.sessions_after_next_list
                .lock()
                .unwrap()
                .push(SessionId::from(id));
        }

        /// Pushes an event to whoever subscribed, standing in for a daemon.
        fn push(&self, event: StatusEvent) {
            if let Some(sender) = self.status.lock().unwrap().as_ref() {
                smol::block_on(sender.send(IdentifiedDaemonEvent {
                    daemon_id: self.instance_id.clone(),
                    event: DaemonEvent::Session(event),
                }))
                .expect("the merged stream is listening");
            }
        }

        /// The layout half of [`Self::push`], for the fanout's own test.
        fn push_layout(&self, event: LayoutEvent) {
            if let Some(sender) = self.status.lock().unwrap().as_ref() {
                smol::block_on(sender.send(IdentifiedDaemonEvent {
                    daemon_id: self.instance_id.clone(),
                    event: DaemonEvent::Layout(event),
                }))
                .expect("the merged stream is listening");
            }
        }

        fn push_workspace_changed(&self, event: LayoutEvent) {
            if let Some(sender) = self.status.lock().unwrap().as_ref() {
                smol::block_on(sender.send(IdentifiedDaemonEvent {
                    daemon_id: self.instance_id.clone(),
                    event: DaemonEvent::WorkspaceChanged(event),
                }))
                .expect("the merged stream is listening");
            }
        }

        fn push_workspace_reset(&self, event: LayoutEvent) {
            if let Some(sender) = self.status.lock().unwrap().as_ref() {
                smol::block_on(sender.send(IdentifiedDaemonEvent {
                    daemon_id: self.instance_id.clone(),
                    event: DaemonEvent::WorkspaceReset(event),
                }))
                .expect("the merged stream is listening");
            }
        }

        /// A workspace another client killed, as the daemon announces it.
        fn push_workspace_removed(&self, workspace_id: &str) {
            if let Some(sender) = self.status.lock().unwrap().as_ref() {
                smol::block_on(sender.send(IdentifiedDaemonEvent {
                    daemon_id: self.instance_id.clone(),
                    event: DaemonEvent::WorkspaceRemoved {
                        workspace_id: workspace_id.to_owned(),
                    },
                }))
                .expect("the merged stream is listening");
            }
        }

        fn record(&self, call: impl Into<String>) -> Result<()> {
            self.calls.lock().unwrap().push(call.into());
            match self.failure.lock().unwrap().as_ref() {
                Some(message) => bail!("{message}"),
                None => Ok(()),
            }
        }
    }

    /// How a recorded call spells the identity it was fenced to. Nothing for
    /// the permissive case, so the calls a legacy row makes read as before.
    fn fence(expected: Option<&str>) -> String {
        expected.map(|id| format!("|{id}")).unwrap_or_default()
    }

    impl SessionBackend for FakeBackend {
        fn create(&self, spec: &SessionSpec, expected: Option<&str>) -> Result<SessionId> {
            self.record(format!("create:{}{}", spec.id, fence(expected)))?;
            self.sessions.lock().unwrap().push(spec.id.clone());
            Ok(spec.id.clone())
        }

        fn list(&self) -> Result<Vec<crate::SessionInfo>> {
            self.record("list")?;
            let listed = self.sessions.lock().unwrap().clone();
            let revived = std::mem::take(&mut *self.sessions_after_next_list.lock().unwrap());
            if !revived.is_empty() {
                let mut sessions = self.sessions.lock().unwrap();
                for session in revived {
                    if !sessions.contains(&session) {
                        sessions.push(session);
                    }
                }
            }
            Ok(listed
                .iter()
                .map(|id| crate::SessionInfo { id: id.clone() })
                .collect())
        }

        fn list_workspaces(&self) -> Result<Vec<BackendWorkspace>> {
            self.record("list_workspaces")?;
            let mut workspaces = self.workspaces.lock().unwrap();
            let listed = workspaces.clone();
            if let Some(next) = self.workspaces_after_next_list.lock().unwrap().take() {
                *workspaces = next;
            }
            Ok(listed)
        }

        fn list_workspaces_identified(&self) -> Result<Identified<BackendWorkspace>> {
            let items = self.list_workspaces()?;
            let daemon_id = {
                let mut identities = self.listing_instance_ids.lock().unwrap();
                if identities.is_empty() {
                    self.instance_id.clone()
                } else {
                    identities.remove(0)
                }
            };
            Ok(Identified { daemon_id, items })
        }

        fn resolve_repository(&self, path: &Path) -> Result<(PathBuf, PathBuf)> {
            self.record(format!("resolve:{}", path.display()))?;
            self.repository_resolution
                .lock()
                .unwrap()
                .clone()
                .context("the fake repository resolution was not configured")
        }

        fn update_workspace_project_scope(
            &self,
            workspace_id: &str,
            project_id: &str,
            project_identity: &str,
            project_root: Option<&str>,
            minimum_scope_rev: Option<u64>,
            expected_daemon_id: Option<&str>,
        ) -> Result<Option<u64>> {
            self.project_scope_updates.lock().unwrap().push((
                workspace_id.to_owned(),
                project_id.to_owned(),
                project_identity.to_owned(),
                project_root.map(str::to_owned),
                minimum_scope_rev,
                expected_daemon_id.map(str::to_owned),
            ));
            if let Some(message) = self.project_scope_update_failure.lock().unwrap().take() {
                bail!(message);
            }
            Ok(None)
        }

        fn exists(&self, id: &SessionId, expected: Option<&str>) -> Result<bool> {
            self.record(format!("exists:{id}{}", fence(expected)))?;
            Ok(self.sessions.lock().unwrap().contains(id))
        }

        fn attach(&self, spec: &SessionSpec, expected: Option<&str>) -> Result<Attached> {
            self.record(format!("attach:{}{}", spec.id, fence(expected)))?;
            Ok(Attached {
                session_id: spec.id.to_string(),
                daemon_id: self.instance_id.clone(),
                argv: vec![format!("{}-attach", self.label), spec.id.to_string()],
            })
        }

        fn detach(&self, id: &SessionId) -> Result<()> {
            self.record(format!("detach:{id}"))
        }

        fn kill(&self, id: &SessionId, expected: Option<&str>) -> Result<()> {
            self.record(format!("kill:{id}{}", fence(expected)))?;
            self.sessions
                .lock()
                .unwrap()
                .retain(|session| session != id);
            Ok(())
        }

        fn reset_workspace_sessions(
            &self,
            id: &SessionId,
            directory: &Path,
            expected: Option<&str>,
        ) -> Result<()> {
            self.record(format!(
                "reset:{id}:{}{}",
                directory.display(),
                fence(expected)
            ))?;
            self.kill(id, expected)
        }

        fn kill_workspace(&self, workspace_id: &str, expected: Option<&str>) -> Result<()> {
            self.record(format!("kill_workspace:{workspace_id}{}", fence(expected)))?;
            if !self.workspace_kill {
                bail!("this session backend has no workspaces of its own to kill");
            }
            self.sessions
                .lock()
                .unwrap()
                .retain(|session| session.as_str() != workspace_id);
            self.workspaces
                .lock()
                .unwrap()
                .retain(|workspace| workspace.id != workspace_id);
            Ok(())
        }

        fn status_delivery(&self) -> StatusDelivery {
            StatusDelivery::Push
        }

        fn subscribe_events(&self) -> Result<Receiver<IdentifiedDaemonEvent>> {
            self.calls.lock().unwrap().push("subscribe".to_owned());
            let (sender, receiver) = smol::channel::unbounded();
            *self.status.lock().unwrap() = Some(sender);
            Ok(receiver)
        }

        /// Deliberately unrecorded in `calls`: the sidebar asks this on every
        /// render, and a call log that grew with the frame rate would make
        /// every "which backend was reached" assertion in this file useless.
        fn daemon_stale(&self) -> bool {
            *self.stale.lock().unwrap()
        }

        fn observe_daemon_freshness(&self, observer: crate::DaemonFreshnessObserver) {
            self.freshness_observers.lock().unwrap().push(observer);
        }

        fn instance_id(&self) -> Option<String> {
            self.instance_id.clone()
        }
    }

    /// The upgrade arrow is drawn from a recorded hash verdict and nothing
    /// else.
    ///
    /// The bug this pins is the arrow appearing on every remote row: a host
    /// having a daemon, or a backend, or a name, says nothing about whether an
    /// update exists. Only a comparison against the binary this client would
    /// deploy does, and a host nobody has compared answers "no".
    #[gpui::test]
    async fn test_the_upgrade_arrow_needs_a_recorded_verdict() {
        let registry =
            AdeWorkspaceRegistry::open_test_db("test_the_upgrade_arrow_needs_a_recorded_verdict")
                .await;
        let local = Arc::new(FakeBackend::new("local"));
        // Reachable, connected, holding sessions — and up to date.
        let current = Arc::new(
            FakeBackend::new("current-box").holding(vec![BackendWorkspace {
                id: "ade-proj-0a1b2c".to_owned(),
                name: "ade-proj-0a1b2c".to_owned(),
                project_id: None,
                project_identity: None,
                project_root: "/home/kingii/proj".to_owned(),
                project_scope_rev: 0,
                created_at: 1_700_000_000,
            }]),
        );
        let behind = Arc::new(FakeBackend::new("behind-box").behind());
        let service = WorkspaceLifecycleService::with_backend(registry, local.clone())
            .with_backend_for_host("current-box", current.clone())
            .with_backend_for_host("behind-box", behind);

        assert!(service.host_daemon_stale("behind-box"));
        assert!(
            !service.host_daemon_stale("current-box"),
            "a busy, connected daemon is not an update"
        );
        // Nothing has contacted it, so nothing is known, so nothing is claimed.
        assert!(!service.host_daemon_stale("never-touched"));
        // And asking did not make a backend for it: drawing a row must never
        // register an ssh connection as a side effect.
        assert!(
            !service
                .backends
                .lock()
                .unwrap()
                .contains_key(&Some("never-touched".to_owned()))
        );
        // Nor did any of it reach a backend's operations.
        assert!(local.calls().is_empty(), "{:?}", local.calls());
        assert!(current.calls().is_empty(), "{:?}", current.calls());
    }

    /// A verdict lands on a background thread and only a render reads it, so a
    /// sidebar that is not told keeps drawing the last answer it happened to
    /// catch — an arrow that outlives the upgrade, or one that never arrives.
    #[gpui::test]
    async fn test_a_freshness_change_reaches_every_watcher() {
        let registry =
            AdeWorkspaceRegistry::open_test_db("test_a_freshness_change_reaches_every_watcher")
                .await;
        let behind = Arc::new(FakeBackend::new("behind-box").behind());
        let service =
            WorkspaceLifecycleService::with_backend(registry, Arc::new(FakeBackend::new("local")))
                .with_backend_for_host("behind-box", behind.clone());

        // Two windows, and the second must not silence the first.
        let one = service.watch_daemon_freshness();
        let two = service.watch_daemon_freshness();
        assert!(one.is_empty() && two.is_empty(), "nothing has moved yet");

        behind.note_freshness(false);
        assert_eq!(one.recv().await.ok(), Some(()));
        assert_eq!(two.recv().await.ok(), Some(()));
        assert!(!service.host_daemon_stale("behind-box"));

        // A closed window's watcher goes with it, rather than being kept for
        // the life of the process.
        drop(one);
        drop(two);
        behind.note_freshness(true);
        assert!(
            service.freshness_watchers.lock().unwrap().is_empty(),
            "a dropped receiver's sender was kept"
        );
        assert!(service.host_daemon_stale("behind-box"));
    }

    #[gpui::test]
    async fn test_record_attached_session_needs_no_backend() {
        let service = backendless_service("test_record_attached_session_needs_no_backend").await;

        // A workspace the registry knows about that has never had a session:
        // exactly what a killed-then-reselected workspace looks like.
        let workspace = AdeWorkspace::new("main", "project-a", "/repos/zed");
        service
            .registry()
            .create_workspace(workspace.clone())
            .await
            .unwrap();
        assert!(workspace.terminal_session_id.is_none());
        assert_eq!(workspace.status, WorkspaceStatus::Creating);

        // The pane has already attached and succeeded, so this only writes
        // down what that produced — no backend is consulted, which is why the
        // failing backend never comes up.
        let recorded = service
            .record_attached_session(&workspace.id, Some("daemon-a".to_owned()))
            .await
            .unwrap();
        assert_eq!(
            recorded.terminal_session_id.as_deref(),
            Some(workspace.tmux_session_name().as_str())
        );
        assert_eq!(recorded.status, WorkspaceStatus::Running);
        assert_eq!(recorded.daemon_id.as_deref(), Some("daemon-a"));

        // And it is the stored row that moved, not just the returned copy.
        let stored = service
            .registry()
            .get_workspace(workspace.id.clone())
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.terminal_session_id.as_deref(),
            Some(workspace.tmux_session_name().as_str())
        );
        assert_eq!(stored.status, WorkspaceStatus::Running);
        assert_eq!(stored.daemon_id.as_deref(), Some("daemon-a"));

        // Idempotent: a second attach to the same session changes nothing.
        let again = service
            .record_attached_session(&workspace.id, None)
            .await
            .unwrap();
        assert_eq!(again, stored);
    }

    #[gpui::test]
    async fn test_unknown_workspace_id_is_an_error() {
        let service = backendless_service("test_unknown_workspace_id_is_an_error").await;
        let id = WorkspaceId::new();
        assert!(service.open_workspace(&id).await.is_err());
        assert!(service.stop_workspace(&id).await.is_err());
        assert!(service.kill_workspace_session(&id).await.is_err());
        assert!(service.recreate_session(&id).await.is_err());
    }
}
