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
//!   [`WorkspaceLifecycleService::kill_workspace_session`] or
//!   [`WorkspaceLifecycleService::kill_workspace`] by name.
//! - **The daemon mints the identity, and this layer only caches it.**
//!   [`WorkspaceLifecycleService::create_workspace`] asks the host for a record
//!   before there is a row, and nothing here ever derives an id. A workspace
//!   with no session is a normal state, not one to repair.
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
    DaemonUpgradeOutcome, Outdated, SessionBackend, SessionId, SessionSpec, StatusDelivery,
    StatusEvent, WorkspaceEvent, WorkspaceId, WorkspaceLayout, WorkspaceStatus, now_whole_seconds,
    project_id_from_path,
};
use ade_session::LayoutDoc;
use anyhow::{Context as _, Result, bail};
use smol::channel::{Receiver, Sender};
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
    /// The workspace has no live session: a row whose first terminal has not
    /// been opened, or one whose shells have all exited. A normal state, not a
    /// failure — opening it makes a session, like any other open.
    Dead,
    /// The row is not linked to a daemon record, so there is nothing to ask
    /// about — a row whose record was killed out from under it.
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
/// sqlite. Its identity is host plus wire id — host included because wire ids
/// are only unique within a host.
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

    /// The id the host's daemon knows this workspace by. `None` only for a row
    /// whose record was killed.
    pub fn wire_id(&self) -> Option<&str> {
        match self {
            Self::Persisted(workspace, _) => workspace.daemon_workspace_id(),
            Self::Discovered { workspace, .. } => Some(&workspace.id),
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
/// between the listing the user clicked and the confirmation, and the caller
/// drops the entry rather than reporting a broken host.
#[derive(Debug)]
pub struct WorkspaceGone {
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

/// **One daemon, however its host is spelled.**
///
/// A destination may be typed several ways — an IP, a hostname, an ssh alias —
/// and each spelling gets a backend of its own. Wire ids are unique per
/// *daemon*, not per spelling, so everything that must not happen twice for one
/// workspace is keyed by this rather than by the string the user typed: the
/// decision lock, the discovery cache, the wire ids a row already uses, the
/// reconcile pass, and the confirmed row in sqlite.
///
/// [`DaemonKey::Host`] is the identity of a daemon that has not said which it
/// is — one too old for [`ade_session::HelloAck::instance_id`], or one no
/// backend has reached yet. It is exactly the identity this layer used before
/// the field existed, so nothing changes for such a host.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DaemonKey {
    Instance(String),
    Host(Option<String>),
}

/// What one reconciliation pass found.
#[derive(Debug, Default)]
pub struct Reconciled {
    /// Every workspace that was reconciled, hosts that failed included: the
    /// rows this client uses first, in recency order, then what the hosts hold
    /// that it does not.
    pub entries: Vec<WorkspaceEntry>,
    /// `(host, message)` for each host that could not be reached — the string
    /// is the host as the registry spells it, or `local` for this machine.
    /// Surfaced beside the rows, never in place of them.
    pub host_errors: Vec<(String, String)>,
    /// Which daemon each host spelling this pass contacted turned out to name.
    ///
    /// For the views: two spellings of one daemon must not select, dedupe or
    /// group as two hosts, and only the pass knows which spellings those are.
    /// A spelling that is absent has not named a daemon — see
    /// [`DaemonKey::Host`].
    pub daemons: HashMap<Option<String>, String>,
}

impl Reconciled {
    /// The identity of one host spelling, as this pass resolved it.
    pub fn daemon_of(&self, host: Option<&str>) -> DaemonKey {
        match self.daemons.get(&host.map(str::to_owned)) {
            Some(instance) => DaemonKey::Instance(instance.clone()),
            None => DaemonKey::Host(host.map(str::to_owned)),
        }
    }
}

/// One spelling's share of a reconcile pass, held until every spelling has
/// been tried — see [`WorkspaceLifecycleService::reconcile`].
struct HostOutcome {
    /// The daemon this spelling names, as far as this pass got to know.
    daemon: DaemonKey,
    host: Option<String>,
    /// Whether the host was asked at all; a host with nothing to probe and
    /// nothing to discover is not contacted, and claims nothing.
    contacting: bool,
    /// `None` when the daemon could not be asked.
    pass: Option<HostPass>,
    /// The live session ids behind it; `None` when they could not be listed,
    /// which is what makes this host's entries read [`SessionState::Unknown`].
    live: Option<HashSet<SessionId>>,
}

/// What one host's locked reconcile pass produced.
struct HostPass {
    /// Which daemon answered, resolved *after* the listing — the handshake
    /// that names it is the one this pass's first request opened.
    daemon: DaemonKey,
    /// The daemon's confirmed rows as they stand *after* the pass's writes,
    /// under every spelling known to reach it.
    survivors: Vec<AdeWorkspace>,
    /// The listing those rows were judged against.
    held: Vec<BackendWorkspace>,
    /// §8.5: the answering daemon's ledger is read-only, so `held` is a
    /// statement about what exists and none at all about what does not.
    degraded: bool,
}

/// The stable order of the discovered half of a pass: `(host, created_at, id)`.
fn discovered_order(entry: &WorkspaceEntry) -> (Option<&str>, u64, Option<&str>) {
    match entry {
        WorkspaceEntry::Discovered {
            remote_host,
            workspace,
            ..
        } => (
            remote_host.as_deref(),
            workspace.created_at,
            Some(workspace.id.as_str()),
        ),
        WorkspaceEntry::Persisted(..) => (entry.remote_host(), 0, entry.wire_id()),
    }
}

/// The row a reattach picks out of several candidates.
///
/// A tie keeps the earlier candidate, so the registry's own `last_opened_at
/// DESC` order decides it.
fn most_recently_opened<'a>(
    rows: impl Iterator<Item = &'a AdeWorkspace>,
) -> Option<&'a AdeWorkspace> {
    rows.fold(None, |best: Option<&AdeWorkspace>, row| match best {
        Some(best) if best.last_opened_at >= row.last_opened_at => Some(best),
        _ => Some(row),
    })
}

/// How a host reads in an error line. The local backend has no name of its own.
fn host_label(host: Option<&str>) -> String {
    host.unwrap_or("local").to_owned()
}

/// The id a workspace goes on the wire under.
///
/// A cache row with no daemon record cannot be opened, renamed, killed or
/// attached to — every one of those is a call to the host about a workspace it
/// would have to be told the name of. Absence is a bug at these call sites, not
/// a state to work around, so it comes back as an error rather than a string
/// this client made up.
fn wire_id(workspace: &AdeWorkspace) -> Result<&str> {
    workspace.daemon_workspace_id().with_context(|| {
        format!(
            "workspace {} is not linked to a daemon record",
            workspace.id
        )
    })
}

/// The registry row for a daemon record this client has just started using.
///
/// **`terminal_session_id` is the whole point.** It carries the backend's id,
/// which is what [`AdeWorkspace::daemon_workspace_id`] returns once recorded —
/// so this row addresses the workspace it was made from, rather than a freshly
/// derived name the daemon has never heard of. The `id` is minted here because
/// it is this client's own key and nothing else refers to it.
///
/// **`created_at` is the backend's, `last_opened_at` is now**: the record is as
/// old as the daemon says, and the client is opening it this second.
///
/// `branch` is left unset: the backend records a root, not a checkout state, and
/// guessing it from the path would be a claim nothing verified.
fn row_for_record(
    workspace: &BackendWorkspace,
    host: Option<&str>,
    daemon_id: Option<String>,
    now: OffsetDateTime,
) -> AdeWorkspace {
    let repository_path = PathBuf::from(&workspace.project_root);
    let project_id = project_id_from_path(&repository_path);
    // Whole seconds, like everything else the registry stores; a backend that
    // reports a time no calendar has is given this client's clock rather than
    // failing over a timestamp.
    let created_at = i64::try_from(workspace.created_at)
        .ok()
        .and_then(|seconds| OffsetDateTime::from_unix_timestamp(seconds).ok())
        .unwrap_or(now);
    AdeWorkspace {
        id: WorkspaceId::new(),
        name: display_name_for(workspace, &project_id),
        project_id,
        repository_path,
        branch: None,
        remote_host: host.map(str::to_owned),
        remote_workspace_path: None,
        terminal_session_id: Some(workspace.id.clone()),
        daemon_id,
        // Nothing has probed its sessions yet, and this is the status a
        // workspace nobody is attached to has. The probe that follows corrects
        // it in the same pass.
        status: WorkspaceStatus::Disconnected,
        created_at,
        last_opened_at: now,
    }
}

/// What to call a discovered workspace: the checkout it is rooted at, unless
/// the backend's name is one a *person* chose.
///
/// A workspace this client created before the daemon minted ids carries its own
/// id as its name, and showing that in the sidebar would be showing the user an
/// id where a project name belongs. Any other name was typed by somebody and
/// must survive verbatim; losing a rename is worse than showing a slug.
/// Legacy-only: every record minted since carries the name the row was created
/// with. An empty name is nobody's, so it falls back the same way.
pub(crate) fn display_name_for(record: &BackendWorkspace, project_id: &str) -> String {
    if is_machine_named(record) || record.name.is_empty() {
        project_id.to_owned()
    } else {
        record.name.clone()
    }
}

/// Whether a daemon record is one whose *name is its id* — the legacy shape,
/// where the name this client sees is the machine string the daemon was told
/// to mint the workspace under.
///
/// The predicate used to be the id's *shape* (`ade-<slug>-<six hex>`), which
/// also ate a legitimate name of that shape: a workspace somebody called
/// `ade-api-00ff11` has a uuid id and its name is as real as any other.
fn is_machine_named(record: &BackendWorkspace) -> bool {
    record.name == record.id
}

/// Whether a cache row still names a record the host holds.
fn is_backed(row: &AdeWorkspace, held: &[BackendWorkspace]) -> bool {
    row.terminal_session_id
        .as_deref()
        .is_some_and(|id| held.iter().any(|record| record.id == id))
}

/// Which of two cache rows for one daemon record survives — see
/// [`WorkspaceLifecycleService::collapse_duplicate_rows`]. Deterministic in
/// every process: metadata a mirror cannot re-derive, then recency, then the
/// lower uuid.
fn beats(row: &AdeWorkspace, other: &AdeWorkspace) -> bool {
    (
        row.branch.is_some(),
        row.last_opened_at,
        std::cmp::Reverse(row.id.as_str()),
    ) > (
        other.branch.is_some(),
        other.last_opened_at,
        std::cmp::Reverse(other.id.as_str()),
    )
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
    /// One decision lock per **daemon**, created on first use.
    ///
    /// Held over the registry read and the write that follows it — the whole
    /// decision two windows on one project would otherwise both take, minting
    /// two workspaces — and over the listing a reconcile pass judges rows by.
    ///
    /// **Not reentrant.** Nothing running under it may call [`Self::create_workspace`],
    /// [`Self::adopt_or_create_workspace`] or [`Self::ensure_host_workspaces`];
    /// the unlocked [`Self::create_workspace_locked`] is what those callers take.
    /// Keyed by [`DaemonKey`], so two spellings of one host take one lock —
    /// which is what stops two aliases from each minting a row for the same
    /// record. A spelling whose daemon has not been reached yet still locks
    /// under itself; the entry it leaves behind is harmless, and the identity
    /// firms up on the first connect.
    decisions: Mutex<HashMap<DaemonKey, Arc<smol::lock::Mutex<()>>>>,
    /// Each host's **last successful** workspace listing.
    ///
    /// A daemon request that fails is reset and retried by the next one, so one
    /// failure is not a disconnect: the sidebar keeps drawing what the host last
    /// held, marked [`SessionState::Unknown`], rather than blinking the host's
    /// workspaces away and back. Replaced only by another successful listing,
    /// and dropped only when the backend itself is evicted.
    ///
    /// Keyed by daemon, like the decision lock above and for the same reason:
    /// one daemon reached through two spellings holds one snapshot.
    discoveries: Mutex<HashMap<DaemonKey, Vec<BackendWorkspace>>>,
}

impl WorkspaceLifecycleService {
    /// The service against this machine's default backend.
    ///
    /// The single place the backend is chosen. It named [`crate::TmuxBackend`]
    /// until 2026-08-03 and now names [`DaemonBackend`]; tmux stays compiled
    /// and tested behind [`Self::with_backend`] until the operator has accepted
    /// the daemon on a desktop build, and is deleted after that.
    pub fn new(registry: AdeWorkspaceRegistry) -> Self {
        Self::with_backend(registry, Arc::new(DaemonBackend::new()))
    }

    /// The service against a specific backend for *this machine*. Remote hosts
    /// still get theirs from [`Self::backend_for_host`].
    pub fn with_backend(registry: AdeWorkspaceRegistry, backend: Arc<dyn SessionBackend>) -> Self {
        let freshness_watchers = Arc::new(Mutex::new(Vec::new()));
        let events = Arc::new(EventFanout::default());
        *events.announce.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(freshness_announcer(&freshness_watchers));
        Self {
            backends: Mutex::new(HashMap::from([(None, backend)])),
            freshness_watchers,
            registry,
            events,
            pumped: Mutex::new(HashSet::new()),
            pump_generation: Arc::new(AtomicU64::new(0)),
            status_errors: Mutex::new(HashMap::new()),
            decisions: Mutex::new(HashMap::new()),
            discoveries: Mutex::new(HashMap::new()),
        }
    }

    /// The decision lock for the daemon behind one host — see
    /// [`Self::decisions`].
    fn decision_lock(&self, host: Option<&str>) -> Arc<smol::lock::Mutex<()>> {
        self.decision_lock_for(&self.daemon_key(host))
    }

    fn decision_lock_for(&self, daemon: &DaemonKey) -> Arc<smol::lock::Mutex<()>> {
        self.decisions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .entry(daemon.clone())
            .or_default()
            .clone()
    }

    /// **Which daemon a host spelling names**, as far as anything already
    /// knows: the id its backend was told at the last handshake, or the
    /// spelling itself.
    ///
    /// Contacts nothing, so a spelling whose backend has never connected
    /// answers [`DaemonKey::Host`] — today's behaviour, and self-correcting:
    /// the first pass through that spelling connects, and every later one
    /// resolves it to the daemon.
    fn daemon_key(&self, host: Option<&str>) -> DaemonKey {
        self.backends
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&host.map(str::to_owned))
            .and_then(|backend| backend.instance_id())
            .map_or_else(
                || DaemonKey::Host(host.map(str::to_owned)),
                DaemonKey::Instance,
            )
    }

    /// The id to record on a row created against `host` — see
    /// [`AdeWorkspace::daemon_id`].
    fn instance_of(&self, host: Option<&str>) -> Option<String> {
        match self.daemon_key(host) {
            DaemonKey::Instance(id) => Some(id),
            DaemonKey::Host(_) => None,
        }
    }

    /// Whether a row belongs to `daemon`: its host spelling names it.
    fn row_is_on(&self, row: &AdeWorkspace, daemon: &DaemonKey) -> bool {
        &self.daemon_key(row.remote_host.as_deref()) == daemon
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
        // The one explicit eviction there is: with the backend gone, its last
        // listing is no longer a fact about anything.
        self.discoveries
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

    /// Creates the workspace **on its host** and caches the record as a row.
    ///
    /// **The daemon goes first, and the row is the copy.** The record is what a
    /// panel row *is*, so its id has to exist before there is a row to key by
    /// it; a row written first would be a workspace only this machine believes
    /// in. The record is created empty — no session, no layout — and the first
    /// terminal is a separate create-session into it.
    ///
    /// A host that refuses or cannot be reached therefore leaves **no row**.
    /// There is no offline row creation: the id is the host's to mint.
    ///
    /// `remote_host` is `None` for this machine; otherwise `repository_path` is
    /// read as a path **on that host**, which is why nothing here touches the
    /// local filesystem to check it.
    ///
    /// Under the host's decision lock, so the gap between the record and the
    /// row cannot be read by a concurrent adopt — which would see a record no
    /// row points at and mirror a duplicate for it.
    pub async fn create_workspace(
        &self,
        name: impl Into<String>,
        project_id: impl Into<String>,
        repository_path: impl Into<PathBuf>,
        branch: Option<String>,
        remote_host: Option<String>,
    ) -> Result<AdeWorkspace> {
        let name = name.into();
        let repository_path = repository_path.into();
        let lock = self.decision_lock(remote_host.as_deref());
        let _creating = lock.lock().await;
        if let Some(recovered) = self
            .recover_created_record(&name, &repository_path, remote_host.as_deref())
            .await?
        {
            return Ok(recovered);
        }
        self.create_workspace_locked(name, project_id, repository_path, branch, remote_host)
            .await
    }

    /// The record a previous attempt at *this* create already minted, if the
    /// registry write is what failed.
    ///
    /// Daemon-first creation is two writes, and the second one can fail: the
    /// host then holds a record no row points at, and a second Confirm would
    /// mint a duplicate beside it. Same host, same root and the same requested
    /// name is a retry of the same request; a different name on that root is a
    /// second workspace and is left alone.
    ///
    /// Caller holds the host's decision lock, so the listing here cannot be
    /// overtaken by another window's create.
    async fn recover_created_record(
        &self,
        name: &str,
        root: &Path,
        host: Option<&str>,
    ) -> Result<Option<AdeWorkspace>> {
        let listed = self
            .backend_for_host(host)?
            .list_workspaces()
            .with_context(|| format!("listing the workspaces on {}", host_label(host)))?;
        let used = self.wire_ids_in_use(host)?;
        let Some(record) = listed
            .workspaces
            .into_iter()
            .filter(|record| !used.contains(&record.id))
            .find(|record| record.name == name && Path::new(&record.project_root) == root)
        else {
            return Ok(None);
        };
        log::info!(
            "reusing workspace record {} on {}: it matches this create and no row uses it",
            record.id,
            host_label(host)
        );
        self.persist_on_open(&record, host).await.map(Some)
    }

    /// The wire ids of `host`'s **daemon** that a confirmed row already
    /// addresses — every spelling of it, since a wire id is the daemon's and
    /// not the spelling's.
    fn wire_ids_in_use(&self, host: Option<&str>) -> Result<HashSet<String>> {
        let daemon = self.daemon_key(host);
        Ok(self
            .registry
            .list_workspaces()?
            .into_iter()
            .filter(|row| self.row_is_on(row, &daemon))
            .filter_map(|row| row.terminal_session_id)
            .collect())
    }

    /// [`Self::create_workspace`] for a caller that already holds the host's
    /// decision lock.
    async fn create_workspace_locked(
        &self,
        name: impl Into<String>,
        project_id: impl Into<String>,
        repository_path: impl Into<PathBuf>,
        branch: Option<String>,
        remote_host: Option<String>,
    ) -> Result<AdeWorkspace> {
        let mut workspace = AdeWorkspace::new(name, project_id, repository_path);
        workspace.branch = branch;
        workspace.remote_host = remote_host;

        let backend = self.backend_for(&workspace)?;
        let record = backend
            .create_workspace(&workspace.repository_path, Some(&workspace.name))
            .with_context(|| {
                format!(
                    "creating workspace {} on {}",
                    workspace.name,
                    host_label(workspace.remote_host.as_deref())
                )
            })?;
        workspace.terminal_session_id = Some(record.id.clone());
        // Read after the create, which is the request that opened the
        // handshake this comes out of.
        workspace.daemon_id = self.instance_of(workspace.remote_host.as_deref());
        // The state of a workspace with no session, which is what it has until
        // its first terminal opens.
        workspace.status = WorkspaceStatus::Disconnected;

        if let Err(error) = self
            .registry
            .create_workspace(workspace.clone())
            .await
            .context("recording the new workspace")
        {
            // Nothing points at that record now, and at generation 2 the create
            // already spawned its first login shell — so leaving it is an
            // orphaned live shell, not a stray row. Compensated on the same
            // backend that made it, while the caller's decision lock still
            // holds, so no other decision sees the half-made workspace. A
            // degraded `persisted:false` kill still killed it.
            if let Err(cleanup) = backend.kill_workspace(&record.id) {
                log::error!(
                    "workspace {} on {} was created but not recorded, and could not be \
                     cleaned up: {cleanup:#}",
                    record.id,
                    host_label(workspace.remote_host.as_deref())
                );
            }
            return Err(error);
        }
        Ok(workspace)
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
    /// UI can name what died, and nothing here recreates it. `workspace` is
    /// updated in place so the caller's copy does not go stale.
    pub async fn probe(&self, workspace: &mut AdeWorkspace) -> Result<SessionState> {
        // Before the backend is asked for, so a workspace with nothing to probe
        // never reaches for a host — a registry row for a machine that is off
        // must not cost an ssh attempt just to say "never created".
        let Some(session) = workspace.terminal_session_id.clone().map(SessionId::from) else {
            return Ok(SessionState::NeverCreated);
        };
        let alive = self
            .backend_for(workspace)?
            .exists(&session)
            .with_context(|| format!("probing session {session}"))?;
        self.record_probe(workspace, alive).await
    }

    /// Records that the workspace has a session again, without asking the
    /// backend anything.
    ///
    /// For the caller that has just run the attach argv and had it succeed: the
    /// pane is the creator, and this is the registry catching up with what the
    /// pane already did. It is deliberately *not* a probe — probing here would
    /// race the pane's own attach-or-create.
    ///
    /// Only sound after an attach the caller watched succeed. Anything that
    /// merely *hopes* a session exists wants [`Self::probe`].
    pub async fn record_attached_session(&self, id: &WorkspaceId) -> Result<AdeWorkspace> {
        let mut workspace = self.get(id)?;

        // No backend is asked for: this is the registry writing down what the
        // pane already did, and it is as true of a remote host as a local one.
        let session = SessionId::from(wire_id(&workspace)?.to_owned());
        self.adopt_session(&mut workspace, session).await?;
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
    /// **The id never moves**, so nothing has to be re-linked: the sessions, the
    /// stored layout and this row are all keyed by it, and a rename moves a
    /// label. An empty name is refused here as well as by the daemon, since
    /// there is no point in a round trip to be told so.
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
            .rename_workspace(wire_id(&workspace)?, name)
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
            if backend.exists(&session)? {
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
    /// stop syncing.
    ///
    /// **The record first, then the row**, and the row's wire id is never
    /// cleared on the way: a delete that fails leaves a row that still names
    /// the dead record, which is what lets the daemon's removal broadcast — or
    /// the next reconcile sweep — finish the job. Clearing the id first would
    /// strand a ghost no other path can address.
    ///
    /// **A failed kill is a failure**, and the caller keeps its row. There used
    /// to be a session-level fallback here for a backend with no workspace of
    /// its own; it ran on *any* error, so an unreachable host had this method
    /// report which sessions it could not find rather than that nothing was
    /// killed — and it cleared `terminal_session_id` on the way, leaving the row
    /// unable to retry the destructive operation it never performed. The
    /// workspace-less backend is tmux, which nothing reaches any more; the
    /// sessions of a workspace are still killable by name through
    /// [`Self::kill_workspace_session`].
    pub async fn kill_workspace(&self, id: &WorkspaceId) -> Result<AdeWorkspace> {
        let workspace = self.get(id)?;
        let killed = wire_id(&workspace)?.to_owned();
        // Held across the kill as well as the delete: a decision listing taken
        // mid-kill would see the record and persist a row for the workspace
        // this call is destroying.
        let lock = self.decision_lock(workspace.remote_host.as_deref());
        let _deciding = lock.lock().await;
        match self
            .backend_for(&workspace)?
            .kill_workspace(&killed)
            .with_context(|| format!("killing workspace {}", workspace.name))
        {
            Ok(()) => {}
            // The kill happened and only the daemon's ledger did not take it,
            // so the row addresses nothing either way. Worth saying out loud:
            // the workspace may come back on that daemon's next restart.
            Err(error)
                if crate::refusal_code(&error)
                    == Some(ade_session::proto::error_code::PERSIST_FAILED) =>
            {
                log::warn!(
                    "workspace {id} was killed but its daemon could not record it, so it may come \
                     back on the daemon's next restart: {error:#}"
                );
            }
            Err(error) => return Err(error),
        }
        // **The backend accepted it, so the record is gone whatever sqlite
        // says next.** Waiting for the daemon's own broadcast would leave the
        // killed workspace rendered as a discovery for as long as the next
        // listing fails.
        self.evict_discovery(workspace.remote_host.as_deref(), &killed);
        self.registry
            .delete_workspace(id.clone())
            .await
            .with_context(|| format!("dropping the killed workspace's row {id}"))?;
        Ok(workspace)
    }

    /// Drops one record from the host's last-successful listing.
    ///
    /// **Called with the host's decision lock held** — the one lock order in
    /// this file is decision lock, then `discoveries` — so a reconcile pass
    /// cannot republish the listing this is erasing from.
    fn evict_discovery(&self, host: Option<&str>, wire_id: &str) {
        let daemon = self.daemon_key(host);
        if let Some(held) = self
            .discoveries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get_mut(&daemon)
        {
            held.retain(|record| record.id != wire_id);
        }
    }

    /// **A workspace that is gone**, whoever killed it: this client forgets the
    /// record it discovered and the row that used it.
    ///
    /// Serialized on the host's decision lock like every other write, so a
    /// removal racing a persist-on-open is ordered rather than interleaved —
    /// the persist re-lists under the same lock and finds nothing to confirm.
    /// Idempotent: a removal reaches every client, and only one of them has a
    /// row for it.
    ///
    /// **The discovery is evicted under that lock too**, not before it: a
    /// reconcile pass publishes its listing while holding it, so an eviction
    /// taken outside would be overwritten by the pass it raced.
    pub async fn forget_workspace(&self, host: Option<&str>, wire_id: &str) -> Result<()> {
        let lock = self.decision_lock(host);
        let _deciding = lock.lock().await;
        self.evict_discovery(host, wire_id);
        self.registry
            .delete_workspaces_for_record(
                host.map(str::to_owned),
                // The row may be filed under another spelling of the same
                // daemon; the record is gone from all of them.
                self.instance_of(host),
                wire_id.to_owned(),
            )
            .await
            .with_context(|| {
                format!(
                    "dropping the row for removed workspace {wire_id} on {}",
                    host_label(host)
                )
            })
    }

    /// Kills the workspace's session and everything running in it, then
    /// forgets the session name and records the workspace as `stopped`.
    ///
    /// **Destructive and irreversible.** Running agents die with the session
    /// and their scrollback goes with them. The workspace-level kill is
    /// [`Self::kill_workspace`], which this is the fallback for; every UI path
    /// that reaches either must say so in its own label ("Kill workspace", not
    /// "Close"): closing, switching away, and removing all detach instead. The
    /// id is cleared because it no longer names anything.
    pub async fn kill_workspace_session(&self, id: &WorkspaceId) -> Result<AdeWorkspace> {
        let mut workspace = self.get(id)?;

        if let Some(session) = workspace.terminal_session_id.clone().map(SessionId::from) {
            let backend = self.backend_for(&workspace)?;
            // A session that is already gone needs no killing: the outcome the
            // caller asked for is the state the world is in, and the rest of
            // the method — forgetting the name, recording `stopped` — is still
            // owed. Same probe-first shape as `stop_workspace`.
            //
            // `SessionBackend::kill` tolerates a missing session too, which
            // covers the gap between this check and the kill.
            if backend
                .exists(&session)
                .with_context(|| format!("checking session {session} before killing it"))?
            {
                backend
                    .kill(&session)
                    .with_context(|| format!("killing session {session}"))?;
            }
        }
        self.record_killed(&mut workspace).await?;
        Ok(workspace)
    }

    /// Writes down what a kill left behind: no session, and `stopped`.
    ///
    /// Shared by both kills so they cannot drift — the name is cleared because
    /// it names nothing now, and the status is the one a workspace nobody is
    /// attached to has.
    async fn record_killed(&self, workspace: &mut AdeWorkspace) -> Result<()> {
        if workspace.terminal_session_id.is_some() {
            self.registry
                .update_terminal_session_id(workspace.id.clone(), None)
                .await?;
            workspace.terminal_session_id = None;
        }
        self.set_status(workspace, WorkspaceStatus::Stopped).await
    }

    /// The startup pass: probes every registered workspace, writes the results
    /// back, and reports what each host holds beside them — so the sidebar opens
    /// showing what is actually running rather than what was running when the
    /// app last closed.
    pub async fn reconcile_all(&self) -> Result<Reconciled> {
        let workspaces = self.registry.list_workspaces()?;
        self.reconcile(workspaces, true, None).await
    }

    /// [`Self::reconcile_all`], narrowed to one project.
    ///
    /// **Rows only**, unlike the pass above: it is asked about one project, and
    /// every host's discoveries would be everything but. The reconciliation a
    /// host needs stays host-wide — its rows are judged together or not at
    /// all — but the *answer* is this project's, since a caller asking about
    /// one project has no use for a sibling's rows and every reason to mistake
    /// them for its own.
    pub async fn reconcile_project(&self, project_id: impl Into<String>) -> Result<Reconciled> {
        let project_id = project_id.into();
        let workspaces = self
            .registry
            .list_workspaces_for_project(project_id.clone())?;
        self.reconcile(workspaces, false, Some(project_id)).await
    }

    /// Brings one host's session backend up, reconciles this client's rows for
    /// it against what it holds, and answers with the registry as it then
    /// stands.
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
    /// connecting to. The answer is the *whole* registry rather than this
    /// host's rows, because matching a workspace back to a connection is the
    /// caller's rule (a destination may be spelled several ways) and narrowing
    /// here would pre-empt it.
    pub async fn ensure_host_workspaces(&self, host: Option<&str>) -> Result<Vec<AdeWorkspace>> {
        let backend = self.backend_for_host(host)?;
        self.reconcile_host(&backend, host).await?;
        self.registry.list_workspaces()
    }

    /// **One daemon's locked pass.** Takes the daemon's listing as the truth,
    /// brings this client's confirmed rows for that daemon into step with it,
    /// and answers with the rows that survived plus the listing itself.
    ///
    /// It only ever copies: nothing here mints an id, creates a daemon record,
    /// or derives anything — and since the usage-only cutover, nothing here
    /// creates a row either. A record no confirmed row points at is a
    /// *discovery*, rendered from the returned listing and persisted only when
    /// the user opens it ([`Self::persist_on_open`]).
    ///
    /// **Matched by `terminal_session_id`, which is the identity.** Matching on
    /// anything else — the path, the name — would act on the wrong workspace.
    /// Ids are *daemon*-scoped, so every row of every spelling that reaches
    /// this daemon is judged here, and a row on a spelling that names some
    /// other daemon is not.
    ///
    /// **A live binding is never moved.** Rebinding a row to the spelling whose
    /// pass is running is a migration off a spelling that has not named a
    /// daemon — a retired alias, a host that cannot be reached — and nothing
    /// else. Two live spellings of one daemon are one pass and rebind nothing;
    /// a row bound to a spelling that names a *different* daemon belongs to
    /// that daemon, whatever ids collide.
    ///
    /// The returned rows are the **post-write** survivors: a rename mirrored, a
    /// duplicate collapsed and a stale row swept here are all visible in the
    /// same refresh rather than one late.
    ///
    /// **And the listing is published from in here**, under the same lock, so a
    /// removal that took the lock after it cannot be undone by a pass that
    /// listed before it.
    async fn reconcile_host(
        &self,
        backend: &Arc<dyn SessionBackend>,
        host: Option<&str>,
    ) -> Result<HostPass> {
        // Around the listing as well as the read-then-write, since 2026-08-18:
        // the drop below judges rows against this listing, and a listing taken
        // outside the lock can predate a row another client has since created —
        // which is then deleted for having no record. It costs no parallelism
        // that was not already spent: one host's requests serialize on that
        // host's single connection anyway.
        //
        // Taken even for an empty listing: the sweep below still has to run —
        // an authoritative empty list is what drops every stale row this host
        // owns.
        // The daemon is only known once it has answered, and the lock has to be
        // held before the question — so a spelling reaching a daemon for the
        // first time locks under itself.
        // ponytail: one such pass per spelling per process; the sqlite
        // uniqueness index is what catches the race it leaves. Re-take the lock
        // under the resolved key if that ever proves too weak.
        let lock = self.decision_lock(host);
        let _deciding = lock.lock().await;
        let listed = backend
            .list_workspaces()
            .with_context(|| format!("listing the workspaces on {}", host_label(host)))?;
        let held = listed.workspaces;
        let daemon = self.daemon_key(host);
        // Duplicates first, so the name below lands on the row that survives
        // the pass rather than on the copy it is about to delete.
        let known = self.registry.list_workspaces()?;
        let known = self.collapse_duplicate_rows(&known, &daemon).await?;

        for workspace in &held {
            if let Some(row) = known.iter().find(|known| {
                self.row_is_on(known, &daemon)
                    && known.terminal_session_id.as_deref() == Some(workspace.id.as_str())
            }) {
                self.mirror_name(row, workspace).await?;
                self.record_daemon_id(row, &daemon).await?;
                continue;
            }

            if let Some(host) = host
                && let Some(existing) = known.iter().find(|known| {
                    known.remote_host.is_some()
                        // A spelling that has named a daemon is that daemon's,
                        // and this pass is not it: the ids collide, the
                        // workspaces do not.
                        && matches!(
                            self.daemon_key(known.remote_host.as_deref()),
                            DaemonKey::Host(_)
                        )
                        && known.terminal_session_id.as_deref() == Some(workspace.id.as_str())
                        && known.repository_path == Path::new(&workspace.project_root)
                })
            {
                self.registry
                    .update_remote_host(existing.id.clone(), Some(host.to_owned()))
                    .await
                    .with_context(|| format!("rebinding workspace {} to {host}", workspace.id))?;
                self.record_daemon_id(existing, &daemon).await?;
            }
        }

        // Re-read before anything is judged: the loop above rebound rows, and
        // another client may have written its own while this pass was talking
        // to the host. A verdict on the pre-write snapshot is a verdict on a
        // registry that no longer exists.
        let known = self.registry.list_workspaces()?;
        self.drop_unbacked_rows(&held, &known, &daemon, backend, listed.degraded)
            .await?;
        let survivors = self
            .registry
            .list_workspaces()?
            .into_iter()
            .filter(|row| self.row_is_on(row, &daemon))
            .collect();
        // Only an authoritative listing becomes the fallback: a degraded
        // daemon's omissions would otherwise erase live discoveries from every
        // later failed pass too.
        if !listed.degraded {
            self.discoveries
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(daemon.clone(), held.clone());
        }
        Ok(HostPass {
            daemon,
            survivors,
            held,
            degraded: listed.degraded,
        })
    }

    /// Records which daemon a row's spelling turned out to name, once.
    ///
    /// The column is what sqlite's one-row-per-record index is keyed by, so a
    /// row written before the daemon named itself has to be caught up before
    /// the index can hold for it. A no-op when it already agrees, which is
    /// every pass after the first.
    async fn record_daemon_id(&self, row: &AdeWorkspace, daemon: &DaemonKey) -> Result<()> {
        let DaemonKey::Instance(id) = daemon else {
            return Ok(());
        };
        if row.daemon_id.as_deref() == Some(id.as_str()) {
            return Ok(());
        }
        self.registry
            .update_daemon_id(row.id.clone(), Some(id.clone()))
            .await
            .with_context(|| format!("recording which daemon holds workspace {}", row.id))
    }

    /// **Persist-on-open**: records that this client is now using a workspace
    /// its host holds, and answers with the row that addresses it.
    ///
    /// A listing never does this — that is the whole usage-only rule. The one
    /// caller is an open, and an open is what stamps `used_by_client_at` and
    /// `last_opened_at`.
    ///
    /// **Promotion first.** A quarantined row for the same daemon, wire id and
    /// root is the same workspace under a uuid this client's windows, layouts
    /// and branch metadata already refer to, so it is confirmed in place rather
    /// than replaced by a fresh row that loses all of it. The *daemon*, not the
    /// spelling: a row quarantined under one alias is the same workspace when
    /// it is opened through another, and minting a second row for it would
    /// lose exactly what the quarantine kept.
    ///
    /// The caller serializes on the host's decision lock and re-lists before
    /// persisting; this does neither, so as not to be unlockable from under it.
    pub async fn persist_on_open(
        &self,
        record: &BackendWorkspace,
        host: Option<&str>,
    ) -> Result<AdeWorkspace> {
        let now = now_whole_seconds();
        let daemon = self.daemon_key(host);
        // A row this client already has for the record: a second window's
        // open, or the same record reached through another spelling of the
        // host. Clicking twice is not two workspaces — and the insert below
        // would *replace* that row, since sqlite holds one confirmed row per
        // record, taking its branch and its history with it.
        if let Some(row) = self.registry.list_workspaces()?.into_iter().find(|row| {
            self.row_is_on(row, &daemon)
                && row.terminal_session_id.as_deref() == Some(record.id.as_str())
        }) {
            return Ok(row);
        }
        let promotable = self
            .registry
            .unconfirmed_workspaces()?
            .into_iter()
            .find(|row| {
                self.row_is_on(row, &daemon)
                    && row.terminal_session_id.as_deref() == Some(record.id.as_str())
                    && row.repository_path == Path::new(&record.project_root)
            });
        if let Some(mut row) = promotable {
            self.registry
                .confirm_workspace(row.id.clone(), now)
                .await
                .with_context(|| format!("confirming workspace {}", row.id))?;
            self.record_daemon_id(&row, &daemon).await?;
            row.daemon_id = self.instance_of(host).or(row.daemon_id);
            row.last_opened_at = now;
            return Ok(row);
        }

        let row = row_for_record(record, host, self.instance_of(host), now);
        self.registry
            .create_workspace(row.clone())
            .await
            .with_context(|| format!("recording workspace {}", record.id))?;
        Ok(row)
    }

    /// **Reconcile**: one daemon record, one cache row.
    ///
    /// Two clients mirroring the same record at once each mint their own uuid
    /// and both inserts succeed — sqlite constrains that uuid, not the
    /// `(remote_host, terminal_session_id)` pair identity actually lives in —
    /// and neither row is unbacked, so nothing else removes either. The
    /// duplicates then show as two sidebar rows whose rename and kill both hit
    /// the same workspace.
    ///
    /// **The survivor must be the same one in every process**, so the choice is
    /// [`beats`]: metadata a mirror cannot re-derive first, recency second, and
    /// the uuid as a tie-break that cannot come out differently anywhere.
    /// Answers with the rows that are left.
    async fn collapse_duplicate_rows(
        &self,
        known: &[AdeWorkspace],
        daemon: &DaemonKey,
    ) -> Result<Vec<AdeWorkspace>> {
        let mut winners: HashMap<&str, &AdeWorkspace> = HashMap::new();
        let mut doomed: Vec<&AdeWorkspace> = Vec::new();
        for row in known.iter().filter(|row| self.row_is_on(row, daemon)) {
            let Some(record) = row.terminal_session_id.as_deref() else {
                continue;
            };
            match winners.entry(record) {
                Entry::Vacant(entry) => {
                    entry.insert(row);
                }
                Entry::Occupied(mut entry) => doomed.push(if beats(row, entry.get()) {
                    entry.insert(row)
                } else {
                    row
                }),
            }
        }
        if doomed.is_empty() {
            return Ok(known.to_vec());
        }
        let mut dropped = HashSet::new();
        for row in doomed {
            log::info!(
                "collapsing duplicate workspace row {} ({}) on {}: another row already mirrors \
                 that record",
                row.name,
                row.terminal_session_id.as_deref().unwrap_or("unlinked"),
                host_label(row.remote_host.as_deref())
            );
            self.registry
                .delete_workspace(row.id.clone())
                .await
                .with_context(|| format!("collapsing duplicate workspace row {}", row.id))?;
            dropped.insert(row.id.clone());
        }
        Ok(known
            .iter()
            .filter(|row| !dropped.contains(&row.id))
            .cloned()
            .collect())
    }

    /// **Reconcile**: drops every confirmed row of `host` that no daemon record
    /// backs — a recorded id absent from `held`, or no recorded id at all.
    /// Spec: "Registry rows with no daemon record ... are dropped."
    ///
    /// Quarantined rows are out of reach here: `known` comes from the confirmed
    /// reads, so a row the migration left unconfirmed is never swept.
    ///
    /// Three drop-guards, all preservation, not deletion, on doubt:
    /// - Only reached after `held` came back from a **successful** list — a
    ///   failed or unreachable host never calls this (see
    ///   [`Self::reconcile_host`]'s early `?` on the listing).
    /// - Never when the listing came back `degraded`: a newer-schema daemon's
    ///   ledger is read-only and its list may be incomplete, so this mirrors in
    ///   whatever it did list and destroys nothing on its silence — the same
    ///   fence inc 6's silent-kill uses, from the client side. The flag is the
    ///   *listing's*, not the backend's now: a mid-pass reconnect would
    ///   otherwise judge a degraded daemon's answer by a healthy one's flag.
    /// - **A candidate is confirmed against a second listing.** Every deletion
    ///   here is a row the host did not name, and a listing is a fact about
    ///   one instant; a second one taken after the candidates are frozen is
    ///   what tells a genuinely dead row from a row created while the first was
    ///   in flight. The set is never enlarged by it — a record that appeared
    ///   between the two belongs to whoever created it.
    async fn drop_unbacked_rows(
        &self,
        held: &[BackendWorkspace],
        known: &[AdeWorkspace],
        daemon: &DaemonKey,
        backend: &Arc<dyn SessionBackend>,
        degraded: bool,
    ) -> Result<()> {
        if degraded {
            return Ok(());
        }
        let candidates: Vec<&AdeWorkspace> = known
            .iter()
            .filter(|row| self.row_is_on(row, daemon))
            .filter(|row| !is_backed(row, held))
            .collect();
        if candidates.is_empty() {
            return Ok(());
        }
        let confirming = backend.list_workspaces().with_context(|| {
            format!(
                "re-listing the workspaces on {}",
                host_label(candidates[0].remote_host.as_deref())
            )
        })?;
        if confirming.degraded {
            return Ok(());
        }
        for row in candidates {
            if is_backed(row, &confirming.workspaces) {
                continue;
            }
            log::info!(
                "dropping workspace row {} ({}) on {}: no daemon record",
                row.name,
                row.terminal_session_id.as_deref().unwrap_or("unlinked"),
                host_label(row.remote_host.as_deref())
            );
            self.registry
                .delete_workspace(row.id.clone())
                .await
                .with_context(|| format!("dropping unbacked workspace row {}", row.id))?;
        }
        Ok(())
    }

    /// Takes the daemon's name for a row the cache already has — which is what
    /// makes a rename in one client show up in every other.
    ///
    /// **A machine name is not news, and neither is no name.** A record this
    /// client created before ids were minted carries its own id as its name
    /// (see [`display_name_for`]), and copying that over would replace the name
    /// the user actually sees with a machine string; an empty one — a record
    /// minted for a degenerate root — would leave the row with no name at all.
    /// Only a name a person could have typed propagates.
    async fn mirror_name(&self, row: &AdeWorkspace, record: &BackendWorkspace) -> Result<()> {
        if row.name == record.name || is_machine_named(record) || record.name.is_empty() {
            return Ok(());
        }
        self.registry
            .update_name(row.id.clone(), record.name.clone())
            .await
            .with_context(|| format!("taking the daemon's name for workspace {}", record.id))
    }

    /// The connect flow's whole decision, taken under the host's decision lock:
    /// reattach to a workspace this project already has — used or merely
    /// discovered — or create its first.
    ///
    /// **Both kinds of candidate, judged by one rule.** `matches` is the
    /// caller's `(remote_host, project root)` predicate: a destination may be
    /// spelled several ways and a local path several cases, and only the caller
    /// knows which of them its connection could reattach to. A confirmed row
    /// wins — it carries the branch and the history — and the most recently
    /// opened of those wins among themselves; failing that, a record the host
    /// holds is *confirmed* here, which is what stops an empty registry from
    /// minting a second workspace on a root the daemon already has.
    ///
    /// **A row only wins if the listing backs it, and only a listing entitled
    /// to say so may overrule one:**
    /// - healthy: every matching row whose wire id the host did not name
    ///   addresses nothing, so it is filtered out *before* recency picks a
    ///   winner — a newer stale row must not beat an older live one. The stale
    ///   row is left for the guarded sweep, never deleted inline.
    /// - failed: absence proves nothing, so a matching row wins; with no row
    ///   the listing's error is the answer.
    /// - degraded (§8.5): the ledger view is incomplete, so a matching row wins
    ///   even when omitted, a *listed* record is still usable when there is no
    ///   row, and an absent match is an error rather than a licence to mint a
    ///   duplicate.
    ///
    /// Both the registry and the **listing** are read inside the lock rather
    /// than reused from the caller's ensure: the gap between them is wide
    /// enough for another client to create or kill a workspace. `true` in the
    /// answer means the workspace was created here.
    pub async fn adopt_or_create_workspace(
        &self,
        root: PathBuf,
        host: Option<String>,
        matches: impl Fn(Option<&str>, &Path) -> bool + Send,
    ) -> Result<(AdeWorkspace, bool)> {
        let lock = self.decision_lock(host.as_deref());
        let _deciding = lock.lock().await;
        let rows = self.registry.list_workspaces()?;
        let candidates: Vec<&AdeWorkspace> = rows
            .iter()
            .filter(|row| matches(row.remote_host.as_deref(), &row.repository_path))
            .collect();

        let listed = match self
            .backend_for_host(host.as_deref())?
            .list_workspaces()
            .with_context(|| format!("listing the workspaces on {}", host_label(host.as_deref())))
        {
            Ok(listed) => listed,
            // The cached row is the *offline* answer, and only that: a daemon
            // this client cannot speak to answered, and answering it with a row
            // sends the window on to open a workspace against it (§6.1 — the
            // incompatibility has to reach the user, not be papered over).
            Err(error) if crate::daemon_backend::incompatible_daemon(&error).is_some() => {
                return Err(error);
            }
            Err(error) => {
                return match most_recently_opened(candidates.into_iter()) {
                    Some(existing) => Ok((existing.clone(), false)),
                    None => Err(error),
                };
            }
        };

        // Present *in this daemon's* listing: a wire id is only unique within
        // one daemon, so another daemon's row must not be validated by an id
        // this one happens to hold as well.
        let daemon = self.daemon_key(host.as_deref());
        let reattachable = if listed.degraded {
            most_recently_opened(candidates.into_iter())
        } else {
            most_recently_opened(candidates.into_iter().filter(|row| {
                self.row_is_on(row, &daemon)
                    && row
                        .daemon_workspace_id()
                        .is_some_and(|id| listed.workspaces.iter().any(|record| record.id == id))
            }))
        };
        if let Some(existing) = reattachable {
            return Ok((existing.clone(), false));
        }

        let used = self.wire_ids_in_use(host.as_deref())?;
        let discovered = listed
            .workspaces
            .into_iter()
            .filter(|record| !used.contains(&record.id))
            .filter(|record| matches(host.as_deref(), Path::new(&record.project_root)))
            // Deterministic: the host's ledger order is nobody's.
            .min_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        if let Some(record) = discovered {
            let row = self.persist_on_open(&record, host.as_deref()).await?;
            return Ok((row, false));
        }
        if listed.degraded {
            bail!(
                "{} could not read its whole workspace ledger, so opening {} now could mint a \
                 duplicate of a workspace it did not list",
                host_label(host.as_deref()),
                root.display()
            );
        }

        let name = project_id_from_path(&root);
        let created = self
            .create_workspace_locked(name.clone(), name, root, None, host)
            .await?;
        Ok((created, true))
    }

    /// **Opening a discovered workspace**: confirms the record the user clicked
    /// and answers with the row that now addresses it.
    ///
    /// The listing is taken again under the host's decision lock, so a record
    /// removed since the sidebar drew it is [`WorkspaceGone`] rather than a
    /// resurrected row. A record this client already uses answers with the row
    /// it has — clicking twice is not two workspaces.
    pub async fn confirm_discovered(
        &self,
        host: Option<&str>,
        wire_id: &str,
    ) -> Result<AdeWorkspace> {
        let lock = self.decision_lock(host);
        let _deciding = lock.lock().await;
        let daemon = self.daemon_key(host);
        if let Some(row) = self.registry.list_workspaces()?.into_iter().find(|row| {
            self.row_is_on(row, &daemon) && row.terminal_session_id.as_deref() == Some(wire_id)
        }) {
            return Ok(row);
        }
        let listed = self
            .backend_for_host(host)?
            .list_workspaces()
            .with_context(|| format!("listing the workspaces on {}", host_label(host)))?;
        let record = listed
            .workspaces
            .into_iter()
            .find(|record| record.id == wire_id)
            .ok_or_else(|| WorkspaceGone {
                remote_host: host.map(str::to_owned),
                wire_id: wire_id.to_owned(),
            })?;
        self.persist_on_open(&record, host).await
    }

    /// The argv a terminal pane runs to attach to this workspace.
    ///
    /// Attach-or-create, so reopening a pane on a live session reattaches to it
    /// with everything still running.
    ///
    /// **Local either way.** A remote workspace's argv still names *this*
    /// machine's attach client, pointed at the host's forwarded socket — one
    /// more channel on the host's single ssh connection.
    pub fn attach_command(&self, workspace: &AdeWorkspace) -> Result<Attached> {
        self.backend_for(workspace)?
            .attach(&Self::session_spec(workspace)?)
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
    pub fn create_session_in_workspace(
        &self,
        workspace: &AdeWorkspace,
        working_directory: &Path,
        view_id: &str,
    ) -> Result<(String, Vec<String>)> {
        let backend = self.backend_for(workspace)?;
        let spec = Self::session_spec(workspace)?;
        let session = backend
            .create_session_in_workspace(spec.id.as_str(), working_directory)
            .with_context(|| format!("creating another session in {}", spec.id))?;
        let argv = backend.attach_session(&session, view_id)?;
        Ok((session, argv))
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
        view_id: &str,
    ) -> Result<Vec<String>> {
        self.backend_for(workspace)?
            .attach_session(session_id, view_id)
    }

    /// Tells the backend that `view_id` is the view the user is now typing in,
    /// so the session's pty follows *its* size instead of the smallest of every
    /// client attached to it.
    ///
    /// No size travels with it: the view's own attach client keeps sending its
    /// geometry, and this only says whose to follow.
    pub fn focus_session(
        &self,
        workspace: &AdeWorkspace,
        session_id: &str,
        view_id: &str,
        hover: bool,
    ) -> Result<()> {
        self.backend_for(workspace)?
            .focus_session(session_id, view_id, hover)
    }

    /// The workspace's layout as the backend holds it, with the revision that
    /// guards the next write.
    ///
    /// An error means there is nothing to render — the backend has never heard
    /// of this workspace, or could not be asked — and the caller falls back to
    /// the single-terminal open.
    pub fn open_workspace_layout(&self, workspace: &AdeWorkspace) -> Result<WorkspaceLayout> {
        self.backend_for(workspace)?
            .open_workspace(wire_id(workspace)?)
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
        self.backend_for(workspace)?
            .update_layout(wire_id(workspace)?, layout, rev)
    }

    /// Kills the one session a closed terminal tab was showing.
    ///
    /// Destructive, and reached from exactly one control: closing a terminal
    /// tab (operator ruling, 2026-08-04). Closing the *window* detaches and
    /// kills nothing, and the workspace-level kill stays
    /// [`Self::kill_workspace_session`].
    pub fn kill_session(&self, workspace: &AdeWorkspace, session_id: &str) -> Result<()> {
        self.backend_for(workspace)?.kill_session(session_id)
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

    /// The protocol generation `destination`'s daemon last handshook at, or
    /// `None` while nothing has reached it.
    ///
    /// Same rules as [`Self::host_daemon_stale`], for the same reason: a render
    /// asks, so no backend is created to answer.
    pub fn host_daemon_generation(&self, destination: &str) -> Option<u32> {
        self.backends
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&Some(destination.to_owned()))
            .and_then(|backend| backend.daemon_generation())
    }

    /// A stream that yields once every time some host's answer to
    /// [`Self::host_daemon_stale`] or [`Self::host_daemon_generation`] changes.
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
        freshness_announcer(&self.freshness_watchers)
    }

    /// The hosts whose event stream is down, as [`Reconciled::host_errors`]
    /// lines. Cleared by the host coming back, unlike [`Self::status_errors`]:
    /// a stream that recovered is not a standing condition.
    fn stream_errors(&self) -> Vec<(String, String)> {
        self.events
            .streams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(host, stream)| {
                (
                    host_label(host.as_deref()),
                    format!("status updates stopped: {}", stream.message),
                )
            })
            .collect()
    }

    /// The next mid-session incompatibility nobody has surfaced yet, and the
    /// direction it is in.
    ///
    /// **The dedupe lives here, not in the caller**, so however many views
    /// watch, one down produces one dialog: the entry is marked reported as it
    /// is handed out, and only the stream going down again makes fresh news.
    /// Scoped to the host spelling the stream runs on, like the arrow (§6.4).
    pub fn take_stream_incompatibility(&self) -> Option<(Option<String>, Outdated)> {
        let mut streams = self
            .events
            .streams
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (host, stream) = streams
            .iter_mut()
            .find(|(_, stream)| !stream.reported && stream.outdated.is_some())?;
        stream.reported = true;
        Some((host.clone(), stream.outdated?))
    }

    /// One listing **per host**, rather than one probe per workspace: startup
    /// reconciles every row, and each probe costs the backend a round trip.
    /// Sound because a listing reports exactly the sessions this app owns on
    /// that host. A host with nothing to probe is never asked, and never
    /// connected to.
    ///
    /// Session ids are only unique *within* a host — two hosts can hold
    /// `ade-main-012345` at once — so the live sets are kept apart rather than
    /// unioned, and a workspace is only ever matched against its own host's.
    ///
    /// A host that cannot be reached fails alone: its rows come back
    /// [`SessionState::Unknown`] with their stored status untouched, and the
    /// reason lands in [`Reconciled::host_errors`].
    ///
    /// **Discovery survives without a row.** With `discover`, the hosts are the
    /// union of the ones this client's rows name, the ones a backend already
    /// exists for, and this machine — so a host whose workspaces the client has
    /// merely *seen* keeps appearing after the connect that first reached it.
    /// Only hosts this pass was already going to contact: reconciliation must
    /// not turn a registry full of machines that are switched off into a wall
    /// of ssh attempts.
    async fn reconcile(
        &self,
        workspaces: Vec<AdeWorkspace>,
        discover: bool,
        project: Option<String>,
    ) -> Result<Reconciled> {
        // The hosts worth asking what they hold: this machine, whether or not a
        // row names it — the daemon is the source of truth and the registry only
        // what this client used — and every host a backend already exists for,
        // which is how a host reached by one connect keeps its discoveries.
        let mut discovering: Vec<Option<String>> = Vec::new();
        if discover {
            discovering.push(None);
            let mut instantiated: Vec<Option<String>> = self
                .backends
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .keys()
                .filter(|host| host.is_some())
                .cloned()
                .collect();
            // The map's order is nobody's; the pass's must be the same twice.
            instantiated.sort();
            discovering.extend(instantiated);
        }
        let mut hosts = discovering.clone();
        for workspace in &workspaces {
            if !hosts.contains(&workspace.remote_host) {
                hosts.push(workspace.remote_host.clone());
            }
        }

        let mut host_errors = self.status_errors();
        host_errors.extend(self.stream_errors());
        let mut persisted: Vec<(AdeWorkspace, SessionState)> = Vec::new();
        let mut discovered: Vec<WorkspaceEntry> = Vec::new();
        let mut daemons: HashMap<Option<String>, String> = HashMap::new();
        // Every spelling's outcome, kept until all of them have been tried:
        // which rows a daemon owns is only settled once each spelling has said
        // which daemon it reaches.
        let mut outcomes: Vec<HostOutcome> = Vec::new();
        // The daemons a pass has already reconciled. A second spelling of one
        // of them would list the same records, judge the same rows and rebind
        // them straight back — Sol M4's two entries and two writes per refresh.
        let mut covered: HashSet<DaemonKey> = HashSet::new();
        for host in hosts {
            // Reported whether or not this spelling is the one that asks: the
            // views have to know that two of them are one daemon, and the
            // spelling that is skipped below is exactly the one they would
            // otherwise treat as a host of its own.
            let known_daemon = self.daemon_key(host.as_deref());
            if let DaemonKey::Instance(id) = &known_daemon {
                daemons.insert(host.clone(), id.clone());
            }
            // Only a daemon already known can be skipped before its listing;
            // one that names itself for the first time below is skipped from
            // the next refresh on, and this pass drops its second copy after
            // the fact.
            if covered.contains(&known_daemon) {
                continue;
            }
            let anything_to_probe = workspaces
                .iter()
                .filter(|workspace| workspace.remote_host == host)
                .any(|workspace| workspace.terminal_session_id.is_some());

            // A host named only by rows with nothing to probe is not contacted:
            // a registry full of machines that are switched off must not become
            // a wall of ssh attempts.
            let contacting = anything_to_probe || discovering.contains(&host);
            // `None` means the host could not be asked, which is what makes its
            // entries read as `Unknown` below.
            let mut live: Option<HashSet<SessionId>> = None;
            let mut pass: Option<HostPass> = None;
            if !contacting {
                live = Some(HashSet::new());
            } else if let Some(backend) = match self.backend_for_host(host.as_deref()) {
                Ok(backend) => Some(backend),
                Err(error) => {
                    host_errors.push((host_label(host.as_deref()), format!("{error:#}")));
                    None
                }
            } {
                // Before the session listing, so the rows this pass swept or
                // collapsed are the ones it probes. A host that will not answer
                // the first question will not answer the second either, so it
                // fails once and its rows go untouched.
                match self.reconcile_host(&backend, host.as_deref()).await {
                    Ok(reconciled) => {
                        if let DaemonKey::Instance(id) = &reconciled.daemon {
                            daemons.insert(host.clone(), id.clone());
                        }
                        covered.insert(reconciled.daemon.clone());
                        pass = Some(reconciled);
                    }
                    Err(error) => {
                        host_errors.push((host_label(host.as_deref()), format!("{error:#}")))
                    }
                }
                if pass.is_some() {
                    match backend.list().context("listing live sessions") {
                        Ok(sessions) => {
                            live = Some(sessions.into_iter().map(|session| session.id).collect());
                        }
                        Err(error) => {
                            host_errors.push((host_label(host.as_deref()), format!("{error:#}")))
                        }
                    }
                }
            }
            outcomes.push(HostOutcome {
                daemon: pass.as_ref().map_or_else(
                    || self.daemon_key(host.as_deref()),
                    |pass| pass.daemon.clone(),
                ),
                host,
                contacting,
                pass,
                live,
            });
        }

        // **Answered first.** A daemon reached through a spelling that failed
        // and a spelling that worked is one daemon that worked; taking the
        // failure's `Unknown` rows first would report it unreachable while
        // holding its listing.
        outcomes.sort_by_key(|outcome| outcome.pass.is_none());
        // One row is one entry, whichever spelling's pass claimed it: on the
        // refresh where a second spelling names its daemon for the first time,
        // both passes ran and both hold the daemon's rows.
        let mut seen_rows: HashSet<WorkspaceId> = HashSet::new();
        let mut rendered: HashSet<DaemonKey> = HashSet::new();
        for outcome in &outcomes {
            let rows = match &outcome.pass {
                Some(pass) => pass.survivors.clone(),
                None => workspaces
                    .iter()
                    .filter(|workspace| workspace.remote_host == outcome.host)
                    .cloned()
                    .collect(),
            };
            for mut workspace in rows {
                if !seen_rows.insert(workspace.id.clone()) {
                    continue;
                }
                let state = match workspace.terminal_session_id.clone().map(SessionId::from) {
                    None => SessionState::NeverCreated,
                    Some(session) => match &outcome.live {
                        Some(live) => {
                            self.record_probe(&mut workspace, live.contains(&session))
                                .await?
                        }
                        None => SessionState::Unknown,
                    },
                };
                persisted.push((workspace, state));
            }
        }

        for outcome in &outcomes {
            if !discover || !outcome.contacting || !rendered.insert(outcome.daemon.clone()) {
                continue;
            }
            // A failed listing is not a disconnect: the host keeps whatever it
            // last held, as `Unknown`, until an authoritative listing replaces
            // it. A degraded one is rendered *over* that snapshot rather than
            // in place of it — what it omitted may still be alive, so dropping
            // those records would blink live workspaces off the sidebar.
            //
            // `present` is which ids this pass actually saw; `None` means the
            // whole listing was authoritative. A record only in the fallback
            // reads `Unknown`, because the current session list says nothing
            // about a workspace the current workspace list never named.
            let (held, present): (Vec<BackendWorkspace>, Option<HashSet<String>>) =
                match &outcome.pass {
                    Some(pass) if !pass.degraded => (pass.held.clone(), None),
                    Some(pass) => {
                        let present: HashSet<String> =
                            pass.held.iter().map(|record| record.id.clone()).collect();
                        let mut listed = pass.held.clone();
                        listed.extend(
                            self.remembered_discoveries(&outcome.daemon)
                                .into_iter()
                                .filter(|record| !present.contains(&record.id)),
                        );
                        (listed, Some(present))
                    }
                    None => (
                        self.remembered_discoveries(&outcome.daemon),
                        Some(HashSet::new()),
                    ),
                };
            // Every spelling of this daemon: a wire id a row already uses is
            // not a discovery, however the row's host was typed.
            let used: HashSet<&str> = persisted
                .iter()
                .filter(|(workspace, _)| self.row_is_on(workspace, &outcome.daemon))
                .filter_map(|(workspace, _)| workspace.daemon_workspace_id())
                .collect();
            for record in held {
                if used.contains(record.id.as_str()) {
                    continue;
                }
                let listed_now = present
                    .as_ref()
                    .is_none_or(|present| present.contains(&record.id));
                let state = match &outcome.live {
                    Some(live) if listed_now => {
                        if live.contains(&SessionId::from(record.id.clone())) {
                            SessionState::Alive
                        } else {
                            SessionState::Dead
                        }
                    }
                    _ => SessionState::Unknown,
                };
                discovered.push(WorkspaceEntry::Discovered {
                    remote_host: outcome.host.clone(),
                    workspace: record,
                    state,
                });
            }
        }

        // A pass narrowed to one project answers about that project: its host's
        // other rows were reconciled along with it, but they are not its.
        if let Some(project) = &project {
            persisted.retain(|(workspace, _)| &workspace.project_id == project);
        }

        // The rows the user worked in most recently lead; what the hosts hold
        // and this client has never opened follows, in an order that cannot come
        // out differently twice.
        persisted.sort_by(|(a, _), (b, _)| {
            b.last_opened_at
                .cmp(&a.last_opened_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        // Daemon creation order, with the opaque id only as a tie-break: sorting
        // by the id alone puts newly created discoveries at positions unrelated
        // to their history.
        discovered.sort_by(|a, b| discovered_order(a).cmp(&discovered_order(b)));
        let entries = persisted
            .into_iter()
            .map(|(workspace, state)| WorkspaceEntry::Persisted(workspace, state))
            .chain(discovered)
            .collect();
        Ok(Reconciled {
            entries,
            host_errors,
            daemons,
        })
    }

    fn remembered_discoveries(&self, daemon: &DaemonKey) -> Vec<BackendWorkspace> {
        self.discoveries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(daemon)
            .cloned()
            .unwrap_or_default()
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
            // Unreachable in practice: every service is built through
            // [`Self::with_backend`], which seeds this machine's backend under
            // the `None` key, so the lookup above has already answered. It is
            // an error rather than an `unwrap` because "this machine has no
            // backend" is a sentence a caller can act on and a panic is not.
            bail!("this service has no backend for the local machine");
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

    /// What the backend is asked to attach to for this workspace: the id the
    /// daemon knows it by, rooted at its checkout.
    ///
    /// `repository_path` is taken verbatim, which for a remote workspace means
    /// a path on *its* host — the backend for that host is the one that will
    /// resolve it, and this machine's filesystem never enters into it.
    fn session_spec(workspace: &AdeWorkspace) -> Result<SessionSpec> {
        Ok(SessionSpec::new(
            wire_id(workspace)?,
            workspace.repository_path.clone(),
        ))
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
    /// Whose event stream is currently down, and why — see [`HostStream`].
    /// Written by the pump threads, read by the service.
    streams: Mutex<HashMap<Option<String>, HostStream>>,
    /// Who to wake when one of those moves; the same seam a freshness verdict
    /// uses, because a redrawn sidebar re-asks either way.
    announce: Mutex<Option<crate::DaemonFreshnessObserver>>,
}

/// One host's event stream, once it has stopped working.
///
/// **One entry is one down.** A stream that goes down, comes back and goes down
/// again is two separate pieces of news — two entries — while the reconnect
/// loop's repeats are the same one, and so is a second window asking.
#[derive(Clone, Debug, PartialEq, Eq)]
struct HostStream {
    message: String,
    /// Typed direction when the two ends cannot speak at all, `None` for an
    /// ordinary reachability failure — which is a host error and no dialog.
    outdated: Option<Outdated>,
    /// Whether the app-global consumer has already surfaced this down.
    reported: bool,
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
    }

    /// Hands one event to its stream. `false` once both streams are gone, which
    /// is what ends a pump.
    ///
    /// A closed receiver clears its slot rather than being retried: dropping the
    /// receiver is the only unsubscribe there is, and a slot left holding a dead
    /// sender would keep [`Self::is_idle`] answering "somebody is listening"
    /// forever.
    fn deliver(&self, remote_host: &Option<String>, event: DaemonEvent) -> bool {
        match event {
            DaemonEvent::Session(event) => send_or_clear(&self.status, event),
            DaemonEvent::Layout(event) => send_or_clear(
                &self.layout,
                WorkspaceEvent::Layout {
                    remote_host: remote_host.clone(),
                    event,
                },
            ),
            DaemonEvent::WorkspaceReset(event) => send_or_clear(
                &self.layout,
                WorkspaceEvent::Reset {
                    remote_host: remote_host.clone(),
                    event,
                },
            ),
            DaemonEvent::WorkspaceRemoved { workspace_id } => send_or_clear(
                &self.layout,
                WorkspaceEvent::Removed {
                    remote_host: remote_host.clone(),
                    workspace_id,
                },
            ),
            DaemonEvent::Up | DaemonEvent::Down { .. } => self.record_stream(remote_host, event),
        }
        !self.is_idle()
    }

    /// Takes one host's stream transition, in the order the backend sent it.
    ///
    /// The backend announces *changes* only, so every `Down` arriving here is
    /// fresh news and replaces whatever this host's last one said — including
    /// whether it has been surfaced.
    fn record_stream(&self, remote_host: &Option<String>, event: DaemonEvent) {
        {
            let mut streams = self.streams.lock().unwrap_or_else(|e| e.into_inner());
            match event {
                DaemonEvent::Up => {
                    streams.remove(remote_host);
                }
                DaemonEvent::Down { message, outdated } => {
                    streams.insert(
                        remote_host.clone(),
                        HostStream {
                            message,
                            outdated,
                            reported: false,
                        },
                    );
                }
                _ => unreachable!("only stream transitions reach here"),
            }
        }
        // Never with the map locked: an observer is a channel send today, but
        // this is the lock a render would otherwise park behind.
        let announce = self
            .announce
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(announce) = announce {
            announce();
        }
    }
}

/// The observer that turns a private discovery — a freshness verdict, a stream
/// that went down — into the public redraw stream.
///
/// A free function because the fanout holds one too, and it must not hold the
/// service: the service owns the fanout.
fn freshness_announcer(watchers: &Arc<Mutex<Vec<Sender<()>>>>) -> crate::DaemonFreshnessObserver {
    let watchers = watchers.clone();
    Arc::new(move || {
        // Unbounded, so a send only fails on a receiver that is gone with its
        // window. Pruning those here is what keeps a long session of opened and
        // closed windows from growing this list forever.
        watchers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retain(|watcher| watcher.try_send(()).is_ok());
    })
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
    use crate::{LayoutEvent, SessionChange};

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

    /// This machine has a backend on every platform ADE ships on, which is the
    /// one thing a `cfg` here used to deny. Nothing is contacted: naming the
    /// backend is what
    /// [`WorkspaceLifecycleService::new`] decides, and reaching a daemon is what
    /// the caller's first operation decides.
    #[gpui::test]
    async fn test_the_default_backend_serves_this_machine() {
        let registry =
            AdeWorkspaceRegistry::open_test_db("test_the_default_backend_serves_this_machine")
                .await;
        WorkspaceLifecycleService::new(registry)
            .backend_for_host(None)
            .expect("this machine's own session backend");
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

    /// The adopt-or-create decision is one caller's at a time per host: two
    /// windows on one project that both miss the registry would otherwise both
    /// create. The lock is taken by hand because the test registry writes
    /// synchronously — nothing else here can park a task mid-decision.
    #[gpui::test]
    async fn test_one_root_gets_one_workspace_however_many_callers_decide(
        cx: &mut gpui::TestAppContext,
    ) {
        let registry = AdeWorkspaceRegistry::open_test_db("test_adopt_or_create_serializes").await;
        let backend = Arc::new(FakeBackend::new("local"));
        let service = Arc::new(WorkspaceLifecycleService::with_backend(
            registry,
            backend.clone(),
        ));

        let gate = service.decision_lock(None);
        let held = gate.lock().await;

        let decide = |service: Arc<WorkspaceLifecycleService>| async move {
            service
                .adopt_or_create_workspace(PathBuf::from("/repos/zed"), None, |host, root| {
                    host.is_none() && root == Path::new("/repos/zed")
                })
                .await
                .expect("the decision succeeds")
        };
        let first = cx.executor().spawn(decide(service.clone()));
        let second = cx.executor().spawn(decide(service.clone()));
        // Creating takes the same lock, so the gap between a minted record and
        // the row that caches it cannot be read by a concurrent decision.
        let third = cx.executor().spawn({
            let service = service.clone();
            async move {
                service
                    .create_workspace("other", "other", "/repos/other", None, None)
                    .await
                    .expect("the create succeeds")
            }
        });

        cx.run_until_parked();
        assert!(
            service.registry().list_workspaces().unwrap().is_empty(),
            "every writer waits on the host's decision lock"
        );

        drop(held);
        cx.run_until_parked();
        let (first, first_created) = first.await;
        let (second, second_created) = second.await;
        third.await;

        assert_eq!(first.id, second.id, "the loser adopted the winner's row");
        assert_ne!(first_created, second_created, "exactly one of them created");
        assert_eq!(service.registry().list_workspaces().unwrap().len(), 2);
        assert_eq!(
            backend
                .calls()
                .iter()
                .filter(|call| call.starts_with("create_workspace:"))
                .count(),
            2
        );
    }

    /// A host's reconcile pass takes the same lock as the connect decision, so
    /// the two cannot interleave their listing, read and write.
    ///
    /// **And the decision reattaches.** The registry is empty — a listing
    /// confirms nothing — so a decision that read only rows would mint a second
    /// daemon workspace on a root the host already holds. It judges the
    /// listing too, and confirms the record it finds.
    #[gpui::test]
    async fn test_reconciling_and_deciding_do_not_interleave(cx: &mut gpui::TestAppContext) {
        let registry = AdeWorkspaceRegistry::open_test_db("test_adopt_vs_decide_serializes").await;
        let backend = Arc::new(FakeBackend::new("local").holding(vec![BackendWorkspace {
            id: "ade-zed-2de8b3".to_owned(),
            name: "ade-zed-2de8b3".to_owned(),
            project_root: "/repos/zed".to_owned(),
            created_at: 1_700_000_000,
        }]));
        let service = Arc::new(WorkspaceLifecycleService::with_backend(
            registry,
            backend.clone(),
        ));

        let gate = service.decision_lock(None);
        let held = gate.lock().await;

        let ensuring = cx.executor().spawn({
            let service = service.clone();
            async move {
                service
                    .ensure_host_workspaces(None)
                    .await
                    .expect("the ensure succeeds")
            }
        });
        // Queued on the gate first, which is the order the connect flow gives
        // them: it ensures the host before it decides.
        cx.run_until_parked();
        let deciding = cx.executor().spawn({
            let service = service.clone();
            async move {
                service
                    .adopt_or_create_workspace(PathBuf::from("/repos/zed"), None, |host, root| {
                        host.is_none() && root == Path::new("/repos/zed")
                    })
                    .await
                    .expect("the decision succeeds")
            }
        });

        cx.run_until_parked();
        assert!(service.registry().list_workspaces().unwrap().is_empty());

        drop(held);
        cx.run_until_parked();
        // The ensure ran to completion before the decision read anything: it
        // listed the host and left the record where it was, unmirrored.
        let after_ensure = ensuring.await;
        assert!(after_ensure.is_empty(), "a listing writes no row");
        let (workspace, created) = deciding.await;

        assert!(!created, "the daemon already held this root");
        assert_eq!(service.registry().list_workspaces().unwrap().len(), 1);
        assert_eq!(
            workspace.daemon_workspace_id(),
            Some("ade-zed-2de8b3"),
            "the record the host holds, confirmed rather than duplicated"
        );
        assert!(
            !backend
                .calls()
                .iter()
                .any(|call| call.starts_with("create_workspace:")),
            "no second daemon workspace: {:?}",
            backend.calls()
        );
    }

    /// **A listing is discovery, not use.** Seeing what a host holds writes no
    /// row: the registry stays empty however many times the host is ensured,
    /// and the records show up as discovered entries instead.
    #[gpui::test]
    async fn test_a_listing_creates_no_confirmed_row() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_listing_creates_no_row").await;
        let local = Arc::new(FakeBackend::new("local"));
        let host = Arc::new(FakeBackend::new("dev-box").holding(vec![BackendWorkspace {
            id: "ade-testproj-2de8b3".to_owned(),
            name: "ade-testproj-2de8b3".to_owned(),
            project_root: "/home/kingii/testproj".to_owned(),
            created_at: 1_700_000_000,
        }]));
        let service = WorkspaceLifecycleService::with_backend(registry, local.clone())
            .with_backend_for_host("dev-box", host.clone());

        for _ in 0..2 {
            assert!(
                service
                    .ensure_host_workspaces(Some("dev-box"))
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
        assert!(service.registry().list_workspaces().unwrap().is_empty());
        // And this machine's backend was never asked about another host's.
        assert!(local.calls().is_empty(), "{:?}", local.calls());

        let discovered = service.reconcile_all().await.unwrap();
        let hosts: Vec<Option<&str>> = discovered
            .entries
            .iter()
            .map(WorkspaceEntry::remote_host)
            .collect();
        assert_eq!(hosts, vec![Some("dev-box")]);
        assert!(matches!(
            discovered.entries[0],
            WorkspaceEntry::Discovered { .. }
        ));
        assert!(service.registry().list_workspaces().unwrap().is_empty());
    }

    /// **Opening is what persists.** The conversion `adopted_row` used to do on
    /// every listing now happens once, for the workspace the user opened, with
    /// `last_opened_at` at the moment of the open rather than the daemon's
    /// creation time.
    #[gpui::test]
    async fn test_persisting_on_open_converts_the_record_once() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_persist_on_open_converts").await;
        let service =
            WorkspaceLifecycleService::with_backend(registry, Arc::new(FakeBackend::new("local")));
        // Machine-named, so the row is named for its checkout instead.
        let machine_named = BackendWorkspace {
            id: "ade-testproj-2de8b3".to_owned(),
            name: "ade-testproj-2de8b3".to_owned(),
            project_root: "/home/kingii/testproj".to_owned(),
            created_at: 1_700_000_000,
        };
        // Named by a person: persisting must not throw that away.
        let renamed = BackendWorkspace {
            id: "ade-scratch-0f1e2d".to_owned(),
            name: "Investigation: vector DB".to_owned(),
            project_root: "/home/kingii/scratch".to_owned(),
            created_at: 1_700_000_100,
        };

        let testproj = service
            .persist_on_open(&machine_named, Some("dev-box"))
            .await
            .unwrap();
        assert_eq!(testproj.name, "testproj");
        assert_eq!(testproj.project_id, "testproj");
        assert_eq!(
            testproj.repository_path,
            PathBuf::from("/home/kingii/testproj")
        );
        assert_eq!(testproj.remote_host.as_deref(), Some("dev-box"));
        assert!(testproj.branch.is_none());
        // The identity the daemon knows it by, so open/attach/rename address it.
        assert_eq!(testproj.daemon_workspace_id(), Some("ade-testproj-2de8b3"));
        assert_eq!(testproj.created_at.unix_timestamp(), 1_700_000_000);
        assert!(testproj.last_opened_at > testproj.created_at, "opened now");

        let scratch = service
            .persist_on_open(&renamed, Some("dev-box"))
            .await
            .unwrap();
        assert_eq!(scratch.name, "Investigation: vector DB");
        assert_eq!(scratch.project_id, "scratch");
        assert_eq!(service.registry().list_workspaces().unwrap().len(), 2);
    }

    /// **Promotion, not replacement.** A row the migration quarantined for the
    /// same host, wire id and root is the workspace this client's windows,
    /// layouts and branch metadata already refer to, so opening it confirms
    /// that row rather than minting a second uuid for the same record.
    #[gpui::test]
    async fn test_opening_promotes_the_quarantined_row_it_matches() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_open_promotes_quarantined").await;
        let service =
            WorkspaceLifecycleService::with_backend(registry, Arc::new(FakeBackend::new("local")));

        let mut quarantined = AdeWorkspace::new("my name for it", "zed", "/repos/zed");
        quarantined.last_opened_at = OffsetDateTime::from_unix_timestamp(1_700_000_500).unwrap();
        quarantined.terminal_session_id = Some("ws-local-1".to_owned());
        quarantined.branch = Some("feature/windows-holder".to_owned());
        quarantined.remote_workspace_path = Some(PathBuf::from("/srv/zed"));
        service
            .registry()
            .create_unconfirmed_workspace(quarantined.clone())
            .await
            .unwrap();
        // A record at another root under the same id: host-scoped ids collide,
        // and promoting across roots would rebind the wrong workspace.
        let elsewhere = BackendWorkspace {
            id: "ws-local-1".to_owned(),
            name: "zed".to_owned(),
            project_root: "/repos/elsewhere".to_owned(),
            created_at: 1_700_000_000,
        };
        let fresh = service.persist_on_open(&elsewhere, None).await.unwrap();
        assert_ne!(fresh.id, quarantined.id);
        assert!(fresh.branch.is_none());
        // One daemon cannot really hold two records under one id, and sqlite
        // now holds one confirmed row per record: the stand-in goes before the
        // real record arrives.
        service
            .registry()
            .delete_workspace(fresh.id.clone())
            .await
            .unwrap();

        let record = BackendWorkspace {
            project_root: "/repos/zed".to_owned(),
            ..elsewhere
        };
        let promoted = service.persist_on_open(&record, None).await.unwrap();

        assert_eq!(promoted.id, quarantined.id, "the row keeps its uuid");
        assert_eq!(promoted.branch.as_deref(), Some("feature/windows-holder"));
        assert_eq!(
            promoted.remote_workspace_path,
            Some(PathBuf::from("/srv/zed"))
        );
        assert_eq!(promoted.name, "my name for it");
        assert!(promoted.last_opened_at > quarantined.last_opened_at);
        assert!(
            service
                .registry()
                .unconfirmed_workspaces()
                .unwrap()
                .is_empty()
        );
    }

    /// **A create that was already half-done.** The daemon minted the record
    /// and the registry write failed, so a second Confirm on the same host,
    /// root and name must recover that record rather than mint a twin.
    #[gpui::test]
    async fn test_a_create_retry_recovers_its_own_record() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_create_retry_recovers").await;
        let backend = Arc::new(FakeBackend::new("local"));
        let service = WorkspaceLifecycleService::with_backend(registry, backend.clone());

        // What the first Confirm left behind: a record with no row.
        SessionBackend::create_workspace(&*backend, Path::new("/repos/zed"), Some("spike"))
            .unwrap();

        let recovered = service
            .create_workspace("spike", "zed", "/repos/zed", None, None)
            .await
            .unwrap();
        assert_eq!(recovered.daemon_workspace_id(), Some("ws-local-1"));
        assert_eq!(recovered.name, "spike");
        assert_eq!(service.registry().list_workspaces().unwrap().len(), 1);
        assert_eq!(
            backend
                .calls()
                .iter()
                .filter(|call| call.starts_with("create_workspace:"))
                .count(),
            1,
            "the retry recovered the record instead of minting one: {:?}",
            backend.calls()
        );

        // Same root, another name: a second workspace, not a retry.
        let sibling = service
            .create_workspace("review", "zed", "/repos/zed", None, None)
            .await
            .unwrap();
        assert_eq!(sibling.daemon_workspace_id(), Some("ws-local-2"));
        assert_eq!(service.registry().list_workspaces().unwrap().len(), 2);
    }

    /// **A removal takes both views of the workspace**: the row that used it
    /// and the discovery the sidebar draws it from. No window, no layout sync,
    /// and nothing left behind.
    #[gpui::test]
    async fn test_a_removal_drops_the_row_and_the_discovery() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_removal_drops_both").await;
        let backend = Arc::new(FakeBackend::new("local").holding(vec![BackendWorkspace {
            id: "ws-local-1".to_owned(),
            name: "spike".to_owned(),
            project_root: "/repos/zed".to_owned(),
            created_at: 1_700_000_000,
        }]));
        let service = WorkspaceLifecycleService::with_backend(registry, backend.clone());

        service.reconcile_all().await.unwrap();
        let confirmed = service
            .confirm_discovered(None, "ws-local-1")
            .await
            .unwrap();
        // Clicking twice is one workspace: the row it already made answers.
        assert_eq!(
            service
                .confirm_discovered(None, "ws-local-1")
                .await
                .unwrap()
                .id,
            confirmed.id
        );
        assert_eq!(service.registry().list_workspaces().unwrap().len(), 1);
        assert_eq!(
            service
                .remembered_discoveries(&service.daemon_key(None))
                .len(),
            1
        );

        backend.forget_all();
        service.forget_workspace(None, "ws-local-1").await.unwrap();

        assert!(service.registry().list_workspaces().unwrap().is_empty());
        assert!(
            service
                .remembered_discoveries(&service.daemon_key(None))
                .is_empty()
        );
        // Told twice — an own kill and the daemon's broadcast — is not an error.
        service.forget_workspace(None, "ws-local-1").await.unwrap();
    }

    /// **The stale-listing resurrection race.** A sidebar entry drawn before a
    /// removal can be clicked after it; the confirmation re-lists under the
    /// host's decision lock, so it writes no row for a record that is gone.
    #[gpui::test]
    async fn test_a_removed_record_cannot_be_confirmed_from_a_stale_listing() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_stale_listing_resurrection").await;
        let backend = Arc::new(FakeBackend::new("local").holding(vec![BackendWorkspace {
            id: "ws-local-1".to_owned(),
            name: "spike".to_owned(),
            project_root: "/repos/zed".to_owned(),
            created_at: 1_700_000_000,
        }]));
        let service = WorkspaceLifecycleService::with_backend(registry, backend.clone());
        service.reconcile_all().await.unwrap();

        backend.forget_all();
        service.forget_workspace(None, "ws-local-1").await.unwrap();

        let error = service
            .confirm_discovered(None, "ws-local-1")
            .await
            .expect_err("the record is gone");
        assert!(
            error.downcast_ref::<WorkspaceGone>().is_some(),
            "the caller must be able to tell a removal from a broken host: {error:#}"
        );
        assert!(service.registry().list_workspaces().unwrap().is_empty());
    }

    /// A store over a fake host holding one discovered workspace, refreshed
    /// once so its entries are drawn.
    ///
    /// Here rather than in `store.rs` because [`FakeBackend`] lives here, and a
    /// second fake backend would be the thing that drifts.
    async fn store_showing_one_discovery(
        cx: &mut gpui::TestAppContext,
        name: &'static str,
    ) -> (gpui::Entity<crate::AdeWorkspaceStore>, Arc<FakeBackend>) {
        let registry = AdeWorkspaceRegistry::open_test_db(name).await;
        let backend = Arc::new(FakeBackend::new("local").holding(vec![BackendWorkspace {
            id: "ws-local-1".to_owned(),
            name: "spike".to_owned(),
            project_root: "/repos/zed".to_owned(),
            created_at: 1_700_000_000,
        }]));
        let service = Arc::new(WorkspaceLifecycleService::with_backend(
            registry,
            backend.clone(),
        ));
        let store = cx.update(|cx| {
            cx.set_global(crate::GlobalLifecycleService(service));
            crate::AdeWorkspaceStore::global(cx)
        });
        cx.run_until_parked();
        store.read_with(cx, |store, _| {
            assert_eq!(store.entries().len(), 1, "{:?}", store.entries());
        });
        (store, backend)
    }

    /// **A refresh that predates a removal must not redraw it.** A pass holds a
    /// whole listing per host, so one in flight when the workspace goes away is
    /// describing a world that no longer exists — and a workspace with no live
    /// session pushes no status event afterwards to correct the ghost.
    ///
    /// Stepped one task at a time rather than run to quiescence: the removal's
    /// own follow-up refresh clears the view at the end whatever happens in the
    /// middle, so it is the invariant *between* the landings that says whether
    /// the stale pass was rejected.
    #[gpui::test]
    async fn test_a_refresh_from_before_a_removal_cannot_redraw_it(cx: &mut gpui::TestAppContext) {
        let (store, backend) =
            store_showing_one_discovery(cx, "test_stale_refresh_before_removal").await;

        // The pass about to run lists the record; the removal lands while it is
        // in flight.
        backend.freeze_next_list();
        store.update(cx, |store, cx| store.refresh(cx));
        backend.forget_all();
        store.update(cx, |store, cx| {
            store.forget_workspace(None, "ws-local-1", cx)
        });

        while cx.executor().tick() {
            store.read_with(cx, |store, _| {
                assert!(
                    store.entries().is_empty(),
                    "a listing from before the removal redrew it: {:?}",
                    store.entries()
                );
            });
        }
    }

    /// A record the pass listed before a removal took the lock must not be
    /// republished as a discovery afterwards.
    ///
    /// The publication and the eviction now happen on the same side of the
    /// host's decision lock, so the interleaving that left a killed workspace
    /// in the last-successful snapshot — visible for as long as the next
    /// listing failed — no longer exists.
    #[gpui::test]
    async fn test_a_forget_cannot_be_undone_by_the_pass_it_raced(cx: &mut gpui::TestAppContext) {
        let registry = AdeWorkspaceRegistry::open_test_db("test_forget_vs_pass_publication").await;
        // Still held by the backend: the pass in flight is listing it, which is
        // exactly the snapshot the removal has to survive.
        let backend = Arc::new(FakeBackend::new("local").holding(vec![BackendWorkspace {
            id: "ws-local-1".to_owned(),
            name: "spike".to_owned(),
            project_root: "/repos/zed".to_owned(),
            created_at: 1_700_000_000,
        }]));
        let service = Arc::new(WorkspaceLifecycleService::with_backend(
            registry,
            backend.clone(),
        ));

        let gate = service.decision_lock(None);
        let held = gate.lock().await;
        let reconciling = cx.executor().spawn({
            let service = service.clone();
            async move { service.reconcile_all().await.expect("the pass succeeds") }
        });
        cx.run_until_parked();
        let forgetting = cx.executor().spawn({
            let service = service.clone();
            async move {
                service
                    .forget_workspace(None, "ws-local-1")
                    .await
                    .expect("the removal succeeds")
            }
        });
        cx.run_until_parked();

        drop(held);
        cx.run_until_parked();
        reconciling.await;
        forgetting.await;

        assert!(
            service
                .remembered_discoveries(&service.daemon_key(None))
                .is_empty(),
            "the pass republished the record the removal evicted: {:?}",
            service.remembered_discoveries(&service.daemon_key(None))
        );
    }

    /// **An own kill evicts the discovery**, without waiting for the daemon to
    /// echo it back: the push connection can drop before the broadcast, and the
    /// next listing can fail, which would leave the just-deleted workspace on
    /// screen as an `Unknown` discovery indefinitely.
    #[gpui::test]
    async fn test_an_own_kill_evicts_the_discovery_it_killed() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_own_kill_evicts_discovery").await;
        let backend = Arc::new(FakeBackend::new("local").holding(vec![BackendWorkspace {
            id: "ws-local-1".to_owned(),
            name: "spike".to_owned(),
            project_root: "/repos/zed".to_owned(),
            created_at: 1_700_000_000,
        }]));
        let service = WorkspaceLifecycleService::with_backend(registry, backend.clone());

        service.reconcile_all().await.unwrap();
        let row = service
            .confirm_discovered(None, "ws-local-1")
            .await
            .unwrap();
        assert_eq!(
            service
                .remembered_discoveries(&service.daemon_key(None))
                .len(),
            1
        );

        service.kill_workspace(&row.id).await.unwrap();
        assert!(
            service
                .remembered_discoveries(&service.daemon_key(None))
                .is_empty(),
            "the kill left its own record in the fallback snapshot"
        );

        // The broadcast never arrives and the next listing fails: the fallback
        // is all the pass has, and it must not name the killed workspace.
        backend.fail_next_list("ssh: broken pipe");
        let reconciled = service.reconcile_all().await.unwrap();
        assert!(
            reconciled
                .entries
                .iter()
                .all(|entry| entry.wire_id() != Some("ws-local-1")),
            "{:?}",
            reconciled.entries
        );
    }

    /// **Recency does not outrank existence.** A newer row whose record the
    /// host no longer has must not beat an older row the host still lists, so
    /// every candidate is filtered against the listing before recency picks.
    #[gpui::test]
    async fn test_a_newer_stale_row_does_not_beat_an_older_live_one() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_stale_row_loses_to_live").await;
        let backend = Arc::new(FakeBackend::new("local").holding(vec![BackendWorkspace {
            id: "live".to_owned(),
            name: "zed".to_owned(),
            project_root: "/repos/zed".to_owned(),
            created_at: 1_700_000_000,
        }]));
        let service = WorkspaceLifecycleService::with_backend(registry, backend.clone());

        let mut live = AdeWorkspace::new("live", "zed", "/repos/zed");
        live.terminal_session_id = Some("live".to_owned());
        let mut stale = AdeWorkspace::new("stale", "zed", "/repos/zed");
        stale.terminal_session_id = Some("gone".to_owned());
        stale.last_opened_at = live.last_opened_at + time::Duration::seconds(60);
        service
            .registry()
            .create_workspace(live.clone())
            .await
            .unwrap();
        service
            .registry()
            .create_workspace(stale.clone())
            .await
            .unwrap();

        let (chosen, created) = service
            .adopt_or_create_workspace(PathBuf::from("/repos/zed"), None, |host, root| {
                host.is_none() && root == Path::new("/repos/zed")
            })
            .await
            .unwrap();

        assert!(!created);
        assert_eq!(chosen.id, live.id, "the row the host still holds wins");
        // No inline delete: the stale row is the guarded sweep's to judge.
        assert_eq!(service.registry().list_workspaces().unwrap().len(), 2);
    }

    /// A confirmed row whose record the host replaced is not the reattachment:
    /// the listed record on the same root is, and it is confirmed rather than
    /// duplicated. With nothing listed at all, the create is what is left.
    #[gpui::test]
    async fn test_connect_takes_the_listed_record_over_a_row_the_host_forgot() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_connect_validates_its_row").await;
        let backend = Arc::new(FakeBackend::new("local").holding(vec![BackendWorkspace {
            id: "new".to_owned(),
            name: "zed".to_owned(),
            project_root: "/repos/zed".to_owned(),
            created_at: 1_700_000_000,
        }]));
        let service = WorkspaceLifecycleService::with_backend(registry, backend.clone());

        let mut stale = AdeWorkspace::new("stale", "zed", "/repos/zed");
        stale.terminal_session_id = Some("old".to_owned());
        service
            .registry()
            .create_workspace(stale.clone())
            .await
            .unwrap();

        let matches =
            |host: Option<&str>, root: &Path| host.is_none() && root == Path::new("/repos/zed");
        let (chosen, created) = service
            .adopt_or_create_workspace(PathBuf::from("/repos/zed"), None, matches)
            .await
            .unwrap();
        assert!(!created);
        assert_eq!(chosen.daemon_workspace_id(), Some("new"));
        assert!(
            !backend
                .calls()
                .iter()
                .any(|call| call.starts_with("create_workspace:")),
            "the host's own record was there to be confirmed: {:?}",
            backend.calls()
        );

        // An authoritative empty listing on a fresh root: nothing to adopt, so
        // the workspace is created.
        backend.forget_all();
        let (created_row, created) = service
            .adopt_or_create_workspace(PathBuf::from("/repos/other"), None, |host, root| {
                host.is_none() && root == Path::new("/repos/other")
            })
            .await
            .unwrap();
        assert!(created);
        assert_eq!(created_row.repository_path, Path::new("/repos/other"));
    }

    /// A listing that failed says nothing about absence, so the stored row
    /// wins — an offline connect still reattaches. With no row there is nothing
    /// to fall back on, and the listing's error is the answer.
    #[gpui::test]
    async fn test_a_failed_listing_leaves_the_stored_row_in_charge() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_failed_listing_keeps_row").await;
        let backend = Arc::new(FakeBackend::new("local"));
        let service = WorkspaceLifecycleService::with_backend(registry, backend.clone());

        let matches =
            |host: Option<&str>, root: &Path| host.is_none() && root == Path::new("/repos/zed");
        backend.fail_next_list("ssh: broken pipe");
        let error = service
            .adopt_or_create_workspace(PathBuf::from("/repos/zed"), None, matches)
            .await
            .expect_err("with no row there is nothing to answer with");
        assert!(format!("{error:#}").contains("broken pipe"), "{error:#}");
        assert!(
            !backend
                .calls()
                .iter()
                .any(|call| call.starts_with("create_workspace:")),
            "{:?}",
            backend.calls()
        );

        let mut row = AdeWorkspace::new("zed", "zed", "/repos/zed");
        row.terminal_session_id = Some("ws-local-1".to_owned());
        service
            .registry()
            .create_workspace(row.clone())
            .await
            .unwrap();
        backend.fail_next_list("ssh: broken pipe");
        let (chosen, created) = service
            .adopt_or_create_workspace(PathBuf::from("/repos/zed"), None, matches)
            .await
            .unwrap();
        assert!(!created);
        assert_eq!(chosen.id, row.id);
    }

    /// **An incompatibility is not an offline host.** The cached row answers
    /// for a daemon that could not be *reached*; a daemon that answered and
    /// cannot be spoken to has news the window must see (§6.1), so it
    /// propagates whether or not this project has a row.
    #[gpui::test]
    async fn test_an_incompatible_listing_never_answers_with_the_cached_row() {
        let registry =
            AdeWorkspaceRegistry::open_test_db("test_incompatible_listing_propagates").await;
        let backend = Arc::new(FakeBackend::new("local"));
        let service = WorkspaceLifecycleService::with_backend(registry, backend.clone());
        let matches =
            |host: Option<&str>, root: &Path| host.is_none() && root == Path::new("/repos/zed");

        let mut row = AdeWorkspace::new("zed", "zed", "/repos/zed");
        row.terminal_session_id = Some("ws-local-1".to_owned());
        service
            .registry()
            .create_workspace(row.clone())
            .await
            .unwrap();

        backend.refuse_next_list_as_incompatible();
        let error = service
            .adopt_or_create_workspace(PathBuf::from("/repos/zed"), None, matches)
            .await
            .expect_err("a row must not stand in for a daemon this client cannot talk to");
        assert_eq!(
            crate::daemon_backend::incompatible_daemon(&error),
            Some(Outdated::Client),
            "the typed refusal survives the wrapping: {error:#}"
        );
        assert!(
            !backend
                .calls()
                .iter()
                .any(|call| call.starts_with("create_workspace:")),
            "{:?}",
            backend.calls()
        );
    }

    /// A daemon record with no row addressing it is an orphan — and at
    /// generation 2 the create already spawned a login shell, so it is a live
    /// orphan. The registry failure is still the answer; the record is not left
    /// behind with it.
    #[gpui::test]
    async fn test_a_create_the_registry_refuses_is_killed_on_the_daemon() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_create_compensates").await;
        let backend = Arc::new(FakeBackend::new("local"));
        let service = WorkspaceLifecycleService::with_backend(registry, backend.clone());
        service.registry().break_for_test().await;

        let error = service
            .create_workspace_locked("zed", "zed", "/repos/zed", None, None)
            .await
            .expect_err("the registry cannot record anything");
        assert!(
            format!("{error:#}").contains("recording the new workspace"),
            "the registry's failure is what the caller gets: {error:#}"
        );
        assert_eq!(
            crate::daemon_backend::incompatible_daemon(&error),
            None,
            "an ordinary failure, so the window may still take a plain terminal: the \
             compensated shell is dead and nothing competes with it"
        );
        assert_eq!(
            backend.calls(),
            vec![
                "create_workspace:/repos/zed".to_owned(),
                "kill_workspace:ws-local-1".to_owned()
            ],
            "the record this create made is killed on the backend that made it"
        );
    }

    /// §8.5: a degraded listing succeeded but is not authoritative about
    /// absence. A matching row therefore wins even when omitted — and with no
    /// row, an omitted match is an error, never a licence to mint a duplicate
    /// of a workspace the daemon could not read.
    #[gpui::test]
    async fn test_a_degraded_listing_keeps_a_row_and_never_licenses_a_create() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_degraded_never_creates").await;
        let backend = Arc::new(FakeBackend::new("local").degraded());
        let service = WorkspaceLifecycleService::with_backend(registry, backend.clone());

        let matches =
            |host: Option<&str>, root: &Path| host.is_none() && root == Path::new("/repos/zed");
        service
            .adopt_or_create_workspace(PathBuf::from("/repos/zed"), None, matches)
            .await
            .expect_err("an incomplete ledger view may not mint a workspace");
        assert!(
            !backend
                .calls()
                .iter()
                .any(|call| call.starts_with("create_workspace:")),
            "{:?}",
            backend.calls()
        );
        assert!(service.registry().list_workspaces().unwrap().is_empty());

        let mut row = AdeWorkspace::new("zed", "zed", "/repos/zed");
        row.terminal_session_id = Some("ws-local-1".to_owned());
        service
            .registry()
            .create_workspace(row.clone())
            .await
            .unwrap();
        let (chosen, created) = service
            .adopt_or_create_workspace(PathBuf::from("/repos/zed"), None, matches)
            .await
            .unwrap();
        assert!(!created);
        assert_eq!(
            chosen.id, row.id,
            "the row survives an omission it cannot trust"
        );
    }

    /// A degraded listing renders **over** the last authoritative one rather
    /// than in place of it: what it omitted may still be alive, and blinking it
    /// off the sidebar is the bug. The fallback-only record reads `Unknown`,
    /// never `Dead` — the current session list says nothing about a workspace
    /// the current workspace list never named.
    #[gpui::test]
    async fn test_a_degraded_listing_renders_over_the_last_authoritative_one() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_degraded_union_rendering").await;
        let record = |id: &str, created_at: u64| BackendWorkspace {
            id: id.to_owned(),
            name: id.to_owned(),
            project_root: format!("/repos/{id}"),
            created_at,
        };
        let backend =
            Arc::new(FakeBackend::new("local").holding(vec![record("ws-1", 1), record("ws-2", 2)]));
        let service = WorkspaceLifecycleService::with_backend(registry, backend.clone());

        service.reconcile_all().await.unwrap();
        assert_eq!(
            service
                .remembered_discoveries(&service.daemon_key(None))
                .len(),
            2
        );

        backend.drop_record("ws-2");
        backend.go_degraded();
        let reconciled = service.reconcile_all().await.unwrap();

        let state = |wire_id: &str| {
            reconciled
                .entries
                .iter()
                .find(|entry| entry.wire_id() == Some(wire_id))
                .map(|entry| entry.state())
        };
        assert_eq!(reconciled.entries.len(), 2, "{:?}", reconciled.entries);
        assert_eq!(state("ws-1"), Some(SessionState::Dead));
        assert_eq!(
            state("ws-2"),
            Some(SessionState::Unknown),
            "a record this listing never named cannot be called dead by it"
        );
        assert_eq!(
            service
                .remembered_discoveries(&service.daemon_key(None))
                .len(),
            2,
            "a degraded listing may not become the fallback"
        );
    }

    /// Discovered entries follow the host's creation order, with the opaque
    /// wire id only as a tie-break: sorting by the id alone puts a newly
    /// created workspace at a position unrelated to its history.
    #[gpui::test]
    async fn test_discovered_entries_follow_daemon_creation_order() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_discovered_ordering").await;
        let backend = Arc::new(FakeBackend::new("local").holding(vec![
            BackendWorkspace {
                id: "zzz".to_owned(),
                name: "first".to_owned(),
                project_root: "/repos/first".to_owned(),
                created_at: 100,
            },
            BackendWorkspace {
                id: "aaa".to_owned(),
                name: "second".to_owned(),
                project_root: "/repos/second".to_owned(),
                created_at: 200,
            },
        ]));
        let service = WorkspaceLifecycleService::with_backend(registry, backend);

        let reconciled = service.reconcile_all().await.unwrap();
        assert_eq!(
            reconciled
                .entries
                .iter()
                .map(|entry| entry.wire_id())
                .collect::<Vec<_>>(),
            vec![Some("zzz"), Some("aaa")]
        );
    }

    /// A pass narrowed to one project answers about that project. Its host's
    /// other rows are reconciled along with it — they share a listing — but a
    /// caller asking about one project has every reason to mistake a sibling's
    /// row for its own.
    #[gpui::test]
    async fn test_a_project_pass_answers_only_about_that_project() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_project_pass_is_narrow").await;
        let record = |id: &str, root: &str| BackendWorkspace {
            id: id.to_owned(),
            name: id.to_owned(),
            project_root: root.to_owned(),
            created_at: 1_700_000_000,
        };
        let backend = Arc::new(FakeBackend::new("local").holding(vec![
            record("ws-zed", "/repos/zed"),
            record("ws-praxis", "/repos/praxis"),
        ]));
        let service = WorkspaceLifecycleService::with_backend(registry, backend);

        for (wire_id, project, root) in [
            ("ws-zed", "zed", "/repos/zed"),
            ("ws-praxis", "praxis", "/repos/praxis"),
        ] {
            let mut row = AdeWorkspace::new(project, project, root);
            row.terminal_session_id = Some(wire_id.to_owned());
            service.registry().create_workspace(row).await.unwrap();
        }

        let narrowed = service.reconcile_project("zed").await.unwrap();

        assert_eq!(narrowed.entries.len(), 1, "{:?}", narrowed.entries);
        assert_eq!(narrowed.entries[0].wire_id(), Some("ws-zed"));
    }

    /// The record every alias test is about.
    fn viral_studio() -> BackendWorkspace {
        BackendWorkspace {
            id: "ade-viral-studio-2de8b3".to_owned(),
            name: "ade-viral-studio-2de8b3".to_owned(),
            project_root: "/home/user/Code/viral-studio".to_owned(),
            created_at: 1_700_000_000,
        }
    }

    fn listings(backend: &FakeBackend) -> usize {
        backend
            .calls()
            .iter()
            .filter(|call| *call == "list_workspaces")
            .count()
    }

    /// **Two spellings of one host are one daemon.** The IP and the hostname
    /// both reach it, both have a backend, and the daemon says which it is — so
    /// the refresh asks it once, renders its workspace once, and leaves the row
    /// bound where the user opened it. Rebinding it to whichever alias's pass
    /// ran last is the flip-flop this replaced: two entries and two registry
    /// writes, every single refresh.
    #[gpui::test]
    async fn test_two_aliases_of_one_daemon_reconcile_once() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_two_aliases_one_pass").await;
        let local = Arc::new(FakeBackend::new("local"));
        let remote = Arc::new(FakeBackend::new("remote").holding(vec![viral_studio()]));
        let service = WorkspaceLifecycleService::with_backend(registry, local)
            .with_backend_for_host("100.78.83.67", remote.clone())
            .with_backend_for_host("fevm1.local", remote.clone());

        let original = service
            .persist_on_open(&viral_studio(), Some("100.78.83.67"))
            .await
            .unwrap();
        assert_eq!(
            original.daemon_id,
            SessionBackend::instance_id(&*remote),
            "the row records which daemon it was opened against"
        );

        for pass in 1..=2 {
            let before = listings(&remote);
            let reconciled = service.reconcile_all().await.unwrap();
            assert_eq!(
                listings(&remote) - before,
                1,
                "pass {pass} asked the daemon once, not once per spelling"
            );
            assert_eq!(
                reconciled.entries.len(),
                1,
                "one daemon workspace is one entry: {reconciled:?}"
            );
            let (row, _) = reconciled.entries[0].persisted().expect("the row");
            assert_eq!(row.id, original.id);
            assert_eq!(
                row.remote_host.as_deref(),
                Some("100.78.83.67"),
                "a live binding is never moved between two spellings of one daemon"
            );
        }
        // Both spellings are reported as the one daemon, which is what lets the
        // views group and select by it.
        assert_eq!(
            service
                .reconcile_all()
                .await
                .unwrap()
                .daemon_of(Some("fevm1.local")),
            service.daemon_key(Some("100.78.83.67"))
        );
    }

    /// **A retired spelling still migrates.** The row is bound to an address
    /// that no longer answers, so nothing about it names a daemon; the alias
    /// that does hold the record takes it over, and records which daemon that
    /// is.
    #[gpui::test]
    async fn test_a_row_on_a_dead_spelling_migrates_to_the_live_one() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_retired_spelling_migrates").await;
        let local = Arc::new(FakeBackend::new("local"));
        let retired = Arc::new(FakeBackend::failing(
            "100.78.83.67",
            "ssh: connect: no route to host",
        ));
        let live = Arc::new(FakeBackend::new("fevm1").holding(vec![viral_studio()]));
        let service = WorkspaceLifecycleService::with_backend(registry, local)
            .with_backend_for_host("100.78.83.67", retired)
            .with_backend_for_host("fevm1.local", live.clone());

        let original = service
            .persist_on_open(&viral_studio(), Some("100.78.83.67"))
            .await
            .unwrap();
        assert_eq!(original.daemon_id, None, "an unreachable host names none");

        let listed = service
            .ensure_host_workspaces(Some("fevm1.local"))
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, original.id);
        assert_eq!(listed[0].remote_host.as_deref(), Some("fevm1.local"));
        assert_eq!(listed[0].daemon_id, SessionBackend::instance_id(&*live));
    }

    /// A daemon too old to name itself is identified by the spelling it was
    /// reached through, exactly as before this existed — the alias rebind
    /// included.
    #[gpui::test]
    async fn test_an_unnamed_daemon_is_still_reached_by_spelling() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_unnamed_daemon_spelling").await;
        let local = Arc::new(FakeBackend::new("local").nameless());
        let remote = Arc::new(
            FakeBackend::new("remote")
                .holding(vec![viral_studio()])
                .nameless(),
        );
        let service = WorkspaceLifecycleService::with_backend(registry, local)
            .with_backend_for_host("100.78.83.67", remote.clone())
            .with_backend_for_host("fevm1.local", remote.clone());

        let original = service
            .persist_on_open(&viral_studio(), Some("100.78.83.67"))
            .await
            .unwrap();
        assert_eq!(original.daemon_id, None);

        let listed = service
            .ensure_host_workspaces(Some("fevm1.local"))
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, original.id);
        assert_eq!(
            listed[0].remote_host.as_deref(),
            Some("fevm1.local"),
            "with nothing to tell the two spellings apart, the pass rebinds as it always did"
        );
    }

    /// **One record, one row, whichever alias opens it.** Two windows on two
    /// spellings of one host used to take a lock each and mint a row each; they
    /// now take the daemon's lock, and the loser finds the winner's row.
    #[gpui::test]
    async fn test_two_aliases_opening_one_record_confirm_one_row(cx: &mut gpui::TestAppContext) {
        let registry = AdeWorkspaceRegistry::open_test_db("test_two_aliases_one_row").await;
        let local = Arc::new(FakeBackend::new("local"));
        let remote = Arc::new(FakeBackend::new("remote").holding(vec![viral_studio()]));
        let service = Arc::new(
            WorkspaceLifecycleService::with_backend(registry, local)
                .with_backend_for_host("100.78.83.67", remote.clone())
                .with_backend_for_host("fevm1.local", remote.clone()),
        );

        // One lock, taken through either spelling: this is the whole fix.
        let gate = service.decision_lock(Some("100.78.83.67"));
        let held = gate.lock().await;
        let open = |service: Arc<WorkspaceLifecycleService>, host: &'static str| async move {
            service
                .confirm_discovered(Some(host), &viral_studio().id)
                .await
                .expect("the open succeeds")
        };
        let first = cx.executor().spawn(open(service.clone(), "100.78.83.67"));
        let second = cx.executor().spawn(open(service.clone(), "fevm1.local"));

        cx.run_until_parked();
        assert!(
            service.registry().list_workspaces().unwrap().is_empty(),
            "both spellings wait on the daemon's decision lock"
        );

        drop(held);
        cx.run_until_parked();
        let (first, second) = (first.await, second.await);
        assert_eq!(first.id, second.id, "the loser found the winner's row");
        assert_eq!(service.registry().list_workspaces().unwrap().len(), 1);
    }

    /// A row quarantined under one alias is the same workspace when it is
    /// opened through another: promoting it is what keeps the uuid its windows
    /// and layouts refer to, and the branch nothing else can re-derive.
    #[gpui::test]
    async fn test_a_quarantined_row_promotes_through_another_alias() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_alias_aware_promotion").await;
        let local = Arc::new(FakeBackend::new("local"));
        let remote = Arc::new(FakeBackend::new("remote").holding(vec![viral_studio()]));
        let service = WorkspaceLifecycleService::with_backend(registry, local)
            .with_backend_for_host("100.78.83.67", remote.clone())
            .with_backend_for_host("fevm1.local", remote.clone());

        let mut quarantined = AdeWorkspace::new(
            "viral studio",
            "viral-studio",
            "/home/user/Code/viral-studio",
        );
        quarantined.remote_host = Some("100.78.83.67".to_owned());
        quarantined.terminal_session_id = Some(viral_studio().id);
        quarantined.branch = Some("feature/x".to_owned());
        service
            .registry()
            .create_unconfirmed_workspace(quarantined.clone())
            .await
            .unwrap();

        let promoted = service
            .persist_on_open(&viral_studio(), Some("fevm1.local"))
            .await
            .unwrap();
        assert_eq!(promoted.id, quarantined.id, "the row keeps its uuid");
        assert_eq!(promoted.branch.as_deref(), Some("feature/x"));
        assert_eq!(
            promoted.daemon_id,
            SessionBackend::instance_id(&*remote),
            "and now records the daemon both spellings reach"
        );
        assert_eq!(service.registry().list_workspaces().unwrap().len(), 1);
    }

    /// **Two daemons, one wire id, one root.** Nothing about that makes them
    /// one workspace: ids are minted per daemon, and two hosts checked out at
    /// the same path are two checkouts. Neither row may be validated,
    /// suppressed or rebound by the other host's listing.
    #[gpui::test]
    async fn test_one_wire_id_on_two_daemons_stays_two_workspaces() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_two_daemons_one_wire_id").await;
        let record = BackendWorkspace {
            id: "ade-main-2de8b3".to_owned(),
            name: "ade-main-2de8b3".to_owned(),
            project_root: "/home/user/zed".to_owned(),
            created_at: 1_700_000_000,
        };
        let local = Arc::new(FakeBackend::new("local"));
        let first = Arc::new(FakeBackend::new("first").holding(vec![record.clone()]));
        let second = Arc::new(FakeBackend::new("second").holding(vec![record.clone()]));
        let service = WorkspaceLifecycleService::with_backend(registry, local)
            .with_backend_for_host("first.example", first)
            .with_backend_for_host("second.example", second);

        let one = service
            .persist_on_open(&record, Some("first.example"))
            .await
            .unwrap();
        let other = service
            .persist_on_open(&record, Some("second.example"))
            .await
            .unwrap();
        assert_ne!(one.id, other.id);
        assert_ne!(one.daemon_id, other.daemon_id);

        let reconciled = service.reconcile_all().await.unwrap();
        assert_eq!(reconciled.entries.len(), 2, "{reconciled:?}");
        assert!(
            reconciled
                .entries
                .iter()
                .all(|entry| entry.persisted().is_some()),
            "each daemon's record is matched to its own row: {reconciled:?}"
        );
        let hosts: HashSet<Option<&str>> = reconciled
            .entries
            .iter()
            .map(WorkspaceEntry::remote_host)
            .collect();
        assert_eq!(hosts.len(), 2, "and neither was rebound onto the other");
    }

    /// The mixed half of the same rule: a row on one daemon must not suppress
    /// the *discovery* of another daemon's record that happens to share its
    /// wire id. Suppression is per daemon, never per wire id alone.
    #[gpui::test]
    async fn test_a_row_does_not_hide_another_daemons_record() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_row_hides_only_its_daemon").await;
        let record = BackendWorkspace {
            id: "ade-main-2de8b3".to_owned(),
            name: "ade-main-2de8b3".to_owned(),
            project_root: "/home/user/zed".to_owned(),
            created_at: 1_700_000_000,
        };
        let local = Arc::new(FakeBackend::new("local"));
        let first = Arc::new(FakeBackend::new("first").holding(vec![record.clone()]));
        let second = Arc::new(FakeBackend::new("second").holding(vec![record.clone()]));
        let service = WorkspaceLifecycleService::with_backend(registry, local)
            .with_backend_for_host("first.example", first)
            .with_backend_for_host("second.example", second);

        service
            .persist_on_open(&record, Some("first.example"))
            .await
            .unwrap();

        let reconciled = service.reconcile_all().await.unwrap();
        assert_eq!(reconciled.entries.len(), 2, "{reconciled:?}");
        let discovered: Vec<&WorkspaceEntry> = reconciled
            .entries
            .iter()
            .filter(|entry| entry.persisted().is_none())
            .collect();
        assert_eq!(discovered.len(), 1, "{reconciled:?}");
        assert_eq!(discovered[0].remote_host(), Some("second.example"));
    }

    /// A removal reaches the row whichever spelling it was announced under:
    /// the workspace is the daemon's, and so is its absence.
    #[gpui::test]
    async fn test_forgetting_through_one_alias_drops_the_row_on_the_other() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_forget_across_aliases").await;
        let local = Arc::new(FakeBackend::new("local"));
        let remote = Arc::new(FakeBackend::new("remote").holding(vec![viral_studio()]));
        let service = WorkspaceLifecycleService::with_backend(registry, local)
            .with_backend_for_host("100.78.83.67", remote.clone())
            .with_backend_for_host("fevm1.local", remote.clone());
        service
            .persist_on_open(&viral_studio(), Some("100.78.83.67"))
            .await
            .unwrap();

        service
            .forget_workspace(Some("fevm1.local"), &viral_studio().id)
            .await
            .unwrap();

        assert!(service.registry().list_workspaces().unwrap().is_empty());
        assert!(
            service
                .remembered_discoveries(&service.daemon_key(Some("100.78.83.67")))
                .is_empty(),
            "and the discovery goes with it, under either spelling"
        );
    }

    /// Wire ids are only unique within a host, so the same one on two hosts is
    /// two workspaces — and neither the rows nor the discoveries may merge them.
    #[gpui::test]
    async fn test_colliding_workspace_ids_stay_separate_per_host() {
        let registry =
            AdeWorkspaceRegistry::open_test_db("test_ensure_host_keeps_id_collisions_separate")
                .await;
        let local = Arc::new(FakeBackend::new("local"));
        let record = |root: &str, created_at: u64| BackendWorkspace {
            id: "ade-main-2de8b3".to_owned(),
            name: "ade-main-2de8b3".to_owned(),
            project_root: root.to_owned(),
            created_at,
        };
        let first =
            Arc::new(FakeBackend::new("first").holding(vec![record("/home/user/first", 1)]));
        let second =
            Arc::new(FakeBackend::new("second").holding(vec![record("/home/user/second", 2)]));
        let service = WorkspaceLifecycleService::with_backend(registry, local)
            .with_backend_for_host("first.example", first)
            .with_backend_for_host("second.example", second);

        service
            .persist_on_open(&record("/home/user/first", 1), Some("first.example"))
            .await
            .unwrap();
        service
            .persist_on_open(&record("/home/user/second", 2), Some("second.example"))
            .await
            .unwrap();

        let listed = service
            .ensure_host_workspaces(Some("second.example"))
            .await
            .unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().any(|workspace| {
            workspace.remote_host.as_deref() == Some("first.example")
                && workspace.repository_path == Path::new("/home/user/first")
        }));
        assert!(listed.iter().any(|workspace| {
            workspace.remote_host.as_deref() == Some("second.example")
                && workspace.repository_path == Path::new("/home/user/second")
        }));

        // And the merged view keeps them apart too: each host's record is its
        // own entry, matched to its own row.
        let reconciled = service.reconcile_all().await.unwrap();
        assert_eq!(reconciled.entries.len(), 2);
        assert!(
            reconciled
                .entries
                .iter()
                .all(|entry| entry.persisted().is_some())
        );
    }

    /// A workspace the daemon holds and this client has never used shows up
    /// beside its rows — as a discovery with its session state, not as a row.
    /// The rows lead, in recency order, and the discoveries follow.
    #[gpui::test]
    async fn test_reconcile_reports_daemon_only_workspaces_as_discoveries() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_reconcile_adopts").await;
        let local = Arc::new(FakeBackend::new("local"));
        let service = WorkspaceLifecycleService::with_backend(registry, local.clone());

        let known = service
            .create_workspace("known", "project-a", "/repos/zed", None, None)
            .await
            .unwrap();
        local.hold(BackendWorkspace {
            id: "ade-testproj-2de8b3".to_owned(),
            name: "ade-testproj-2de8b3".to_owned(),
            project_root: "/home/kingii/testproj".to_owned(),
            created_at: 1_700_000_000,
        });

        let reconciled = service.reconcile_all().await.unwrap();
        assert!(
            reconciled.host_errors.is_empty(),
            "{:?}",
            reconciled.host_errors
        );
        assert_eq!(reconciled.entries.len(), 2);
        assert_eq!(
            reconciled.entries[0].persisted().map(|(row, _)| &row.id),
            Some(&known.id),
            "the row this client uses leads"
        );

        let WorkspaceEntry::Discovered {
            remote_host,
            workspace,
            state,
        } = &reconciled.entries[1]
        else {
            panic!("the daemon's workspace is a discovery: {:?}", reconciled);
        };
        assert!(remote_host.is_none());
        assert_eq!(workspace.id, "ade-testproj-2de8b3");
        // Probed in the same pass: the fake holds no session under that id, so
        // it reads as the disconnected workspace it is.
        assert_eq!(*state, SessionState::Dead);
        // And no row was written for it, however many passes run.
        service.reconcile_all().await.unwrap();
        assert_eq!(service.registry().list_workspaces().unwrap().len(), 1);
    }

    /// **A failed listing is not a disconnect.** The daemon transport is reset
    /// and retried by the next request, so one failure must leave the host's
    /// discoveries on screen as `Unknown` rather than blinking them away — and
    /// a later success replaces the snapshot.
    #[gpui::test]
    async fn test_a_transient_failure_keeps_the_hosts_discoveries() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_transient_failure_keeps").await;
        let local = Arc::new(FakeBackend::new("local"));
        let host = Arc::new(FakeBackend::new("dev-box").holding(vec![BackendWorkspace {
            id: "ws-1".to_owned(),
            name: "zed".to_owned(),
            project_root: "/srv/zed".to_owned(),
            created_at: 1_700_000_000,
        }]));
        let service = WorkspaceLifecycleService::with_backend(registry, local)
            .with_backend_for_host("dev-box", host.clone());

        let first = service.reconcile_all().await.unwrap();
        assert_eq!(first.entries.len(), 1);
        assert_eq!(first.entries[0].state(), SessionState::Dead);

        host.fail_next_list("ssh: connect: no route to host");
        let degraded = service.reconcile_all().await.unwrap();
        assert_eq!(degraded.entries.len(), 1, "the discovery is still shown");
        assert_eq!(degraded.entries[0].wire_id(), Some("ws-1"));
        assert_eq!(degraded.entries[0].state(), SessionState::Unknown);
        assert_eq!(degraded.host_errors.len(), 1);
        assert_eq!(degraded.host_errors[0].0, "dev-box");

        // A successful listing is the only thing that replaces the snapshot.
        host.forget_all();
        let recovered = service.reconcile_all().await.unwrap();
        assert!(recovered.entries.is_empty(), "{:?}", recovered.entries);
    }

    /// A rename mirrored, a duplicate collapsed and a stale row swept are all
    /// visible in the refresh that did them: the result is built from the rows
    /// that survived the pass, not the ones it started with.
    #[gpui::test]
    async fn test_a_pass_reports_the_rows_it_left_behind() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_pass_reports_survivors").await;
        let backend = Arc::new(FakeBackend::new("local").holding(vec![BackendWorkspace {
            id: "ws-local-1".to_owned(),
            name: "renamed elsewhere".to_owned(),
            project_root: "/repos/zed".to_owned(),
            created_at: 1_700_000_000,
        }]));
        let service = WorkspaceLifecycleService::with_backend(registry, backend.clone());

        let mut mine = AdeWorkspace::new("zed", "zed", "/repos/zed");
        mine.terminal_session_id = Some("ws-local-1".to_owned());
        mine.branch = Some("feature/x".to_owned());
        service
            .registry()
            .create_workspace(mine.clone())
            .await
            .unwrap();
        // A second row for the same record, and a row for a record the daemon
        // does not hold: one to collapse, one to sweep.
        let mut duplicate = AdeWorkspace::new("zed", "zed", "/repos/zed");
        duplicate.terminal_session_id = Some("ws-local-1".to_owned());
        duplicate.daemon_id = SessionBackend::instance_id(&*backend);
        duplicate.last_opened_at = mine.last_opened_at + time::Duration::seconds(60);
        service
            .registry()
            .create_workspace(duplicate.clone())
            .await
            .unwrap();
        let mut stale = AdeWorkspace::new("gone", "zed", "/repos/gone");
        stale.terminal_session_id = Some("ws-local-9".to_owned());
        service
            .registry()
            .create_workspace(stale.clone())
            .await
            .unwrap();

        let reconciled = service.reconcile_all().await.unwrap();

        let rows: Vec<&AdeWorkspace> = reconciled
            .entries
            .iter()
            .filter_map(|entry| entry.persisted().map(|(row, _)| row))
            .collect();
        assert_eq!(rows.len(), 1, "{reconciled:?}");
        assert_eq!(rows[0].id, mine.id, "the row with the branch survives");
        assert_eq!(
            rows[0].name, "renamed elsewhere",
            "and carries the name this same pass mirrored"
        );
    }

    /// The record whose name is its own id is the legacy one, and only that
    /// one. A *name* of the same shape belongs to whoever typed it.
    #[test]
    fn test_a_machine_named_record_is_recognised() {
        let record = |id: &str, name: &str| BackendWorkspace {
            id: id.to_owned(),
            name: name.to_owned(),
            project_root: "/repos/zed".to_owned(),
            created_at: 1_700_000_000,
        };

        let legacy = record("ade-testproj-2de8b3", "ade-testproj-2de8b3");
        assert!(is_machine_named(&legacy));
        assert_eq!(display_name_for(&legacy, "testproj"), "testproj");

        // The shape the old guard ate: a minted record with a uuid id, named
        // `ade-api-00ff11` by a person. It propagates like any other name.
        let looks_derived = record("3fa85f64-5717-4562", "ade-api-00ff11");
        assert!(!is_machine_named(&looks_derived));
        assert_eq!(
            display_name_for(&looks_derived, "testproj"),
            "ade-api-00ff11"
        );

        assert_eq!(
            display_name_for(&record("ws-1", "a name"), "testproj"),
            "a name"
        );
        // A record with no name at all leaves the row named for its checkout.
        assert_eq!(
            display_name_for(&record("ws-1", ""), "testproj"),
            "testproj"
        );
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

    /// A host that will not mint a record leaves **no row**: the id is the
    /// host's to give, and a row without one is a workspace only this machine
    /// believes in. There is no offline row creation.
    #[gpui::test]
    async fn test_a_host_that_refuses_leaves_no_row() {
        let service = backendless_service("test_a_host_that_refuses_leaves_no_row").await;

        let error = service
            .create_workspace("main", "project-a", "/repos/zed", None, None)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("no session backend in this test"));

        assert!(service.registry().list_workspaces().unwrap().is_empty());
    }

    /// Every entry point a remote workspace can reach lands on the backend for
    /// **its** host, and this machine's is never touched.
    ///
    /// The four refusal assertions this replaces (a remote workspace erroring
    /// out of probe / stop / kill / attach) are the same four paths, now
    /// asserted to route rather than to refuse.
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
        let session = workspace.daemon_workspace_id().unwrap().to_owned();
        assert_eq!(workspace.remote_host.as_deref(), Some("dev-box"));
        // The record exists; nothing runs in it yet.
        assert_eq!(workspace.status, WorkspaceStatus::Disconnected);
        // The listing is the retry check: a record this create already minted
        // is recovered rather than duplicated.
        assert_eq!(
            host.calls(),
            vec![
                "list_workspaces".to_owned(),
                "create_workspace:/srv/checkouts/zed".to_owned()
            ]
        );

        // Attach: the argv is the host backend's, not this machine's, and it is
        // what brings the first session into being.
        let argv = service.attach_command(&workspace).unwrap().argv;
        assert_eq!(argv, vec!["dev-box-attach".to_owned(), session.clone()]);

        // Probe, now that a session exists.
        let mut probed = workspace.clone();
        assert_eq!(
            service.probe(&mut probed).await.unwrap(),
            SessionState::Alive
        );

        // Stop detaches on the host; kill kills there.
        let stopped = service.stop_workspace(&workspace.id).await.unwrap();
        assert_eq!(stopped.status, WorkspaceStatus::Stopped);
        assert!(host.calls().contains(&format!("detach:{session}")));

        let killed = service.kill_workspace_session(&workspace.id).await.unwrap();
        assert_eq!(killed.status, WorkspaceStatus::Stopped);
        assert!(killed.terminal_session_id.is_none());
        assert!(host.calls().contains(&format!("kill:{session}")));

        assert!(
            local.calls().is_empty(),
            "this machine's backend was asked about a remote workspace: {:?}",
            local.calls()
        );
    }

    /// **The record comes first, and its id is the row's identity.**
    ///
    /// The bug this pins is the one the whole cutover removes: a row created
    /// locally under an id this client derived, which the daemon would only
    /// hear about when a session named it. The minted id is opaque, so a row
    /// carrying one cannot have derived it.
    #[gpui::test]
    async fn test_creating_a_row_mints_the_record_first() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_creating_a_row_mints_first").await;
        let backend = Arc::new(FakeBackend::new("local"));
        let service = WorkspaceLifecycleService::with_backend(registry, backend.clone());

        let workspace = service
            .create_workspace("Vector DB spike", "zed", "/repos/zed", None, None)
            .await
            .unwrap();

        // The host was asked for its ledger — the retry check — and then to
        // mint. No session, and nothing local.
        assert_eq!(
            backend.calls(),
            vec!["list_workspaces", "create_workspace:/repos/zed"]
        );
        assert_eq!(
            workspace.daemon_workspace_id(),
            Some("ws-local-1"),
            "the recorded id is the daemon's, not one this client derived"
        );
        // The record's name is the row's, so the mirror agrees with it.
        assert_eq!(
            backend.list_workspaces().unwrap().workspaces[0].name,
            "Vector DB spike"
        );

        // The row caches the record, with no session in it yet.
        let stored = service.registry().list_workspaces().unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].terminal_session_id.as_deref(), Some("ws-local-1"));
        assert_eq!(stored[0].status, WorkspaceStatus::Disconnected);
    }

    /// A row with zero sessions is a normal state, and its first terminal goes
    /// into the workspace the panel row already made — not a second one.
    #[gpui::test]
    async fn test_the_first_terminal_lands_in_the_row_that_was_created() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_first_terminal_lands_in_row").await;
        let backend = Arc::new(FakeBackend::new("local"));
        let service = WorkspaceLifecycleService::with_backend(registry, backend.clone());

        let mut workspace = service
            .create_workspace("zed", "zed", "/repos/zed", None, None)
            .await
            .unwrap();
        let id = workspace.daemon_workspace_id().unwrap().to_owned();

        // No sessions: the row exists on its own.
        assert!(backend.list().unwrap().is_empty());
        assert_eq!(
            service.probe(&mut workspace).await.unwrap(),
            SessionState::Dead
        );

        // The first terminal, which is attach-or-create against the same id.
        let attached = service.attach_command(&workspace).unwrap();
        assert_eq!(attached.session_id, id);
        assert_eq!(
            backend.list().unwrap(),
            vec![crate::SessionInfo {
                id: SessionId::from(id.clone())
            }]
        );

        // And the workspace is still the one the row was created as.
        let recorded = service
            .record_attached_session(&workspace.id)
            .await
            .unwrap();
        assert_eq!(recorded.daemon_workspace_id(), Some(id.as_str()));
        assert_eq!(recorded.status, WorkspaceStatus::Running);
        assert_eq!(service.registry().list_workspaces().unwrap().len(), 1);
    }

    /// A record with no name is not news: the row keeps the name it has rather
    /// than being blanked by a degenerate one.
    #[gpui::test]
    async fn test_an_empty_record_name_never_overwrites_a_row() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_empty_record_name").await;
        let backend = Arc::new(FakeBackend::new("local").holding(vec![BackendWorkspace {
            id: "ws-local-1".to_owned(),
            name: String::new(),
            project_root: "/repos/zed".to_owned(),
            created_at: 1_700_000_000,
        }]));
        let service = WorkspaceLifecycleService::with_backend(registry, backend.clone());

        let mut row = AdeWorkspace::new("Vector DB spike", "zed", "/repos/zed");
        row.terminal_session_id = Some("ws-local-1".to_owned());
        service.registry().create_workspace(row).await.unwrap();

        let listed = service.ensure_host_workspaces(None).await.unwrap();

        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "Vector DB spike");
    }

    /// **Rename propagates through the daemon.** One client renames; the other
    /// learns it from the list, on the row it already had.
    #[gpui::test]
    async fn test_a_rename_in_one_client_reaches_the_other() {
        let daemon = Arc::new(FakeBackend::new("host"));
        let client_a = WorkspaceLifecycleService::with_backend(
            AdeWorkspaceRegistry::open_test_db("test_rename_reaches_client_a").await,
            Arc::new(FakeBackend::new("a-local")),
        )
        .with_backend_for_host("host", daemon.clone());
        let client_b = WorkspaceLifecycleService::with_backend(
            AdeWorkspaceRegistry::open_test_db("test_rename_reaches_client_b").await,
            Arc::new(FakeBackend::new("b-local")),
        )
        .with_backend_for_host("host", daemon.clone());

        let workspace = client_a
            .create_workspace("zed", "zed", "/repos/zed", None, Some("host".into()))
            .await
            .unwrap();
        // B has seen the record and opened it, which is what gives it a row.
        client_b
            .persist_on_open(
                &daemon.list_workspaces().unwrap().workspaces[0],
                Some("host"),
            )
            .await
            .unwrap();
        let row_in_b = || {
            client_b
                .registry()
                .list_workspaces()
                .unwrap()
                .pop()
                .expect("B has the row")
        };
        assert_eq!(row_in_b().name, "zed");

        client_a
            .rename_workspace(&workspace.id, "Vector DB spike")
            .await
            .unwrap();

        // B's next listing brings the new name onto the row it already has,
        // rather than skipping it as known or adding a second one.
        client_b.ensure_host_workspaces(Some("host")).await.unwrap();
        assert_eq!(client_b.registry().list_workspaces().unwrap().len(), 1);
        assert_eq!(row_in_b().name, "Vector DB spike");
        assert_eq!(
            row_in_b().daemon_workspace_id(),
            workspace.daemon_workspace_id(),
            "the id never moves with the label"
        );
    }

    /// A host that cannot be reached fails alone: local rows still reconcile,
    /// the failure is named against its host, and nothing is claimed about that
    /// host's sessions.
    ///
    /// **Also pins reconcile's drop-guard (a)**: `there`'s row has no daemon
    /// record backing it in this test (the host never got a chance to say so),
    /// and a failed list must not read that as license to drop it — the
    /// `get_workspace` below panics if it did.
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
        // Its first terminal, so this row has a live session to reconcile.
        service.attach_command(&here).unwrap();
        // A row already cached for the host that is down: it cannot be created
        // through the service, because creating goes to the host first.
        let there = {
            let mut there = AdeWorkspace::new("there", "project-a", "/srv/zed");
            there.remote_host = Some("h1".into());
            there.terminal_session_id = Some("ws-h1-1".to_owned());
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
        // And each host was asked exactly once per question, not once per
        // workspace. The host that is down fails on the first of them and is
        // not asked the second.
        assert_eq!(
            local.calls(),
            vec![
                "list_workspaces".to_owned(),
                "create_workspace:/repos/zed".to_owned(),
                format!("attach:{}", here.daemon_workspace_id().unwrap()),
                "list_workspaces".to_owned(),
                "list".to_owned()
            ]
        );
        assert_eq!(down.calls(), vec!["list_workspaces".to_owned()]);
    }

    /// Reconcile (inc 7): a successful list drops every confirmed row this host
    /// holds no record for — a row still pointing at an id the daemon no longer
    /// has, and a row that was never linked at all — while a backed row
    /// survives.
    #[gpui::test]
    async fn test_reconcile_drops_rows_with_no_daemon_record() {
        let registry =
            AdeWorkspaceRegistry::open_test_db("test_reconcile_drops_unbacked_rows").await;
        let backend = Arc::new(FakeBackend::new("local").holding(vec![BackendWorkspace {
            id: "ade-zed-2de8b3".to_owned(),
            name: "zed".to_owned(),
            project_root: "/repos/zed".to_owned(),
            created_at: 1_700_000_000,
        }]));
        let service = WorkspaceLifecycleService::with_backend(registry, backend.clone());

        // The row for the record the daemon does hold: it survives.
        let backed = service
            .persist_on_open(&backend.list_workspaces().unwrap().workspaces[0], None)
            .await
            .unwrap();

        // Linked to an id the daemon no longer holds — the real-world shape:
        // a uuid-looking session id from a dead probe or a reset ledger.
        let mut stale = AdeWorkspace::new("adopted", "zed", "/repos/other");
        stale.terminal_session_id = Some("3fa85f64-5717-4562-b3fc-2c963f66afa6".to_owned());
        service
            .registry()
            .create_workspace(stale.clone())
            .await
            .unwrap();

        // Never linked at all: a row whose record was killed under it.
        let orphan = AdeWorkspace::new("orphan", "zed", "/repos/nowhere");
        service
            .registry()
            .create_workspace(orphan.clone())
            .await
            .unwrap();

        let listed = service.ensure_host_workspaces(None).await.unwrap();

        assert_eq!(listed.len(), 1, "only the backed row remains");
        assert_eq!(listed[0].id, backed.id);
        assert!(
            service
                .registry()
                .get_workspace(stale.id)
                .unwrap()
                .is_none(),
            "a row linked to an absent id is dropped"
        );
        assert!(
            service
                .registry()
                .get_workspace(orphan.id)
                .unwrap()
                .is_none(),
            "a row never linked to any record is dropped"
        );
    }

    /// Drop-guard (b): a host that answered its handshake `degraded` may not
    /// be listing everything it holds — its ledger is read-only, from a newer
    /// schema — so reconcile mirrors in what it did list and drops nothing on
    /// its silence, the same fence inc 6's silent-kill uses.
    #[gpui::test]
    async fn test_reconcile_leaves_a_degraded_host_alone() {
        let registry =
            AdeWorkspaceRegistry::open_test_db("test_reconcile_leaves_degraded_host_alone").await;
        let backend = Arc::new(
            FakeBackend::new("local")
                .holding(vec![BackendWorkspace {
                    id: "ade-zed-2de8b3".to_owned(),
                    name: "zed".to_owned(),
                    project_root: "/repos/zed".to_owned(),
                    created_at: 1_700_000_000,
                }])
                .degraded(),
        );
        let service = WorkspaceLifecycleService::with_backend(registry, backend.clone());

        let mut stale = AdeWorkspace::new("adopted", "zed", "/repos/other");
        stale.terminal_session_id = Some("3fa85f64-5717-4562-b3fc-2c963f66afa6".to_owned());
        service
            .registry()
            .create_workspace(stale.clone())
            .await
            .unwrap();
        let orphan = AdeWorkspace::new("orphan", "zed", "/repos/nowhere");
        service
            .registry()
            .create_workspace(orphan.clone())
            .await
            .unwrap();

        let listed = service.ensure_host_workspaces(None).await.unwrap();

        // Nothing dropped.
        assert_eq!(listed.len(), 2);
        assert!(
            service
                .registry()
                .get_workspace(stale.id)
                .unwrap()
                .is_some(),
            "a degraded host's silence must not read as license to drop"
        );
        assert!(
            service
                .registry()
                .get_workspace(orphan.id)
                .unwrap()
                .is_some()
        );
    }

    /// **A listing is a fact about one instant.** A row another client created
    /// while this pass's listing was in flight is not an unbacked row, and the
    /// confirming listing is what tells them apart.
    ///
    /// Before this, two windows connecting at once had one delete the other's
    /// fresh row; the workspace came back on the next pass under a new uuid,
    /// taking the branch, the selection and every window binding with it.
    #[gpui::test]
    async fn test_a_stale_listing_does_not_drop_a_row_the_host_holds() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_stale_listing_keeps_row").await;
        let backend = Arc::new(FakeBackend::new("local"));
        let service = WorkspaceLifecycleService::with_backend(registry, backend.clone());

        // The record and its row, as the client that created them left them.
        let created = service
            .create_workspace("main", "zed", "/repos/zed", None, None)
            .await
            .unwrap();
        // This pass's listing is a snapshot from before that create landed.
        backend.blind_next_list();

        let listed = service.ensure_host_workspaces(None).await.unwrap();

        assert_eq!(listed.len(), 1, "the row is not dropped on a stale listing");
        assert_eq!(listed[0].id, created.id, "and it keeps its uuid");
        assert_eq!(listed[0].terminal_session_id, created.terminal_session_id);
    }

    /// Drop-guard (b) reads the `degraded` of the **listing it is judging**,
    /// not of whatever daemon the backend is talking to by then: a reconnect
    /// mid-pass can replace a read-only daemon with a healthy one, and the
    /// healthy one's flag would license dropping rows the other never listed.
    #[gpui::test]
    async fn test_the_drop_gate_reads_the_listing_it_judges() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_drop_gate_reads_listing").await;
        let backend = Arc::new(FakeBackend::new("local").degraded_once());
        let service = WorkspaceLifecycleService::with_backend(registry, backend.clone());

        let mut stale = AdeWorkspace::new("adopted", "zed", "/repos/other");
        stale.terminal_session_id = Some("3fa85f64-5717-4562-b3fc-2c963f66afa6".to_owned());
        service
            .registry()
            .create_workspace(stale.clone())
            .await
            .unwrap();

        let listed = service.ensure_host_workspaces(None).await.unwrap();

        assert_eq!(listed.len(), 1);
        assert!(
            service
                .registry()
                .get_workspace(stale.id)
                .unwrap()
                .is_some(),
            "the degraded daemon's silence must not read as license to drop"
        );
    }

    /// **An empty registry is the case discovery exists for.** The startup pass
    /// used to derive its hosts from the rows alone, so a client with no rows
    /// never asked this machine's daemon anything and the panel stayed empty
    /// across restarts — with the daemon holding the workspaces all along.
    ///
    /// The narrowed pass keeps deriving them: it is asked about one project,
    /// and every host's discoveries would be everything but.
    #[gpui::test]
    async fn test_reconcile_all_discovers_from_this_machine_with_an_empty_registry() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_reconcile_all_empty_cache").await;
        let backend = Arc::new(FakeBackend::new("local").holding(vec![BackendWorkspace {
            id: "ws-local-1".to_owned(),
            name: "zed".to_owned(),
            project_root: "/repos/zed".to_owned(),
            created_at: 1_700_000_000,
        }]));
        let service = WorkspaceLifecycleService::with_backend(registry, backend.clone());

        let narrowed = service.reconcile_project("zed").await.unwrap();
        assert!(narrowed.entries.is_empty());
        assert!(
            backend.calls().is_empty(),
            "a project pass must not import this machine's other workspaces: {:?}",
            backend.calls()
        );

        let reconciled = service.reconcile_all().await.unwrap();
        assert!(
            reconciled.host_errors.is_empty(),
            "{:?}",
            reconciled.host_errors
        );
        assert_eq!(reconciled.entries.len(), 1);
        assert_eq!(reconciled.entries[0].wire_id(), Some("ws-local-1"));
        assert!(matches!(
            reconciled.entries[0],
            WorkspaceEntry::Discovered { .. }
        ));
    }

    /// **One record, one row.** Two clients mirroring the same record at once
    /// each mint their own uuid, and nothing else removes either: both rows are
    /// backed, and the sidebar shows a workspace twice with both copies acting
    /// on the same daemon workspace.
    ///
    /// The survivor is the row carrying metadata a mirror cannot re-derive, so
    /// every process collapses the pair the same way.
    #[gpui::test]
    async fn test_reconcile_collapses_two_rows_for_one_record() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_reconcile_collapses_rows").await;
        let backend = Arc::new(FakeBackend::new("local").holding(vec![BackendWorkspace {
            id: "ws-local-1".to_owned(),
            name: "zed".to_owned(),
            project_root: "/repos/zed".to_owned(),
            created_at: 1_700_000_000,
        }]));
        let service = WorkspaceLifecycleService::with_backend(registry, backend.clone());

        let mut mine = AdeWorkspace::new("zed", "zed", "/repos/zed");
        mine.terminal_session_id = Some("ws-local-1".to_owned());
        mine.branch = Some("feature/windows-holder".to_owned());
        service
            .registry()
            .create_workspace(mine.clone())
            .await
            .unwrap();

        // The other client's mirror: same record, fresh uuid, no branch — and
        // newer, so recency alone would pick the wrong one. Written by a build
        // that already records which daemon it reached, which is what lets two
        // rows for one record exist at all: sqlite's one-row-per-record index
        // sees two different keys and only this pass knows they are one
        // daemon.
        let mut theirs = AdeWorkspace::new("zed", "zed", "/repos/zed");
        theirs.terminal_session_id = Some("ws-local-1".to_owned());
        theirs.daemon_id = SessionBackend::instance_id(&*backend);
        theirs.last_opened_at = mine.last_opened_at + time::Duration::seconds(60);
        service
            .registry()
            .create_workspace(theirs.clone())
            .await
            .unwrap();

        let listed = service.ensure_host_workspaces(None).await.unwrap();

        assert_eq!(
            listed.len(),
            1,
            "one record cannot hold two rows: {listed:?}"
        );
        assert_eq!(listed[0].id, mine.id, "the row with the branch survives");
        assert_eq!(listed[0].branch.as_deref(), Some("feature/windows-holder"));
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

    /// **One dialog per down, however many watchers ask.** The stream going
    /// down is news once; the reconnect loop's repeats are not, and neither is
    /// a second view. Coming back and going down again is a new epoch, and news
    /// again.
    #[gpui::test]
    async fn test_one_down_epoch_is_reported_once() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_one_down_epoch").await;
        let service =
            WorkspaceLifecycleService::with_backend(registry, Arc::new(FakeBackend::new("local")));
        let down = |outdated| DaemonEvent::Down {
            message: "no protocol generation is common".to_owned(),
            outdated,
        };
        let host = Some("fevm1".to_owned());

        service.events.deliver(&host, down(Some(Outdated::Client)));
        assert_eq!(
            service.take_stream_incompatibility(),
            Some((host.clone(), Outdated::Client))
        );
        assert_eq!(
            service.take_stream_incompatibility(),
            None,
            "a second watcher gets nothing to show"
        );
        assert_eq!(
            service.stream_errors(),
            vec![(
                "fevm1".to_owned(),
                "status updates stopped: no protocol generation is common".to_owned()
            )],
            "the host error stands while the stream is down"
        );

        service.events.deliver(&host, DaemonEvent::Up);
        assert_eq!(service.stream_errors(), Vec::new());
        assert_eq!(service.take_stream_incompatibility(), None);

        service.events.deliver(&host, down(Some(Outdated::Client)));
        assert_eq!(
            service.take_stream_incompatibility(),
            Some((host, Outdated::Client)),
            "a second down is a second piece of news"
        );
    }

    /// An ordinary reachability failure is a host error and nothing else: no
    /// dialog, because there is nothing incompatible to tell the user about.
    #[gpui::test]
    async fn test_an_ordinary_stream_failure_opens_no_dialog() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_ordinary_stream_failure").await;
        let service =
            WorkspaceLifecycleService::with_backend(registry, Arc::new(FakeBackend::new("local")));

        service.events.deliver(
            &None,
            DaemonEvent::Down {
                message: "ssh: broken pipe".to_owned(),
                outdated: None,
            },
        );

        assert_eq!(service.take_stream_incompatibility(), None);
        assert_eq!(
            service.stream_errors(),
            vec![(
                host_label(None),
                "status updates stopped: ssh: broken pipe".to_owned()
            )]
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
            event: layout,
        } = smol::block_on(layouts.recv()).unwrap()
        else {
            panic!("a pushed layout must arrive as one");
        };
        assert_eq!(remote_host, None);
        assert_eq!(layout.workspace_id, "ade-here-000001");
        assert_eq!(layout.rev, 7);

        local.push_workspace_reset(LayoutEvent {
            workspace_id: "ade-here-000001".to_owned(),
            layout: LayoutDoc::empty(),
            rev: 1,
        });
        let WorkspaceEvent::Reset {
            remote_host,
            event: reset,
        } = smol::block_on(layouts.recv()).unwrap()
        else {
            panic!("an incarnation replacement must remain distinct from a kill");
        };
        assert_eq!(remote_host, None);
        assert_eq!(reset.workspace_id, "ade-here-000001");
        assert_eq!(reset.rev, 1);

        // A killed workspace rides the same stream, so a client cannot see a
        // layout for a workspace it has already been told is gone.
        local.push_workspace_removed("ade-here-000001");
        assert_eq!(
            smol::block_on(layouts.recv()).unwrap(),
            WorkspaceEvent::Removed {
                remote_host: None,
                workspace_id: "ade-here-000001".to_owned()
            }
        );
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
                    workspace_id: "same-id".to_owned(),
                },
                WorkspaceEvent::Removed {
                    remote_host: Some("h1".to_owned()),
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

    /// The workspace-level kill, and what it does to the registry.
    #[gpui::test]
    async fn test_killing_a_workspace_is_one_call_and_drops_its_row() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_killing_a_workspace_is_one").await;
        let backend = Arc::new(FakeBackend::new("local"));
        let service = WorkspaceLifecycleService::with_backend(registry, backend.clone());

        let workspace = service
            .create_workspace("main", "project-a", "/repos/zed", None, None)
            .await
            .unwrap();
        let session = workspace.daemon_workspace_id().unwrap().to_owned();
        // The first terminal, so there is something for a kill to take.
        service.attach_command(&workspace).unwrap();

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

        // The row goes with the record, in the same call: a workspace this
        // client killed is not a row for the next reconcile to sweep. The
        // answer still names what died, wire id and all.
        assert_eq!(killed.daemon_workspace_id(), Some(session.as_str()));
        assert_eq!(
            service
                .registry()
                .get_workspace(workspace.id.clone())
                .unwrap(),
            None
        );
    }

    /// **A kill that failed is a failure**, and the row keeps everything it
    /// needs to try again.
    ///
    /// The session-level fallback that used to run here fired on *any* error,
    /// so an unreachable host produced a success that killed nothing — and it
    /// cleared `terminal_session_id` on the way out, so the row could no longer
    /// name the workspace a retry would have to take.
    #[gpui::test]
    async fn test_a_failed_workspace_kill_is_reported_and_keeps_the_link() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_kill_reports_failure").await;
        let backend = Arc::new(FakeBackend::new("local").without_workspace_kill());
        let service = WorkspaceLifecycleService::with_backend(registry, backend.clone());

        let workspace = service
            .create_workspace("main", "project-a", "/repos/zed", None, None)
            .await
            .unwrap();
        let session = workspace.daemon_workspace_id().unwrap().to_owned();
        service.attach_command(&workspace).unwrap();

        let error = service
            .kill_workspace(&workspace.id)
            .await
            .expect_err("a backend that cannot kill the workspace must say so");
        assert!(
            format!("{error:#}").contains("no workspaces of its own to kill"),
            "{error:#}"
        );
        assert!(
            !backend.calls().contains(&format!("kill:{session}")),
            "no session-level kill stands in for the workspace kill: {:?}",
            backend.calls()
        );

        let stored = service
            .registry()
            .get_workspace(workspace.id.clone())
            .unwrap()
            .expect("the row survives a failed kill");
        assert_eq!(
            stored.terminal_session_id.as_deref(),
            Some(session.as_str())
        );
        assert_ne!(stored.status, WorkspaceStatus::Stopped);
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
            reconciled.entries.first().map(|entry| entry.state()),
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
        assert!(killed.terminal_session_id.is_none());
        let after = service.reconcile_all().await.expect("reconciling again");
        assert_eq!(
            after.entries.first().map(|entry| entry.state()),
            Some(SessionState::NeverCreated)
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
            reconciled.entries.first().map(|entry| entry.state()),
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
        assert!(killed.terminal_session_id.is_none());
        let after = service.reconcile_all().await.expect("reconciling again");
        assert_eq!(
            after.entries.first().map(|entry| entry.state()),
            Some(SessionState::NeverCreated)
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
        /// Every call fails with this, for the host that is down.
        failure: Option<String>,
        /// Whether this backend has workspaces of its own, i.e. whether it is
        /// daemon-shaped or tmux-shaped.
        workspace_kill: bool,
        sessions: Mutex<Vec<SessionId>>,
        /// What this backend holds that the registry may not know about, i.e.
        /// what adoption has to find.
        workspaces: Mutex<Vec<BackendWorkspace>>,
        calls: Mutex<Vec<String>>,
        status: Mutex<Option<Sender<DaemonEvent>>>,
        /// Whether this backend's host is behind the client, as a hash
        /// comparison would have found — and who asked to be told when that
        /// moves. The two halves of what the sidebar's upgrade arrow reads.
        stale: Mutex<bool>,
        /// The generation this host's daemon last handshook at, if it has.
        generation: Mutex<Option<u32>>,
        freshness_observers: Mutex<Vec<crate::DaemonFreshnessObserver>>,
        /// Whether this host's listings come back `degraded` — see
        /// [`crate::WorkspaceListing`].
        degraded: Mutex<bool>,
        /// Answer the next listing from a snapshot taken earlier: the pass
        /// whose listing predates whatever happened since.
        frozen_list: Mutex<Option<Vec<BackendWorkspace>>>,
        /// Answer the *next* listing as if it were a snapshot taken before
        /// another client's create landed: empty, whatever this backend holds.
        blind_list: Mutex<bool>,
        /// Report `degraded` on the first listing only — the daemon that
        /// answered was the read-only one, the connection has since been
        /// replaced by a healthy daemon's.
        degraded_once: Mutex<bool>,
        /// Fail the *next* listing, as a dropped transport does. The request
        /// after it reconnects and answers, which is why one failure is not a
        /// disconnect.
        list_failure: Mutex<Option<String>>,
        /// Fail the next listing the way an incompatible daemon does — a typed
        /// refusal, which is not a reachability failure.
        incompatible_list: Mutex<bool>,
        /// Which daemon this backend reaches — one per `FakeBackend`, so the
        /// same `Arc` registered under two host spellings is one daemon and
        /// two separate fakes are two. `None` is a daemon too old to say.
        instance_id: Option<String>,
    }

    /// Distinct per fake, like the uuid a real daemon mints.
    fn fake_instance_id() -> String {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        format!(
            "fake-daemon-{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )
    }

    impl FakeBackend {
        fn new(label: &str) -> Self {
            Self {
                label: label.to_owned(),
                failure: None,
                workspace_kill: true,
                sessions: Mutex::new(Vec::new()),
                workspaces: Mutex::new(Vec::new()),
                calls: Mutex::new(Vec::new()),
                status: Mutex::new(None),
                stale: Mutex::new(false),
                generation: Mutex::new(None),
                freshness_observers: Mutex::new(Vec::new()),
                degraded: Mutex::new(false),
                frozen_list: Mutex::new(None),
                blind_list: Mutex::new(false),
                degraded_once: Mutex::new(false),
                list_failure: Mutex::new(None),
                incompatible_list: Mutex::new(false),
                instance_id: Some(fake_instance_id()),
            }
        }

        /// A daemon that predates [`HelloAck::instance_id`], or one no request
        /// has reached: identity falls back to the host spelling.
        fn nameless(self) -> Self {
            Self {
                instance_id: None,
                ..self
            }
        }

        fn fail_next_list(&self, message: &str) {
            *self.list_failure.lock().unwrap() = Some(message.to_owned());
        }

        /// The next listing fails the way a daemon that cannot speak to this
        /// client does: typed, so the classification is structural.
        fn refuse_next_list_as_incompatible(&self) {
            *self.incompatible_list.lock().unwrap() = true;
        }

        /// Every workspace gone, as an authoritative empty listing reports.
        fn forget_all(&self) {
            self.workspaces.lock().unwrap().clear();
        }

        /// One workspace gone — the record a degraded ledger cannot read, or
        /// the one another client killed.
        fn drop_record(&self, id: &str) {
            self.workspaces.lock().unwrap().retain(|held| held.id != id);
        }

        /// The next listing answers from a snapshot taken **now**, whatever
        /// this backend holds by the time it is asked: the in-flight pass whose
        /// listing predates a removal.
        fn freeze_next_list(&self) {
            *self.frozen_list.lock().unwrap() = Some(self.workspaces.lock().unwrap().clone());
        }

        /// From here on this host's daemon answers `degraded`.
        fn go_degraded(&self) {
            *self.degraded.lock().unwrap() = true;
        }

        /// The next listing answers empty — a snapshot from before whatever
        /// this backend now holds was created.
        fn blind_next_list(&self) {
            *self.blind_list.lock().unwrap() = true;
        }

        /// The first listing is served by a degraded daemon, every later one by
        /// a healthy replacement.
        fn degraded_once(self) -> Self {
            *self.degraded_once.lock().unwrap() = true;
            self
        }

        /// A host already found to be running an older daemon than this client
        /// would deploy. Silent, because at construction nobody is listening.
        fn behind(self) -> Self {
            *self.stale.lock().unwrap() = true;
            self
        }

        fn at_generation(self, generation: u32) -> Self {
            *self.generation.lock().unwrap() = Some(generation);
            self
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

        fn hold(&self, workspace: BackendWorkspace) {
            self.workspaces.lock().unwrap().push(workspace);
        }

        fn failing(label: &str, message: &str) -> Self {
            Self {
                failure: Some(message.to_owned()),
                // A backend that cannot answer has never been told a daemon
                // id, which is what a retired host spelling looks like.
                instance_id: None,
                ..Self::new(label)
            }
        }

        /// tmux-shaped: sessions, but nothing a workspace kill could take.
        fn without_workspace_kill(self) -> Self {
            Self {
                workspace_kill: false,
                ..self
            }
        }

        /// This host's daemon answered `degraded`: its ledger is read-only, so
        /// reconcile must mirror what it lists and drop nothing.
        fn degraded(self) -> Self {
            self.go_degraded();
            self
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        /// Pushes an event to whoever subscribed, standing in for a daemon.
        fn push(&self, event: StatusEvent) {
            if let Some(sender) = self.status.lock().unwrap().as_ref() {
                smol::block_on(sender.send(DaemonEvent::Session(event)))
                    .expect("the merged stream is listening");
            }
        }

        /// The layout half of [`Self::push`], for the fanout's own test.
        fn push_layout(&self, event: LayoutEvent) {
            if let Some(sender) = self.status.lock().unwrap().as_ref() {
                smol::block_on(sender.send(DaemonEvent::Layout(event)))
                    .expect("the merged stream is listening");
            }
        }

        fn push_workspace_reset(&self, event: LayoutEvent) {
            if let Some(sender) = self.status.lock().unwrap().as_ref() {
                smol::block_on(sender.send(DaemonEvent::WorkspaceReset(event)))
                    .expect("the merged stream is listening");
            }
        }

        /// A workspace another client killed, as the daemon announces it.
        fn push_workspace_removed(&self, workspace_id: &str) {
            if let Some(sender) = self.status.lock().unwrap().as_ref() {
                smol::block_on(sender.send(DaemonEvent::WorkspaceRemoved {
                    workspace_id: workspace_id.to_owned(),
                }))
                .expect("the merged stream is listening");
            }
        }

        fn record(&self, call: impl Into<String>) -> Result<()> {
            self.calls.lock().unwrap().push(call.into());
            match &self.failure {
                Some(message) => bail!("{message}"),
                None => Ok(()),
            }
        }
    }

    impl SessionBackend for FakeBackend {
        fn instance_id(&self) -> Option<String> {
            self.instance_id.clone()
        }

        fn create(&self, spec: &SessionSpec) -> Result<SessionId> {
            self.record(format!("create:{}", spec.id))?;
            self.sessions.lock().unwrap().push(spec.id.clone());
            Ok(spec.id.clone())
        }

        fn list(&self) -> Result<Vec<crate::SessionInfo>> {
            self.record("list")?;
            Ok(self
                .sessions
                .lock()
                .unwrap()
                .iter()
                .map(|id| crate::SessionInfo { id: id.clone() })
                .collect())
        }

        fn list_workspaces(&self) -> Result<crate::WorkspaceListing> {
            self.record("list_workspaces")?;
            if let Some(message) = self.list_failure.lock().unwrap().take() {
                bail!("{message}");
            }
            if std::mem::take(&mut *self.incompatible_list.lock().unwrap()) {
                return Err(anyhow::Error::new(crate::DaemonRefusal {
                    code: ade_session::error_code::UNSUPPORTED_GENERATION.to_owned(),
                    message: "no protocol generation is common".to_owned(),
                })
                .context("the session daemon refused the handshake"));
            }
            let blind = std::mem::take(&mut *self.blind_list.lock().unwrap());
            let frozen = self.frozen_list.lock().unwrap().take();
            let degraded_once = std::mem::take(&mut *self.degraded_once.lock().unwrap());
            Ok(crate::WorkspaceListing {
                workspaces: match (blind, frozen) {
                    (true, _) => Vec::new(),
                    (false, Some(frozen)) => frozen,
                    (false, None) => self.workspaces.lock().unwrap().clone(),
                },
                degraded: *self.degraded.lock().unwrap() || degraded_once,
            })
        }

        /// Mints an id the way a daemon does — opaque, and nothing a client
        /// could have derived, so a test that finds a derived id in a row has
        /// found a bug rather than a coincidence.
        fn create_workspace(&self, root: &Path, name: Option<&str>) -> Result<BackendWorkspace> {
            self.record(format!("create_workspace:{}", root.display()))?;
            let mut workspaces = self.workspaces.lock().unwrap();
            let workspace = BackendWorkspace {
                id: format!("ws-{}-{}", self.label, workspaces.len() + 1),
                name: name
                    .map(str::to_owned)
                    .unwrap_or_else(|| project_id_from_path(root)),
                project_root: root.display().to_string(),
                created_at: 1_700_000_000,
            };
            workspaces.push(workspace.clone());
            Ok(workspace)
        }

        fn exists(&self, id: &SessionId) -> Result<bool> {
            self.record(format!("exists:{id}"))?;
            Ok(self.sessions.lock().unwrap().contains(id))
        }

        /// Attach-or-create, like the real seam: attaching to a workspace with
        /// no session is what opens its first terminal.
        fn attach(&self, spec: &SessionSpec) -> Result<Attached> {
            self.record(format!("attach:{}", spec.id))?;
            let mut sessions = self.sessions.lock().unwrap();
            if !sessions.contains(&spec.id) {
                sessions.push(spec.id.clone());
            }
            drop(sessions);
            Ok(Attached {
                session_id: spec.id.to_string(),
                argv: vec![format!("{}-attach", self.label), spec.id.to_string()],
            })
        }

        fn detach(&self, id: &SessionId) -> Result<()> {
            self.record(format!("detach:{id}"))
        }

        fn kill(&self, id: &SessionId) -> Result<()> {
            self.record(format!("kill:{id}"))?;
            self.sessions
                .lock()
                .unwrap()
                .retain(|session| session != id);
            Ok(())
        }

        fn reset_workspace_sessions(&self, id: &SessionId, directory: &Path) -> Result<()> {
            self.record(format!("reset:{id}:{}", directory.display()))?;
            self.kill(id)
        }

        fn rename_workspace(&self, workspace_id: &str, name: &str) -> Result<()> {
            self.record(format!("rename_workspace:{workspace_id}"))?;
            let mut workspaces = self.workspaces.lock().unwrap();
            let workspace = workspaces
                .iter_mut()
                .find(|workspace| workspace.id == workspace_id)
                .with_context(|| format!("no such workspace {workspace_id}"))?;
            workspace.name = name.to_owned();
            Ok(())
        }

        fn kill_workspace(&self, workspace_id: &str) -> Result<()> {
            self.record(format!("kill_workspace:{workspace_id}"))?;
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

        fn subscribe_events(&self) -> Result<Receiver<DaemonEvent>> {
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

        fn daemon_generation(&self) -> Option<u32> {
            *self.generation.lock().unwrap()
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
                project_root: "/home/kingii/proj".to_owned(),
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

    /// The compatibility arrow's fact, on the same terms as the stale-binary
    /// one: reported per host, unknown for a host nothing has reached, and
    /// never a reason to make a backend.
    #[gpui::test]
    async fn test_the_compat_arrow_reads_the_hosts_negotiated_generation() {
        let registry = AdeWorkspaceRegistry::open_test_db(
            "test_the_compat_arrow_reads_the_hosts_negotiated_generation",
        )
        .await;
        let service =
            WorkspaceLifecycleService::with_backend(registry, Arc::new(FakeBackend::new("local")))
                .with_backend_for_host(
                    "old-box",
                    Arc::new(FakeBackend::new("old-box").at_generation(2)),
                )
                .with_backend_for_host(
                    "new-box",
                    Arc::new(FakeBackend::new("new-box").at_generation(3)),
                );

        assert_eq!(service.host_daemon_generation("old-box"), Some(2));
        assert_eq!(service.host_daemon_generation("new-box"), Some(3));
        assert_eq!(service.host_daemon_generation("never-touched"), None);
        assert!(
            !service
                .backends
                .lock()
                .unwrap()
                .contains_key(&Some("never-touched".to_owned()))
        );
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

        // A row with a daemon record and nothing running in it: exactly what a
        // freshly created panel row is.
        let mut workspace = AdeWorkspace::new("main", "project-a", "/repos/zed");
        workspace.terminal_session_id = Some("ws-local-1".to_owned());
        workspace.status = WorkspaceStatus::Disconnected;
        service
            .registry()
            .create_workspace(workspace.clone())
            .await
            .unwrap();

        // The pane has already attached and succeeded, so this only writes
        // down what that produced — no backend is consulted, which is why the
        // failing backend never comes up.
        let recorded = service
            .record_attached_session(&workspace.id)
            .await
            .unwrap();
        assert_eq!(recorded.terminal_session_id.as_deref(), Some("ws-local-1"));
        assert_eq!(recorded.status, WorkspaceStatus::Running);

        // And it is the stored row that moved, not just the returned copy.
        let stored = service
            .registry()
            .get_workspace(workspace.id.clone())
            .unwrap()
            .unwrap();
        assert_eq!(stored.terminal_session_id.as_deref(), Some("ws-local-1"));
        assert_eq!(stored.status, WorkspaceStatus::Running);

        // Idempotent: a second attach to the same session changes nothing.
        let again = service
            .record_attached_session(&workspace.id)
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
    }
}
