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
    WorkspaceStatus, now_whole_seconds, project_id_from_path,
};
use ade_session::LayoutDoc;
use anyhow::{Context as _, Result, bail};
use smol::{
    channel::{Receiver, Sender},
    lock::Mutex as AsyncMutex,
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

/// What one reconciliation pass found.
#[derive(Debug, Default)]
pub struct Reconciled {
    /// Every workspace that was reconciled, hosts that failed included.
    pub entries: Vec<(AdeWorkspace, SessionState)>,
    /// `(host, message)` for each host that could not be reached — the string
    /// is the host as the registry spells it, or `local` for this machine.
    /// Surfaced beside the rows, never in place of them.
    pub host_errors: Vec<(String, String)>,
}

/// How a host reads in an error line. The local backend has no name of its own.
fn host_label(host: Option<&str>) -> String {
    host.unwrap_or("local").to_owned()
}

/// The registry row for a workspace the backend holds and the registry has
/// never seen. See [`WorkspaceLifecycleService::adopt_workspaces`].
///
/// **`terminal_session_id` is the whole point.** It carries the backend's id,
/// which is what [`AdeWorkspace::daemon_workspace_id`] returns once recorded —
/// so this row addresses the workspace it was adopted from, rather than a
/// freshly derived name the daemon has never heard of. The `id` is minted here
/// because it is this client's own key and nothing else refers to it.
///
/// **Both timestamps are the backend's `created_at`**, not now. `last_opened_at`
/// is what a fresh connection reattaches by, and stamping an adopted row with
/// the current time would make a workspace nobody has opened in a month beat
/// the one the user was working in ten minutes ago.
///
/// `branch` is left unset: the backend records a root, not a checkout state, and
/// guessing it from the path would be a claim nothing verified.
fn adopted_row(
    workspace: &BackendWorkspace,
    host: Option<&str>,
    daemon_id: Option<&str>,
) -> AdeWorkspace {
    let repository_path = PathBuf::from(&workspace.project_root);
    let project_id = project_id_from_path(&repository_path);
    // Whole seconds, like everything else the registry stores; a backend that
    // reports a time no calendar has is given this client's clock rather than
    // failing the adoption over a timestamp.
    let created_at = i64::try_from(workspace.created_at)
        .ok()
        .and_then(|seconds| OffsetDateTime::from_unix_timestamp(seconds).ok())
        .unwrap_or_else(now_whole_seconds);
    AdeWorkspace {
        id: WorkspaceId::new(),
        name: display_name_for(&workspace.name, &project_id),
        project_id,
        repository_path,
        branch: None,
        remote_host: host.map(str::to_owned),
        remote_workspace_path: None,
        terminal_session_id: Some(workspace.id.clone()),
        daemon_id: daemon_id.map(str::to_owned),
        // Nothing has probed its sessions yet, and this is the status a
        // workspace nobody is attached to has. The probe that follows adoption
        // corrects it in the same pass.
        status: WorkspaceStatus::Disconnected,
        created_at,
        last_opened_at: created_at,
    }
}

/// The daemon an adoption decision is scoped to. Nameless daemons fall back to
/// the route used to reach them.
#[derive(Clone, PartialEq, Eq, Hash)]
enum DaemonKey {
    Instance(String),
    Host(Option<String>),
}

/// A persisted daemon identity is exclusive. Legacy rows retain the old
/// same-route or wire-id-plus-root matching.
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
        (Some(persisted), Some(current)) => return persisted == current,
        (Some(_), None) => return false,
        (None, _) => {}
    }
    if candidate.remote_host.as_deref() == host {
        return true;
    }
    candidate.remote_host.is_some()
        && candidate.repository_path == Path::new(&workspace.project_root)
}

/// What to call an adopted workspace: the checkout it is rooted at, unless the
/// backend's name is one a *person* chose.
///
/// A workspace created by this app is named for the machine — the daemon's
/// record is keyed by `ade-<slug>-<id6>` and the record's name follows it — and
/// showing that in the sidebar would be showing the user a session id where a
/// project name belongs. A name that does not fit the derived shape was typed by
/// somebody (see [`WorkspaceLifecycleService::rename_workspace`]) and must
/// survive adoption verbatim; losing a rename is worse than showing a slug.
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
    /// Serializes each daemon's adoption read and writes across aliases.
    daemon_decision_locks: Mutex<HashMap<DaemonKey, Arc<AsyncMutex<()>>>>,
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
    pub async fn create_workspace(
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

        self.registry
            .create_workspace(workspace.clone())
            .await
            .context("recording the new workspace")?;

        self.start_session(&mut workspace).await?;
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
                self.kill_and_delete_workspace(&duplicate)
                    .await
                    .with_context(|| format!("removing stale workspace {duplicate_daemon_id}"))?;
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

        let backend = self.backend_for(&workspace)?;

        // ponytail: this is the backend's existing list-and-kill operation;
        // add an atomic ResetWorkspace protocol frame if concurrent clients
        // creating sessions during recovery becomes a real problem.
        backend
            .reset_workspace_sessions(
                &SessionId::from(daemon_workspace_id.clone()),
                &workspace.repository_path,
                workspace.daemon_id.as_deref(),
            )
            .with_context(|| format!("killing sessions in workspace {daemon_workspace_id}"))?;
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
    /// adopting it again.
    pub async fn cleanup_dead_workspaces(&self, candidate_ids: Vec<WorkspaceId>) -> Result<usize> {
        let candidate_ids = candidate_ids.into_iter().collect::<HashSet<_>>();
        let reconciled = self.reconcile_all().await?;
        for (host, error) in reconciled.host_errors {
            log::warn!("skipping dead workspace cleanup on {host}: {error}");
        }

        let mut cleaned = 0;
        for (workspace, state) in reconciled.entries {
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
    pub async fn record_attached_session(&self, id: &WorkspaceId) -> Result<AdeWorkspace> {
        let mut workspace = self.get(id)?;

        // No backend is asked for: this is the registry writing down what the
        // pane already did, and it is as true of a remote host as a local one.
        let session = SessionId::from(workspace.daemon_workspace_id());
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
    /// An identityless legacy backend may have no workspace record of its own;
    /// its fallback is [`Self::kill_workspace_session`]. An identified daemon's
    /// refusal must propagate, because falling back could hide an identity
    /// mismatch while leaving the workspace record and layout behind.
    pub async fn kill_workspace(&self, id: &WorkspaceId) -> Result<AdeWorkspace> {
        let mut workspace = self.get(id)?;
        let backend = self.backend_for(&workspace)?;
        if let Err(error) = backend.kill_workspace(
            &workspace.daemon_workspace_id(),
            workspace.daemon_id.as_deref(),
        ) {
            if workspace.daemon_id.is_some() {
                return Err(error)
                    .with_context(|| format!("killing daemon workspace {}", workspace.id));
            }
            log::warn!(
                "no workspace-level kill for {}, killing its session instead: {error:#}",
                workspace.id
            );
            return self.kill_workspace_session(id).await;
        }
        self.record_killed(&mut workspace).await?;
        Ok(workspace)
    }

    /// Kills a daemon workspace record before deleting its registry row.
    ///
    /// This is deliberately stricter than [`Self::kill_workspace`]: its
    /// session-only fallback cannot prove that the daemon record and layout are
    /// gone. The row is only deleted after the record kill succeeds, so any
    /// unsupported operation or connectivity failure remains visible and
    /// retryable instead of leaving a record that reconciliation can re-adopt.
    async fn kill_and_delete_workspace(&self, workspace: &AdeWorkspace) -> Result<()> {
        let daemon_workspace_id = workspace.daemon_workspace_id();
        self.backend_for(workspace)?
            .kill_workspace(&daemon_workspace_id, workspace.daemon_id.as_deref())
            .with_context(|| format!("killing daemon workspace record {daemon_workspace_id}"))?;
        self.registry
            .delete_workspace(workspace.id.clone())
            .await
            .with_context(|| format!("deleting workspace {} from the registry", workspace.id))
    }

    /// Kills the workspace's session and everything running in it, then
    /// forgets the session name and records the workspace as `stopped`.
    ///
    /// **Destructive and irreversible.** Running agents die with the session
    /// and their scrollback goes with them. The workspace-level kill is
    /// [`Self::kill_workspace`], which this is the fallback for; every UI path
    /// that reaches either must say so in its own label ("Kill workspace", not
    /// "Close"): closing, switching away, and removing all detach instead. The
    /// session name is cleared because it no longer names anything — a killed
    /// workspace reads as never-created, not as disconnected, since there is
    /// nothing left to reconnect to.
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
                .exists(&session, workspace.daemon_id.as_deref())
                .with_context(|| format!("checking session {session} before killing it"))?
            {
                backend
                    .kill(&session, workspace.daemon_id.as_deref())
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

    /// The startup pass: probes every registered workspace and writes the
    /// results back, so the sidebar opens showing what is actually running
    /// rather than what was running when the app last closed.
    pub async fn reconcile_all(&self) -> Result<Reconciled> {
        let workspaces = self.registry.list_workspaces()?;
        self.reconcile(workspaces).await
    }

    /// [`Self::reconcile_all`], narrowed to one project.
    pub async fn reconcile_project(&self, project_id: impl Into<String>) -> Result<Reconciled> {
        let workspaces = self
            .registry
            .list_workspaces_for_project(project_id.into())?;
        self.reconcile(workspaces).await
    }

    /// Brings one host's session backend up, adopts every workspace it holds
    /// that the registry has never heard of, and answers with the registry as
    /// it then stands.
    ///
    /// **The connect decision's one call.** A window that has just connected to
    /// a host has to decide between reattaching to a workspace and making the
    /// first one, and until 2026-08-05 it decided that from
    /// `registry().list_workspaces()` alone — so a client with an empty
    /// registry created a second workspace on a host whose daemon already held
    /// one. The registry is a cache; asking it what a host has is asking the
    /// wrong party. Adoption is what makes the daemon's answer available to a
    /// client that has never seen it.
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
        self.adopt_workspaces(&backend, host).await
    }

    fn daemon_decision_lock(&self, key: DaemonKey) -> Arc<AsyncMutex<()>> {
        self.daemon_decision_locks
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .entry(key)
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    /// Writes a registry row for every workspace the backend holds that no row
    /// already points at, and answers with the registry as it then stands. A
    /// remote row reached through a different alias is rebound to the current
    /// destination.
    ///
    /// **Matched by `terminal_session_id`, which is the identity.**
    /// [`AdeWorkspace::daemon_workspace_id`] returns that column when it is
    /// recorded, so a row adopted under a daemon workspace's id addresses that
    /// workspace for every later open, attach, rename and kill. Matching on
    /// anything else — the path, the name — would adopt a second row for a
    /// workspace already present, and matching on it makes adopting twice a
    /// no-op.
    ///
    /// The first successful listing reveals the daemon identity. The registry
    /// is then reread under that daemon's lock before any adoption or rebind.
    ///
    /// **Adoption cannot resurrect a killed workspace.** The one workspace-level
    /// kill removes the daemon's record (`SessionTable::kill_workspace`,
    /// `crates/ade_session_daemon/src/sessions.rs`), so a killed workspace is
    /// not in this listing to be adopted from.
    async fn adopt_workspaces(
        &self,
        backend: &Arc<dyn SessionBackend>,
        host: Option<&str>,
    ) -> Result<Vec<AdeWorkspace>> {
        // Adoption must use the listing's control identity.
        let listing = backend
            .list_workspaces_identified()
            .with_context(|| format!("listing the workspaces on {}", host_label(host)))?;
        let held = listing.items;

        let instance_id = listing.daemon_id;
        let key = match instance_id.clone() {
            Some(instance_id) => DaemonKey::Instance(instance_id),
            None => DaemonKey::Host(host.map(str::to_owned)),
        };
        let lock = self.daemon_decision_lock(key);
        let _decision = lock.lock().await;

        // Another alias may have written while this caller was listing.
        let known = self.registry.list_workspaces()?;

        for workspace in held {
            // Prefer the row that already owns this identity over a matching
            // legacy row, or the update can collide with the unique index.
            let existing = known
                .iter()
                .filter(|known| {
                    is_the_same_daemons_row(known, instance_id.as_deref(), host, &workspace)
                })
                .max_by_key(|known| known.daemon_id.is_some());
            if let Some(existing) = existing {
                if existing.remote_host.as_deref() != host || existing.daemon_id != instance_id {
                    self.registry
                        .update_remote_host_and_daemon_id(
                            existing.id.clone(),
                            host.map(str::to_owned),
                            instance_id.clone(),
                        )
                        .await
                        .with_context(|| {
                            format!(
                                "rebinding workspace {} to {}",
                                workspace.id,
                                host_label(host)
                            )
                        })?;
                }
                continue;
            }

            let row = adopted_row(&workspace, host, instance_id.as_deref());
            self.registry
                .create_workspace(row)
                .await
                .with_context(|| format!("adopting workspace {}", workspace.id))?;
        }
        self.registry.list_workspaces()
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
    pub fn create_session_in_workspace(
        &self,
        workspace: &AdeWorkspace,
        working_directory: &Path,
    ) -> Result<(String, Vec<String>)> {
        let backend = self.backend_for(workspace)?;
        let spec = Self::session_spec(workspace);
        let session = backend
            .create_session_in_workspace(
                spec.id.as_str(),
                working_directory,
                workspace.daemon_id.as_deref(),
            )
            .with_context(|| format!("creating another session in {}", spec.id))?;
        let argv = backend.attach_session(&session, workspace.daemon_id.as_deref())?;
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
    ) -> Result<Vec<String>> {
        self.backend_for(workspace)?
            .attach_session(session_id, workspace.daemon_id.as_deref())
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
    /// **Every host contacted is also adopted from**, so a workspace the daemon
    /// holds and the registry has never seen becomes a row in this same pass
    /// rather than staying invisible until something else asks. Only hosts this
    /// pass was already going to contact: reconciliation must not turn a
    /// registry full of machines that are switched off into a wall of ssh
    /// attempts, and the connect path has
    /// [`Self::ensure_host_workspaces`] for the host it does care about.
    async fn reconcile(&self, mut workspaces: Vec<AdeWorkspace>) -> Result<Reconciled> {
        let mut hosts: Vec<Option<String>> = Vec::new();
        for workspace in &workspaces {
            if !hosts.contains(&workspace.remote_host) {
                hosts.push(workspace.remote_host.clone());
            }
        }

        // Registry-wide, not `workspaces`' own scope: `reconcile_project`
        // carries only one project's rows, and a different project's row
        // already in the registry before this pass must not be mistaken below
        // for one this pass just adopted.
        let known_before: HashSet<WorkspaceId> = self
            .registry
            .list_workspaces()?
            .into_iter()
            .map(|workspace| workspace.id)
            .collect();

        let mut host_errors = self.status_errors();
        // A row cannot be probed against another daemon's listing.
        let mut live: HashMap<Option<String>, (Option<String>, HashSet<SessionId>)> =
            HashMap::new();
        for host in hosts {
            let anything_to_probe = workspaces.iter().any(|workspace| {
                workspace.remote_host == host && workspace.terminal_session_id.is_some()
            });
            if !anything_to_probe {
                live.insert(host, (None, HashSet::new()));
                continue;
            }
            let backend = match self.backend_for_host(host.as_deref()) {
                Ok(backend) => backend,
                // Left out of `live` on purpose: no entry is what makes this
                // host's rows read as `Unknown` below.
                Err(error) => {
                    host_errors.push((host_label(host.as_deref()), format!("{error:#}")));
                    continue;
                }
            };
            // Before the session listing, so a workspace adopted here is probed
            // by the same pass that adopted it. A host that will not answer the
            // first question will not answer the second either, so it fails
            // once and its rows go untouched.
            match self.adopt_workspaces(&backend, host.as_deref()).await {
                Ok(snapshot) => {
                    // Rows already in scope pick up whatever adoption just
                    // wrote for them (a rebind's new `remote_host`/`daemon_id`,
                    // most notably).
                    for workspace in &mut workspaces {
                        if let Some(updated) = snapshot.iter().find(|row| row.id == workspace.id) {
                            *workspace = updated.clone();
                        }
                    }
                    // Everything else this host's pass put in the snapshot that
                    // was not already known anywhere in the registry is a fresh
                    // adoption, and only those join the working set.
                    //
                    // Excludes ids already in `workspaces` too: a second alias
                    // of the same daemon, reconciled later in this same pass,
                    // must not re-append the row the first alias just adopted
                    // or rebound above.
                    let already_in_scope: HashSet<WorkspaceId> = workspaces
                        .iter()
                        .map(|workspace| workspace.id.clone())
                        .collect();
                    workspaces.extend(snapshot.into_iter().filter(|row| {
                        row.remote_host == host
                            && !known_before.contains(&row.id)
                            && !already_in_scope.contains(&row.id)
                    }));
                }
                Err(error) => {
                    host_errors.push((host_label(host.as_deref()), format!("{error:#}")));
                    continue;
                }
            }
            match backend.list_identified().context("listing live sessions") {
                Ok(listing) => {
                    live.insert(
                        host,
                        (
                            listing.daemon_id,
                            listing
                                .items
                                .into_iter()
                                .map(|session| session.id)
                                .collect(),
                        ),
                    );
                }
                Err(error) => host_errors.push((host_label(host.as_deref()), format!("{error:#}"))),
            }
        }

        let mut entries = Vec::with_capacity(workspaces.len());
        for mut workspace in workspaces {
            let state = match workspace.terminal_session_id.clone().map(SessionId::from) {
                None => SessionState::NeverCreated,
                Some(session) => match live.get(&workspace.remote_host) {
                    // Another daemon's listing says nothing about this row.
                    Some((daemon_id, _))
                        if workspace
                            .daemon_id
                            .as_deref()
                            .is_some_and(|expected| daemon_id.as_deref() != Some(expected)) =>
                    {
                        SessionState::Unknown
                    }
                    Some((_, live)) => {
                        self.record_probe(&mut workspace, live.contains(&session))
                            .await?
                    }
                    None => SessionState::Unknown,
                },
            };
            entries.push((workspace, state));
        }
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
        SessionSpec::new(
            workspace.daemon_workspace_id(),
            workspace.repository_path.clone(),
        )
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
            DaemonEvent::WorkspaceReset(event) => send_or_clear(
                &self.layout,
                WorkspaceEvent::Reset {
                    remote_host: remote_host.clone(),
                    daemon_id,
                    event,
                },
            ),
            DaemonEvent::WorkspaceRemoved { workspace_id } => send_or_clear(
                &self.layout,
                WorkspaceEvent::Removed {
                    remote_host: remote_host.clone(),
                    daemon_id,
                    workspace_id,
                },
            ),
        }
        !self.is_idle()
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
    use crate::{LayoutEvent, SessionChange};
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

    /// The connect decision reads the **daemon**, not the registry: a client
    /// whose registry is empty still has to see what the host already holds, or
    /// it makes a second workspace beside the first (found 2026-08-05).
    #[gpui::test]
    async fn test_ensure_host_adopts_what_the_daemon_holds() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_ensure_host_adopts").await;
        let local = Arc::new(FakeBackend::new("local"));
        let host = Arc::new(
            FakeBackend::new("dev-box")
                .identified("daemon-dev-box")
                .holding(vec![
                    // Machine-named, so the row is named for its checkout instead.
                    BackendWorkspace {
                        id: "ade-testproj-2de8b3".to_owned(),
                        name: "ade-testproj-2de8b3".to_owned(),
                        project_root: "/home/user/testproj".to_owned(),
                        created_at: 1_700_000_000,
                    },
                    // Renamed by a person: adoption must not throw that away.
                    BackendWorkspace {
                        id: "ade-scratch-0f1e2d".to_owned(),
                        name: "Investigation: vector DB".to_owned(),
                        project_root: "/home/user/scratch".to_owned(),
                        created_at: 1_700_000_100,
                    },
                ]),
        );
        let service = WorkspaceLifecycleService::with_backend(registry, local.clone())
            .with_backend_for_host("dev-box", host.clone());

        let listed = service
            .ensure_host_workspaces(Some("dev-box"))
            .await
            .unwrap();
        assert_eq!(listed.len(), 2);

        let adopted = |session: &str| {
            listed
                .iter()
                .find(|workspace| workspace.terminal_session_id.as_deref() == Some(session))
                .cloned()
                .expect("the daemon's workspace became a row")
        };

        let testproj = adopted("ade-testproj-2de8b3");
        assert_eq!(testproj.name, "testproj");
        assert_eq!(testproj.project_id, "testproj");
        assert_eq!(
            testproj.repository_path,
            PathBuf::from("/home/user/testproj")
        );
        assert_eq!(testproj.remote_host.as_deref(), Some("dev-box"));
        assert!(testproj.branch.is_none());
        // The daemon's clock, not this client's: `last_opened_at` is what a
        // fresh connection reattaches by.
        assert_eq!(testproj.created_at.unix_timestamp(), 1_700_000_000);
        assert_eq!(testproj.last_opened_at, testproj.created_at);
        // The identity the daemon knows it by, so open/attach/rename address it.
        assert_eq!(testproj.daemon_workspace_id(), "ade-testproj-2de8b3");
        // And the daemon's own identity, so a second alias to it is recognised.
        assert_eq!(testproj.daemon_id.as_deref(), Some("daemon-dev-box"));

        let scratch = adopted("ade-scratch-0f1e2d");
        assert_eq!(scratch.name, "Investigation: vector DB");
        assert_eq!(scratch.project_id, "scratch");

        // Adopting twice is a no-op, not a second pair of rows.
        let again = service
            .ensure_host_workspaces(Some("dev-box"))
            .await
            .unwrap();
        assert_eq!(again.len(), 2);
        assert_eq!(service.registry().list_workspaces().unwrap().len(), 2);

        // And this machine's backend was never asked about another host's.
        assert!(local.calls().is_empty(), "{:?}", local.calls());
    }

    #[gpui::test]
    async fn test_ensure_host_rebinds_workspace_reached_through_an_alias() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_ensure_host_rebinds_alias").await;
        let local = Arc::new(FakeBackend::new("local"));
        let remote = Arc::new(FakeBackend::new("remote").holding(vec![BackendWorkspace {
            id: "ade-viral-studio-2de8b3".to_owned(),
            name: "ade-viral-studio-2de8b3".to_owned(),
            project_root: "/home/user/Code/viral-studio".to_owned(),
            created_at: 1_700_000_000,
        }]));
        let service = WorkspaceLifecycleService::with_backend(registry, local)
            .with_backend_for_host("100.78.83.67", remote.clone())
            .with_backend_for_host("fevm1.local", remote);

        let original = service
            .ensure_host_workspaces(Some("100.78.83.67"))
            .await
            .unwrap()
            .into_iter()
            .next()
            .expect("the IP address adopts the daemon workspace");

        let listed = service
            .ensure_host_workspaces(Some("fevm1.local"))
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, original.id);
        assert_eq!(listed[0].terminal_session_id, original.terminal_session_id);
        assert_eq!(listed[0].remote_host.as_deref(), Some("fevm1.local"));
    }

    #[gpui::test]
    async fn test_sequential_alias_of_an_identified_daemon_reuses_the_same_row() {
        let registry =
            AdeWorkspaceRegistry::open_test_db("test_sequential_identified_alias_reuses_row").await;
        let local = Arc::new(FakeBackend::new("local"));
        let remote = Arc::new(
            FakeBackend::new("remote")
                .identified("daemon-1")
                .holding(vec![BackendWorkspace {
                    id: "ade-viral-studio-2de8b3".to_owned(),
                    name: "ade-viral-studio-2de8b3".to_owned(),
                    project_root: "/home/user/Code/viral-studio".to_owned(),
                    created_at: 1_700_000_000,
                }]),
        );
        let service = WorkspaceLifecycleService::with_backend(registry, local)
            .with_backend_for_host("100.78.83.67", remote.clone())
            .with_backend_for_host("fevm1.local", remote);

        let original = service
            .ensure_host_workspaces(Some("100.78.83.67"))
            .await
            .unwrap()
            .into_iter()
            .next()
            .expect("the IP address adopts the daemon workspace");

        let listed = service
            .ensure_host_workspaces(Some("fevm1.local"))
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(
            listed[0].id, original.id,
            "the original WorkspaceId is reused, not duplicated"
        );
        assert_eq!(
            listed[0].terminal_session_id, original.terminal_session_id,
            "its session history moves with it"
        );
        assert_eq!(listed[0].remote_host.as_deref(), Some("fevm1.local"));
        assert_eq!(listed[0].daemon_id.as_deref(), Some("daemon-1"));
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
            .into_iter()
            .find(|(row, _)| row.id == owned.id)
            .expect("the row is reconciled, not dropped");
        assert_eq!(state, SessionState::Unknown);
        assert_eq!(
            row.status,
            WorkspaceStatus::Running,
            "a stranger's listing must not move the recorded status"
        );
    }

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

        let workspace = |root: &str| BackendWorkspace {
            id: "ade-main-2de8b3".to_owned(),
            name: "ade-main-2de8b3".to_owned(),
            project_root: root.to_owned(),
            created_at: 1_700_000_000,
        };
        let daemon_a = Arc::new(
            FakeBackend::new("daemon-a-box")
                .identified("daemon-a")
                .holding(vec![workspace("/home/user/main")]),
        );
        let daemon_b = Arc::new(
            FakeBackend::new("daemon-b-box")
                .identified("daemon-b")
                .holding(vec![workspace("/home/user/main")]),
        );
        let service = WorkspaceLifecycleService::with_backend(registry, local)
            .with_backend_for_host("a.example", daemon_a);

        let listed = service
            .ensure_host_workspaces(Some("a.example"))
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, cold_started.id);
        assert_eq!(listed[0].remote_host.as_deref(), Some("a.example"));

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
        assert_eq!(listed.len(), 2);
        let a_row = listed
            .iter()
            .find(|workspace| workspace.id == cold_started.id)
            .expect("daemon A's row survives untouched");
        assert_eq!(
            a_row.remote_host.as_deref(),
            Some("a.example"),
            "daemon B must not have reclaimed it"
        );
        assert_eq!(a_row.daemon_id.as_deref(), Some("daemon-a"));
        let b_row = listed
            .iter()
            .find(|workspace| workspace.id != cold_started.id)
            .expect("daemon B adopted a row of its own");
        assert_eq!(b_row.remote_host.as_deref(), Some("a.example"));
        assert_eq!(b_row.daemon_id.as_deref(), Some("daemon-b"));
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
        registry.create_workspace(exact.clone()).await.unwrap();

        let mut legacy = AdeWorkspace {
            terminal_session_id: Some(session_id.to_owned()),
            remote_host: Some("new-alias".to_owned()),
            ..AdeWorkspace::new("legacy", "project-a", "/home/user/main")
        };
        legacy.last_opened_at = OffsetDateTime::from_unix_timestamp(2_000).unwrap();
        registry.create_workspace(legacy.clone()).await.unwrap();

        let backend = Arc::new(
            FakeBackend::new("daemon-a")
                .identified("daemon-a")
                .holding(vec![BackendWorkspace {
                    id: session_id.to_owned(),
                    name: session_id.to_owned(),
                    project_root: "/home/user/main".to_owned(),
                    created_at: 1_700_000_000,
                }]),
        );
        let service =
            WorkspaceLifecycleService::with_backend(registry, Arc::new(FakeBackend::new("local")))
                .with_backend_for_host("new-alias", backend);

        let listed = service
            .ensure_host_workspaces(Some("new-alias"))
            .await
            .unwrap();
        let exact = listed
            .iter()
            .find(|workspace| workspace.id == exact.id)
            .expect("the identified row survives");
        assert_eq!(exact.remote_host.as_deref(), Some("new-alias"));
        assert_eq!(exact.daemon_id.as_deref(), Some("daemon-a"));
        let legacy = listed
            .iter()
            .find(|workspace| workspace.id == legacy.id)
            .expect("the legacy row is not rebound");
        assert!(legacy.daemon_id.is_none());
    }

    #[gpui::test]
    async fn test_concurrent_aliases_of_one_daemon_adopt_a_single_row(
        cx: &mut gpui::TestAppContext,
    ) {
        let registry = AdeWorkspaceRegistry::open_test_db("test_concurrent_aliases_one_row").await;
        let local = Arc::new(FakeBackend::new("local"));
        let remote = Arc::new(
            FakeBackend::new("remote")
                .identified("daemon-1")
                .holding(vec![BackendWorkspace {
                    id: "ade-viral-studio-2de8b3".to_owned(),
                    name: "ade-viral-studio-2de8b3".to_owned(),
                    project_root: "/home/user/Code/viral-studio".to_owned(),
                    created_at: 1_700_000_000,
                }]),
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
                service.ensure_host_workspaces(Some("100.78.83.67")).await
            })
        };
        let by_name = {
            let service = service.clone();
            cx.background_spawn(
                async move { service.ensure_host_workspaces(Some("fevm1.local")).await },
            )
        };
        cx.run_until_parked();

        let calls = remote.calls();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(
            calls.iter().all(|call| call == "list_workspaces"),
            "{calls:?}"
        );
        assert_eq!(service.registry().list_workspaces().unwrap().len(), 0);

        drop(guard);
        let (by_ip, by_name) = (by_ip.await.unwrap(), by_name.await.unwrap());

        assert_eq!(
            service.registry().list_workspaces().unwrap().len(),
            1,
            "one daemon must own exactly one row, however many aliases reached it at once"
        );
        assert_eq!(by_ip.len(), 1);
        assert_eq!(by_ip[0].id, by_name[0].id);
    }

    /// A workspace the daemon holds and the registry does not becomes a row in
    /// the same pass that probes it — the sidebar showed nothing for one on
    /// 2026-08-05 because reconciliation only ever probed rows it already had.
    #[gpui::test]
    async fn test_reconcile_adopts_daemon_only_workspaces() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_reconcile_adopts").await;
        let local = Arc::new(FakeBackend::new("local"));
        let service = WorkspaceLifecycleService::with_backend(registry, local.clone());

        // One row of our own, so the host is contacted at all.
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

        let (adopted, state) = reconciled
            .entries
            .iter()
            .find(|(workspace, _)| workspace.id != known.id)
            .expect("the daemon's workspace became a row");
        assert_eq!(
            adopted.terminal_session_id.as_deref(),
            Some("ade-testproj-2de8b3")
        );
        assert_eq!(adopted.name, "testproj");
        // Probed in the same pass: the fake holds no session under that id, so
        // the row reads as the disconnected one it is.
        assert_eq!(*state, SessionState::Dead);
        assert_eq!(adopted.status, WorkspaceStatus::Disconnected);

        // A second pass adopts nothing further.
        let again = service.reconcile_all().await.unwrap();
        assert_eq!(again.entries.len(), 2);
    }

    #[test]
    fn test_a_derived_workspace_name_is_recognised() {
        // What `tmux_session_name` produces, which is a session id in the place
        // a project name belongs.
        assert!(is_derived_workspace_name("ade-testproj-2de8b3"));
        assert!(is_derived_workspace_name("ade-feature-auth-012345"));
        // A name somebody typed, which adoption keeps verbatim.
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
        assert!(killed.terminal_session_id.is_none());
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
                .find(|(workspace, _)| &workspace.id == id)
                .map(|(_, state)| *state)
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
                format!("create:{}", here.tmux_session_name()),
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

    /// A backend with no workspace of its own — tmux — still has to be
    /// killable from the one control that says "Kill".
    #[gpui::test]
    async fn test_a_backend_without_workspaces_falls_back_to_killing_the_session() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_kill_falls_back").await;
        let backend = Arc::new(FakeBackend::new("local").without_workspace_kill());
        let service = WorkspaceLifecycleService::with_backend(registry, backend.clone());

        let workspace = service
            .create_workspace("main", "project-a", "/repos/zed", None, None)
            .await
            .unwrap();
        let session = workspace.tmux_session_name();

        let killed = service.kill_workspace(&workspace.id).await.unwrap();

        assert!(
            backend.calls().contains(&format!("kill:{session}")),
            "the fallback must take the session: {:?}",
            backend.calls()
        );
        assert_eq!(killed.status, WorkspaceStatus::Stopped);
        assert!(killed.terminal_session_id.is_none());
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
            reconciled.entries.first().map(|(_, state)| *state),
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
            after.entries.first().map(|(_, state)| *state),
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
            reconciled.entries.first().map(|(_, state)| *state),
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
            after.entries.first().map(|(_, state)| *state),
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
        sessions_after_next_list: Mutex<Vec<SessionId>>,
        /// What this backend holds that the registry may not know about, i.e.
        /// what adoption has to find.
        workspaces: Mutex<Vec<BackendWorkspace>>,
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
                failure: None,
                workspace_kill: true,
                sessions: Mutex::new(Vec::new()),
                sessions_after_next_list: Mutex::new(Vec::new()),
                workspaces: Mutex::new(Vec::new()),
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

        fn hold(&self, workspace: BackendWorkspace) {
            self.workspaces.lock().unwrap().push(workspace);
        }

        fn failing(label: &str, message: &str) -> Self {
            Self {
                failure: Some(message.to_owned()),
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

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
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
            match &self.failure {
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
            Ok(self.workspaces.lock().unwrap().clone())
        }

        fn exists(&self, id: &SessionId, expected: Option<&str>) -> Result<bool> {
            self.record(format!("exists:{id}{}", fence(expected)))?;
            Ok(self.sessions.lock().unwrap().contains(id))
        }

        fn attach(&self, spec: &SessionSpec, expected: Option<&str>) -> Result<Attached> {
            self.record(format!("attach:{}{}", spec.id, fence(expected)))?;
            Ok(Attached {
                session_id: spec.id.to_string(),
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
            .record_attached_session(&workspace.id)
            .await
            .unwrap();
        assert_eq!(
            recorded.terminal_session_id.as_deref(),
            Some(workspace.tmux_session_name().as_str())
        );
        assert_eq!(recorded.status, WorkspaceStatus::Running);

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
        assert!(service.recreate_session(&id).await.is_err());
    }
}
