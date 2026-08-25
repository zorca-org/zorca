//! The ADE workspace model: a durable "development room" — a repository
//! checkout (local or remote) plus the persistent terminal session attached to
//! it — that outlives any single Zed window.
//!
//! The source of truth for whether a session is actually alive is the session
//! backend ([`session_backend`] — the ADE session daemon); the registry in
//! [`registry`] is only a cache of the metadata
//! needed to find that session again. The registry never shells out —
//! [`lifecycle`] is the one layer that drives the backend, and the one that
//! keeps the cache honest.

mod attach;
mod connect;
mod create_workspace_modal;
mod daemon_backend;
mod layout;
mod lifecycle;
mod missing_tab;
mod registry;
mod rename_workspace_modal;
mod session_backend;
mod store;
mod terminal_pane;
mod workspace_sidebar;
mod workspace_view;

pub use attach::{
    can_reset_workspace_sessions, kill_and_recreate_workspace_sessions, kill_workspace_sessions,
    open_workspace_session,
};
pub use connect::{destination_for, open_connection_workspace};
pub use create_workspace_modal::{OnWorkspaceCreated, open_create_workspace_modal};
pub use daemon_backend::{DAEMON_BIN_ENV, DaemonBackend, DaemonUpgradeOutcome};
pub use layout::{
    AdeLayouts, Arrangement, Broadcast, LayoutSync, Leaf, MIN_SPLIT_RATIO, PUSH_DEBOUNCE,
    arrangement_from_layout, broadcast_action, capture_layout, layout_from_arrangement,
    render_layout, session_task_id, split_flexes,
};
pub use lifecycle::{Reconciled, SessionState, WorkspaceEntry, WorkspaceLifecycleService};
pub use missing_tab::MissingTab;
pub use registry::AdeWorkspaceRegistry;
pub use rename_workspace_modal::open_rename_workspace_modal;
pub use session_backend::{
    Attached, BackendWorkspace, DaemonEvent, DaemonFreshnessObserver, Identified,
    IdentifiedDaemonEvent, LayoutEvent, SessionBackend, SessionChange, SessionId, SessionInfo,
    SessionSpec, StatusDelivery, StatusEvent, WorkspaceEvent, WorkspaceLayout,
};
pub use store::AdeWorkspaceStore;
pub use terminal_pane::open_workspace_terminal;
pub use workspace_sidebar::{ToggleWorkspacesView, WorkspaceSidebar};

use anyhow::{Result, bail};
use db::sqlez::{
    bindable::{Bind, Column, StaticColumnCount},
    statement::Statement,
};
use gpui::{App, Global};
use std::{fmt, path::PathBuf, str::FromStr, sync::Arc};
use time::OffsetDateTime;

/// Registers the workspace sidebar's actions on every Zed workspace, and brings
/// up the shared [`AdeWorkspaceStore`].
///
/// The sidebar itself is nothing but a pane item opened on demand by
/// [`ToggleWorkspacesView`] — nothing here creates one. What this installs is
/// what has to exist before one can be created: that toggle, and the one
/// consumer of the daemon's status stream, so that a view which merely *reads*
/// workspaces (the ledger sidebar) never has to be the thing that starts it.
pub fn init(cx: &mut App) {
    // **Before `terminal_view::init`**, which `main` calls later: the two
    // register handlers for the same "new terminal" actions on every workspace,
    // and the first one registered is the one that answers. See [`layout::init`].
    layout::init(cx);
    workspace_sidebar::init(cx);
    workspace_view::init(cx);
    AdeWorkspaceStore::global(cx);
    cx.on_app_quit(|cx| {
        if let Some(service) = cx.try_global::<GlobalLifecycleService>() {
            service.0.disconnect();
        }
        async {}
    })
    .detach();
}

struct GlobalLifecycleService(Arc<WorkspaceLifecycleService>);

impl Global for GlobalLifecycleService {}

/// The one [`WorkspaceLifecycleService`] for the process, created on first use.
///
/// Shared rather than built per caller: the service holds one session backend
/// per host, and a backend owns that host's single ssh connection, so a second
/// service would mean a second connection to every host. Every entry point into
/// the lifecycle layer — the workspace sidebar, the ledger's "Add workspace" —
/// takes it from here.
pub fn lifecycle_service(cx: &mut App) -> Arc<WorkspaceLifecycleService> {
    if !cx.has_global::<GlobalLifecycleService>() {
        let service = Arc::new(WorkspaceLifecycleService::new(
            AdeWorkspaceRegistry::global(cx),
        ));
        cx.set_global(GlobalLifecycleService(service));
    }
    cx.global::<GlobalLifecycleService>().0.clone()
}

/// The service, but only if something has already asked for it.
///
/// For a **render**, which must not build the service as a side effect of
/// drawing: [`lifecycle_service`] opens the workspace registry's database on
/// the way, and a panel that has nothing ADE in it would pay for that just by
/// painting. A caller that finds `None` has its answer — nothing has talked to
/// any host yet, so nothing is known about any of them.
pub fn try_lifecycle_service(cx: &App) -> Option<Arc<WorkspaceLifecycleService>> {
    cx.try_global::<GlobalLifecycleService>()
        .map(|global| global.0.clone())
}

/// Prefix for every tmux session ADE owns. The product name is not settled
/// yet, so this is the single place the literal lives.
pub const SESSION_PREFIX: &str = "ade";

/// Stable, machine-independent workspace identity.
///
/// Deliberately a string (a UUID) rather than a sqlite `INTEGER PRIMARY KEY`:
/// a workspace has to be recognisable as the same workspace from another
/// client, whose local database would hand out different row ids.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkspaceId(String);

impl WorkspaceId {
    /// Mints a new random id.
    pub fn new() -> Self {
        Self(db::uuid::Uuid::new_v4().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for WorkspaceId {
    fn default() -> Self {
        Self::new()
    }
}

impl From<String> for WorkspaceId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

impl From<&str> for WorkspaceId {
    fn from(id: &str) -> Self {
        Self(id.to_owned())
    }
}

impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl StaticColumnCount for WorkspaceId {}

impl Bind for WorkspaceId {
    fn bind(&self, statement: &Statement, start_index: i32) -> Result<i32> {
        self.0.bind(statement, start_index)
    }
}

impl Column for WorkspaceId {
    fn column(statement: &mut Statement, start_index: i32) -> Result<(Self, i32)> {
        let (id, next_index) = String::column(statement, start_index)?;
        Ok((Self(id), next_index))
    }
}

/// Lifecycle state of a workspace, as last observed. Persisted as text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceStatus {
    Creating,
    Running,
    Disconnected,
    Stopped,
    Error,
}

impl WorkspaceStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Creating => "creating",
            Self::Running => "running",
            Self::Disconnected => "disconnected",
            Self::Stopped => "stopped",
            Self::Error => "error",
        }
    }
}

impl FromStr for WorkspaceStatus {
    type Err = anyhow::Error;

    fn from_str(status: &str) -> Result<Self> {
        Ok(match status {
            "creating" => Self::Creating,
            "running" => Self::Running,
            "disconnected" => Self::Disconnected,
            "stopped" => Self::Stopped,
            "error" => Self::Error,
            _ => bail!("unknown workspace status: {status}"),
        })
    }
}

impl fmt::Display for WorkspaceStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl StaticColumnCount for WorkspaceStatus {}

impl Bind for WorkspaceStatus {
    fn bind(&self, statement: &Statement, start_index: i32) -> Result<i32> {
        self.as_str().bind(statement, start_index)
    }
}

impl Column for WorkspaceStatus {
    fn column(statement: &mut Statement, start_index: i32) -> Result<(Self, i32)> {
        let (status, next_index) = String::column(statement, start_index)?;
        Ok((status.parse()?, next_index))
    }
}

/// A workspace's durable metadata.
#[derive(Clone, Debug, PartialEq)]
pub struct AdeWorkspace {
    pub id: WorkspaceId,
    pub name: String,
    /// Human-readable project group label.
    pub project_id: String,
    /// Canonical main-worktree paths identifying the project group.
    ///
    /// `None` is a row created before project identity was persisted. Those
    /// rows use their repository path until a live project backfills them.
    pub project_identity: Option<String>,
    pub repository_path: PathBuf,
    /// Monotonic revision of the project identity, label, and root.
    pub project_scope_rev: u64,
    /// `None` for a workspace not tied to a branch yet.
    pub branch: Option<String>,
    /// `None` means the workspace is local.
    pub remote_host: Option<String>,
    pub remote_workspace_path: Option<PathBuf>,
    /// The backend's session id — a tmux session name today — once a session
    /// has been created for this workspace. See [`SessionId`].
    pub terminal_session_id: Option<String>,
    /// The daemon's own identity, as of this row's last confirmation, rebind,
    /// or creation. `None` if the daemon reports no identity of its own.
    pub daemon_id: Option<String>,
    pub status: WorkspaceStatus,
    pub created_at: OffsetDateTime,
    pub last_opened_at: OffsetDateTime,
}

impl AdeWorkspace {
    /// A new local workspace with a fresh id, `Creating` status, and both
    /// timestamps set to now.
    pub fn new(
        name: impl Into<String>,
        project_id: impl Into<String>,
        repository_path: impl Into<PathBuf>,
    ) -> Self {
        let now = now_whole_seconds();
        Self {
            id: WorkspaceId::new(),
            name: name.into(),
            project_id: project_id.into(),
            project_identity: None,
            repository_path: repository_path.into(),
            project_scope_rev: 0,
            branch: None,
            remote_host: None,
            remote_workspace_path: None,
            terminal_session_id: None,
            daemon_id: None,
            status: WorkspaceStatus::Creating,
            created_at: now,
            last_opened_at: now,
        }
    }

    pub fn is_remote(&self) -> bool {
        self.remote_host.is_some()
    }

    pub fn project_identity(&self) -> String {
        self.project_identity
            .clone()
            .unwrap_or_else(|| self.repository_path.to_string_lossy().into_owned())
    }

    /// The tmux session name this workspace's terminal should use.
    pub fn tmux_session_name(&self) -> String {
        tmux_session_name(&self.name, &self.id)
    }

    /// The id the session daemon knows this workspace by.
    ///
    /// The *same string* as [`Self::tmux_session_name`] until a session has
    /// been created, and deliberately so: [`crate::DaemonBackend`] hands the
    /// seam's session id to the daemon as each session's `workspace_id`, so the
    /// daemon's workspace record is already keyed by it. Naming it separately is
    /// what lets the tmux-era name be deleted later without hunting for the
    /// callers that meant "workspace" rather than "tmux session".
    ///
    /// **Once a session has been recorded, that string *is* the identity** and
    /// the derived name stops being consulted. The derivation reads
    /// [`Self::name`], which the user can change (see
    /// [`crate::WorkspaceLifecycleService::rename_workspace`]) — deriving afresh
    /// after a rename would point at a workspace the daemon has never heard of,
    /// stranding the sessions, the layout and the scrollback under the old
    /// string. A rename moves a label; it must never move an identity.
    ///
    /// `terminal_session_id` is cleared only by a kill, which ends the daemon's
    /// record too — so falling back to the derived name there is right: there is
    /// nothing left to be identical to.
    pub fn daemon_workspace_id(&self) -> String {
        self.terminal_session_id
            .clone()
            .unwrap_or_else(|| self.tmux_session_name())
    }
}

/// `ade-<slug>-<id6>`: the slug is the lowercased name with runs of
/// non-alphanumerics collapsed to a single `-`, and `id6` is the first six
/// characters of the id — enough to disambiguate same-named workspaces while
/// keeping the session name typeable.
pub fn tmux_session_name(name: &str, id: &WorkspaceId) -> String {
    let mut slug = String::with_capacity(name.len());
    for character in name.chars() {
        if character.is_ascii_alphanumeric() {
            slug.extend(character.to_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');

    let id = id.as_str();
    let id6 = &id[..6.min(id.len())];

    format!("{SESSION_PREFIX}-{slug}-{id6}")
}

/// The project group a checkout belongs to: the last path component.
///
/// Deriving the group rather than asking for it is what makes several
/// workspaces on one repository fall under one heading without the user naming
/// it twice. A path with no final component (`/`, `..`, a bare root) has no
/// basename to use, so the whole path stands in — a visible, if ugly, group
/// beats an empty one.
pub fn project_id_from_path(repository_path: &std::path::Path) -> String {
    repository_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| repository_path.to_string_lossy().into_owned())
}

pub fn project_id_from_identity(project_identity: &str) -> String {
    let paths = project_identity
        .split('\n')
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return project_identity.to_owned();
    }
    project::ProjectGroupKey::new(None, util::path_list::PathList::new(&paths))
        .display_name(&Default::default())
        .to_string()
}

/// Timestamps are persisted as whole unix seconds, so truncate on the way in;
/// otherwise a workspace would not compare equal to its own round-trip.
pub(crate) fn now_whole_seconds() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(OffsetDateTime::now_utc().unix_timestamp())
        .expect("the current time is always a valid unix timestamp")
}

/// Column order shared by the registry's `SELECT`/`INSERT` statements and by
/// the [`Bind`]/[`Column`] impls below.
const COLUMN_COUNT: usize = 14;

impl StaticColumnCount for AdeWorkspace {
    fn column_count() -> usize {
        COLUMN_COUNT
    }
}

impl Bind for AdeWorkspace {
    fn bind(&self, statement: &Statement, start_index: i32) -> Result<i32> {
        let next_index = statement.bind(&self.id, start_index)?;
        let next_index = statement.bind(&self.name, next_index)?;
        let next_index = statement.bind(&self.project_id, next_index)?;
        let next_index = statement.bind(&self.project_identity, next_index)?;
        let next_index = statement.bind(&self.repository_path, next_index)?;
        let next_index = statement.bind(&self.project_scope_rev, next_index)?;
        let next_index = statement.bind(&self.branch, next_index)?;
        let next_index = statement.bind(&self.remote_host, next_index)?;
        let next_index = statement.bind(&self.remote_workspace_path, next_index)?;
        let next_index = statement.bind(&self.terminal_session_id, next_index)?;
        let next_index = statement.bind(&self.daemon_id, next_index)?;
        let next_index = statement.bind(&self.status, next_index)?;
        let next_index = statement.bind(&self.created_at.unix_timestamp(), next_index)?;
        statement.bind(&self.last_opened_at.unix_timestamp(), next_index)
    }
}

impl Column for AdeWorkspace {
    fn column(statement: &mut Statement, start_index: i32) -> Result<(Self, i32)> {
        let (id, next_index) = WorkspaceId::column(statement, start_index)?;
        let (name, next_index) = String::column(statement, next_index)?;
        let (project_id, next_index) = String::column(statement, next_index)?;
        let (project_identity, next_index) = Option::<String>::column(statement, next_index)?;
        let (repository_path, next_index) = PathBuf::column(statement, next_index)?;
        let (project_scope_rev, next_index) = u64::column(statement, next_index)?;
        let (branch, next_index) = Option::<String>::column(statement, next_index)?;
        let (remote_host, next_index) = Option::<String>::column(statement, next_index)?;
        let (remote_workspace_path, next_index) = Option::<PathBuf>::column(statement, next_index)?;
        let (terminal_session_id, next_index) = Option::<String>::column(statement, next_index)?;
        let (daemon_id, next_index) = Option::<String>::column(statement, next_index)?;
        let (status, next_index) = WorkspaceStatus::column(statement, next_index)?;
        let (created_at, next_index) = i64::column(statement, next_index)?;
        let (last_opened_at, next_index) = i64::column(statement, next_index)?;

        let workspace = Self {
            id,
            name,
            project_id,
            project_identity,
            repository_path,
            project_scope_rev,
            branch,
            remote_host,
            remote_workspace_path,
            terminal_session_id,
            daemon_id,
            status,
            created_at: OffsetDateTime::from_unix_timestamp(created_at)?,
            last_opened_at: OffsetDateTime::from_unix_timestamp(last_opened_at)?,
        };
        Ok((workspace, next_index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_string_round_trip() {
        let statuses = [
            WorkspaceStatus::Creating,
            WorkspaceStatus::Running,
            WorkspaceStatus::Disconnected,
            WorkspaceStatus::Stopped,
            WorkspaceStatus::Error,
        ];
        for status in statuses {
            assert_eq!(status.as_str().parse::<WorkspaceStatus>().unwrap(), status);
            assert_eq!(status.to_string(), status.as_str());
        }
        assert_eq!(WorkspaceStatus::Disconnected.as_str(), "disconnected");
        assert!("nonsense".parse::<WorkspaceStatus>().is_err());
    }

    #[test]
    fn test_tmux_session_name() {
        let id = WorkspaceId::from("0123456789abcdef");

        assert_eq!(tmux_session_name("main", &id), "ade-main-012345");
        assert_eq!(
            tmux_session_name("feature/auth", &id),
            "ade-feature-auth-012345"
        );
        // Runs of non-alphanumerics collapse, and edges are trimmed.
        assert_eq!(
            tmux_session_name("  Investigation: vector DB!  ", &id),
            "ade-investigation-vector-db-012345"
        );
        // Short ids are used whole rather than panicking on the slice.
        assert_eq!(
            tmux_session_name("main", &WorkspaceId::from("abc")),
            "ade-main-abc"
        );

        let workspace = AdeWorkspace::new("Feature/Auth", "project-a", "/repo");
        assert_eq!(
            workspace.tmux_session_name(),
            tmux_session_name(&workspace.name, &workspace.id)
        );
    }

    #[test]
    fn test_project_id_from_path() {
        use std::path::Path;

        assert_eq!(project_id_from_path(Path::new("/repos/zed")), "zed");
        // A trailing separator is not a component, so the group is unchanged.
        assert_eq!(project_id_from_path(Path::new("/repos/zed/")), "zed");
        assert_eq!(project_id_from_path(Path::new("zed")), "zed");
        // Nothing to take a basename from: the whole path stands in rather
        // than the group coming out empty.
        assert_eq!(project_id_from_path(Path::new("/")), "/");
        assert_eq!(project_id_from_path(Path::new("")), "");
        assert_eq!(project_id_from_path(Path::new("..")), "..");
    }

    #[test]
    fn test_project_id_from_canonical_identity() {
        assert_eq!(
            project_id_from_identity("/home/user/Code/viral-studio"),
            "viral-studio"
        );
        assert_eq!(
            project_id_from_identity("/repos/alpha\n/repos/beta"),
            "alpha, beta"
        );
    }

    #[test]
    fn test_new_workspace_defaults() {
        let workspace = AdeWorkspace::new("main", "project-a", "/repo");
        assert_eq!(workspace.status, WorkspaceStatus::Creating);
        assert!(!workspace.is_remote());
        assert_eq!(workspace.created_at, workspace.last_opened_at);
        assert!(workspace.terminal_session_id.is_none());
    }
}
